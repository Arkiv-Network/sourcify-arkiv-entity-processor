use anyhow::{bail, ensure, Context, Result};
use arkiv_entity_processor::{bytes, entity_key, hx, sha, transform_line, Entity, Line};
use base64::{engine::general_purpose::STANDARD, Engine};
use clap::{Args, Parser, Subcommand};
use flate2::{write::GzEncoder, Compression};
use serde_json::{json, Value};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{self, File},
    io::{self, BufRead, BufReader, BufWriter, Read, Write},
    path::PathBuf,
    sync::{mpsc, Arc, Mutex},
    thread,
    time::Instant,
};

mod hydrate_cli;
mod pack;
mod s3;
mod upload;
mod verify;

const ABOUT: &str =
    "Turn Sourcify Ethereum contract records (NDJSON) into a deduplicated Arkiv prefill seed bundle";

const LONG_ABOUT: &str = "\
Turn Sourcify Ethereum contract records (NDJSON) into a deduplicated Arkiv prefill seed bundle.

Input: newline-delimited JSON, one verified Ethereum mainnet (chain 1) contract record per line
(bare, or wrapped as {\"record\": ...} or {\"contract\": ...}), optionally terminated by a source
completion marker {\"type\":\"source-complete\",\"records\":N}.

Output: a bundle directory with entities/*.jsonl (import-ready Arkiv prefill entities),
payloads.bin (deduplicated raw payloads), metadata.jsonl, report.json, SHA256SUMS and seed.json.
seed.json is published last, only after the whole input was processed (fail-closed).

`process`, `verify` and `hydrate` run offline: no database, RPC, Sourcify API or transactions. `pack` turns a
bundle into one deterministic .tar.zst archive and `upload` publishes it to S3-compatible storage
(Cloudflare R2) without ever overwriting an object.";

const PROCESS_LONG_ABOUT: &str = "\
Transform an NDJSON stream of Sourcify Ethereum contract records into an Arkiv prefill seed bundle.

Each non-blank input line is one contract record (bare object, or wrapped as {\"record\": ...} or
{\"contract\": ...}). A final line {\"type\":\"source-complete\",\"records\":N} (optionally with a
\"skipped\" list of {\"matchId\",\"reason\"} objects) declares the source complete. With
--require-source-complete the bundle is published only if that marker is present, its count
matches and no data follows it. Without it, EOF counts as complete: use that only for trusted,
finished files.

Records are transformed into sourcefile, code, compilation, verified_contract and blob entities,
deduplicated globally by semantic staging ID (payload bytes are stored once by SHA-256) and
written in input order, dependencies first.

Output directory (must not exist; runs are never resumed or overwritten):
  entities/NNNNNN.jsonl  import-ready prefill entities, --part-size per file
  payloads.bin           unique raw payload bytes, back to back
  metadata.jsonl         one row per entity: key, kind, attributes, payload offset/length/SHA-256
  report.json            counts, configuration, timings, per-part checksums
  SHA256SUMS             checksums of all of the above and seed.json
  seed.json              version-1 prefill manifest, published atomically last

On any error the partial directory is left for diagnosis without seed.json. Published files are
byte-identical for every --workers / --compression-workers setting; only report.json timing and
configuration fields differ.";

const PROCESS_EXAMPLES: &str = "\
Examples:
  # Piped source that writes a completion marker (recommended)
  source-adapter | arkiv-entity-processor process --require-source-complete \\
      --output bundle --owner 0x1111111111111111111111111111111111111111 \\
      --workers 4 --compression-workers 4

  # A finished, trusted NDJSON file (EOF counts as complete)
  arkiv-entity-processor process -i records.ndjson -o bundle \\
      --owner 0x1111111111111111111111111111111111111111

  # Check the result
  arkiv-entity-processor verify bundle";

#[derive(Parser, Debug)]
#[command(
    name = "arkiv-entity-processor",
    version,
    about = ABOUT,
    long_about = LONG_ABOUT,
    propagate_version = true,
    arg_required_else_help = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Transform NDJSON contract records into a seed bundle
    #[command(long_about = PROCESS_LONG_ABOUT, after_help = PROCESS_EXAMPLES)]
    Process(ProcessArgs),
    /// Reconstruct full contract NDJSON from stored wire entities
    #[command(long_about = hydrate_cli::LONG_ABOUT)]
    Hydrate(hydrate_cli::HydrateArgs),
    /// Verify a finished bundle: SHA256SUMS, report.json status and seed.json
    #[command(long_about = verify::LONG_ABOUT)]
    Verify(VerifyArgs),
    /// Pack a verified bundle into a deterministic .tar.zst archive and record
    #[command(long_about = pack::LONG_ABOUT, after_help = pack::EXAMPLES)]
    Pack(pack::PackArgs),
    /// Upload an archive to S3-compatible storage (R2) without overwriting, then verify it
    #[command(long_about = upload::LONG_ABOUT, after_help = upload::EXAMPLES)]
    Upload(upload::UploadArgs),
}

#[derive(Args, Debug)]
struct ProcessArgs {
    /// NDJSON input file, or `-` for stdin
    #[arg(short, long, value_name = "FILE", default_value = "-")]
    input: String,
    /// Output bundle directory (must not exist)
    ///
    /// The directory is created; an existing one is refused, never overwritten or resumed.
    #[arg(short, long, value_name = "DIR")]
    output: PathBuf,
    /// Owner/creator address of every entity
    ///
    /// Public seed metadata, not a private key or signing account. Must be a nonzero
    /// 0x-prefixed 20-byte hex address; it is written lowercase.
    #[arg(
        long,
        value_name = "ADDRESS",
        default_value = "0x0000000000000000000000000000000000000001"
    )]
    owner: String,
    /// Destination Arkiv chain ID written to seed.json
    ///
    /// The source chain is always Ethereum mainnet (1). Alias: --chain-id.
    #[arg(
        long = "destination-chain-id",
        alias = "chain-id",
        value_name = "ID",
        default_value_t = 7706815
    )]
    chain_id: u64,
    /// Entities per entities/NNNNNN.jsonl file
    ///
    /// Alias: --part-entities.
    #[arg(
        long = "part-size",
        alias = "part-entities",
        value_name = "N",
        default_value_t = 10000
    )]
    part_entities: usize,
    /// Publish only after a matching source-complete marker
    ///
    /// Fail without seed.json unless the input ends with {"type":"source-complete","records":N},
    /// N equals the number of records and no data follows. Recommended for piped input.
    #[arg(long)]
    require_source_complete: bool,
    /// Stop after N records (trusted files only)
    ///
    /// Not allowed with --require-source-complete: limit at the source instead, so its
    /// completion marker covers exactly the selected records.
    #[arg(long, value_name = "N")]
    limit: Option<u64>,
    /// Parse/transform worker threads, 1-256
    ///
    /// Results are always committed in input order. With N > 1 a reader thread is added and
    /// at most 8*N records are in flight.
    #[arg(short, long, value_name = "N", default_value_t = 1)]
    workers: usize,
    /// Gzip worker threads after deduplication, 1-256
    ///
    /// With N > 1 at most 8*N accepted entities are in flight; entity lines are still written
    /// in acceptance order.
    #[arg(long, value_name = "N", default_value_t = 1)]
    compression_workers: usize,
}

#[derive(Args, Debug)]
struct VerifyArgs {
    /// Bundle directory written by `process`
    #[arg(value_name = "BUNDLE_DIR")]
    dir: PathBuf,
}
/// Submitted-but-uncommitted records per transform worker. Contract cost varies widely;
/// a deeper window keeps workers busy behind an expensive earlier contract.
const RECORDS_PER_WORKER: usize = 8;
/// Accepted-but-unwritten entities per gzip worker, for the same reason with payload sizes.
const ENTITIES_PER_COMPRESSOR: usize = 8;
/// Raw stdin lines the reader may buffer ahead, so a producer writing a source batch in a
/// burst need not wait for transformation.
/// Files cannot stall a producer and use two lines per worker (fewer cache-cold lines).
const PIPE_READ_AHEAD_LINES: usize = 128;
/// Read buffer: large reads for files; pipes return what is available.
const INPUT_BUFFER: usize = 1 << 20;
struct Writer {
    dir: PathBuf,
    owner: String,
    part_size: usize,
    seen: HashSet<[u8; 32]>,
    payloads: HashMap<[u8; 32], (u64, usize)>,
    pack: BufWriter<File>,
    metadata: BufWriter<File>,
    part: Option<BufWriter<File>>,
    part_paths: Vec<String>,
    pack_bytes: u64,
    count: u64,
    part_hash: sha2::Sha256,
    part_bytes: u64,
    part_audit: Vec<Value>,
    compression_seconds: f64,
    commit_seconds: f64,
    compressors: Option<Compressors>,
    compression_pending: BTreeMap<u64, Compressed>,
    accepted: u64,
    compression_capacity: usize,
    peak_compression_in_flight: usize,
    /// Records the source skipped (completion marker `skipped`), copied into report.json only.
    source_skipped: Option<Vec<Value>>,
}
impl Writer {
    fn new(a: &ProcessArgs) -> Result<Self> {
        ensure!(a.chain_id > 0, "destination-chain-id must be positive");
        ensure!(
            a.owner != "0x0000000000000000000000000000000000000000",
            "owner must be nonzero"
        );
        ensure!(a.part_entities > 0, "part-size must be positive");
        ensure!(
            a.owner.starts_with("0x") && a.owner.len() == 42 && hex::decode(&a.owner[2..]).is_ok(),
            "invalid owner"
        );
        ensure!(
            !a.output.exists(),
            "output already exists; use a fresh directory (v1 does not resume)"
        );
        fs::create_dir_all(a.output.join("entities"))?;
        use sha2::Digest;
        Ok(Self {
            dir: a.output.clone(),
            owner: a.owner.to_lowercase(),
            part_size: a.part_entities,
            seen: HashSet::new(),
            payloads: HashMap::new(),
            pack: BufWriter::new(File::create(a.output.join("payloads.bin"))?),
            metadata: BufWriter::new(File::create(a.output.join("metadata.jsonl"))?),
            part: None,
            part_paths: vec![],
            pack_bytes: 0,
            count: 0,
            part_hash: sha2::Sha256::new(),
            part_bytes: 0,
            part_audit: vec![],
            compression_seconds: 0.,
            commit_seconds: 0.,
            compressors: (a.compression_workers > 1)
                .then(|| Compressors::new(a.compression_workers, &a.owner.to_lowercase())),
            compression_pending: BTreeMap::new(),
            accepted: 0,
            compression_capacity: if a.compression_workers == 1 {
                1
            } else {
                a.compression_workers * ENTITIES_PER_COMPRESSOR
            },
            peak_compression_in_flight: 0,
            source_skipped: None,
        })
    }
    fn close_part(&mut self) -> Result<()> {
        use sha2::Digest;
        if let Some(mut f) = self.part.take() {
            f.flush()?;
            f.get_ref().sync_all()?;
            self.part_audit.push(json!({"path":self.part_paths.last(),"bytes":self.part_bytes,"sha256":hex::encode(self.part_hash.clone().finalize())}));
        }
        Ok(())
    }
    fn write(&mut self, e: Entity) -> Result<()> {
        if self.seen.contains(&e.id) {
            return Ok(());
        }
        for a in e.attributes.values() {
            if a["type"] == "key" {
                let raw = hex::decode(a["value"].as_str().context("invalid reference")?)?;
                let id: [u8; 32] = raw
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("invalid reference length"))?;
                ensure!(
                    self.seen.contains(&id),
                    "missing dependency: {}",
                    hex::encode(id)
                );
            }
        }
        let hash = sha(&e.payload);
        let (offset, length) = if let Some(p) = self.payloads.get(&hash) {
            *p
        } else {
            let p = (self.pack_bytes, e.payload.len());
            self.pack.write_all(&e.payload)?;
            self.pack_bytes += e.payload.len() as u64;
            self.payloads.insert(hash, p);
            p
        };
        serde_json::to_writer(
            &mut self.metadata,
            &json!({"id":hex::encode(e.id),"key":entity_key(&e.id),"kind":e.kind,"contentType":e.content_type,"attributes":e.attributes,"payloadSha256":hex::encode(hash),"payloadOffset":offset,"payloadBytes":length}),
        )?;
        self.metadata.write_all(b"\n")?;
        let id = e.id;
        if let Some(pool) = &self.compressors {
            pool.submit(self.accepted, e, hash)?;
            self.seen.insert(id);
            self.accepted += 1;
            self.peak_compression_in_flight = self
                .peak_compression_in_flight
                .max((self.accepted - self.count) as usize);
            while self.accepted - self.count >= self.compression_capacity as u64 {
                self.collect_compressed()?;
            }
        } else {
            let compressed = compress(e, hash, &self.owner)?;
            self.commit_compressed(compressed)?;
            self.seen.insert(id);
            self.accepted += 1;
            self.peak_compression_in_flight = 1;
        }
        Ok(())
    }
    fn collect_compressed(&mut self) -> Result<()> {
        let (sequence, result) = self.compressors.as_ref().unwrap().receive()?;
        self.compression_pending.insert(sequence, result?);
        while let Some(compressed) = self.compression_pending.remove(&self.count) {
            self.commit_compressed(compressed)?;
        }
        Ok(())
    }
    fn drain_compression(&mut self) -> Result<()> {
        while self.count < self.accepted {
            self.collect_compressed()?;
        }
        Ok(())
    }
    fn commit_compressed(&mut self, compressed: Compressed) -> Result<()> {
        use sha2::Digest;
        let Compressed { line, seconds } = compressed;
        self.compression_seconds += seconds;
        if self.count.is_multiple_of(self.part_size as u64) {
            self.close_part()?;
            let path = format!("entities/{:06}.jsonl", self.part_paths.len() + 1);
            self.part = Some(BufWriter::new(File::create(self.dir.join(&path))?));
            self.part_paths.push(path);
            self.part_hash = sha2::Sha256::new();
            self.part_bytes = 0;
        }
        self.part.as_mut().unwrap().write_all(&line)?;
        self.part_hash.update(&line);
        self.part_bytes += line.len() as u64;
        self.count += 1;
        Ok(())
    }
    fn finish(mut self, a: &ProcessArgs, records: u64, elapsed: f64) -> Result<()> {
        ensure!(
            self.accepted == self.count,
            "compression must drain before publishing manifest"
        );
        self.close_part()?;
        self.pack.flush()?;
        self.pack.get_ref().sync_all()?;
        self.metadata.flush()?;
        self.metadata.get_ref().sync_all()?;
        let mut report = json!({"schema":"sourcify-offline-import.v1","status":"completed","sourceChainId":1,"contracts":records,"entities":self.count,"uniquePayloads":self.payloads.len(),"payloadBytes":self.pack_bytes,"seconds":elapsed,"contractsPerSecond":records as f64/elapsed,"parts":self.part_audit,"identity":"helper staging entity SHA-256, semantic attributes included","expiry":"never","proxyResolution":"offline; unknown remains unknown","sourceCompleteRequired":a.require_source_complete,"workers":a.workers,"commitSeconds":self.commit_seconds,"compressionSeconds":self.compression_seconds,"compressionTiming":"sum of per-job elapsed seconds; overlaps across workers","maxInFlightRecords":if a.workers == 1 {1} else {a.workers * RECORDS_PER_WORKER},"compressionWorkers":a.compression_workers,"maxInFlightCompression":self.compression_capacity,"peakInFlightCompression":self.peak_compression_in_flight});
        if let Some(skipped) = self.source_skipped.take() {
            report["sourceSkippedCount"] = json!(skipped.len());
            report["sourceSkipped"] = Value::Array(skipped);
        }
        write_json(self.dir.join("report.json"), &report)?;
        write_json(
            self.dir.join("seed.json.tmp"),
            &json!({"version":1,"chainId":a.chain_id,"entities":self.part_paths.iter().map(|p|json!({"path":p})).collect::<Vec<_>>()}),
        )?;
        let mut sums = String::new();
        for path in self.part_paths.iter().map(String::as_str).chain([
            "payloads.bin",
            "metadata.jsonl",
            "report.json",
            "seed.json",
        ]) {
            use sha2::Digest;
            let mut file = File::open(self.dir.join(if path == "seed.json" {
                "seed.json.tmp"
            } else {
                path
            }))?;
            let mut digest = sha2::Sha256::new();
            let mut buffer = [0u8; 65536];
            loop {
                let n = file.read(&mut buffer)?;
                if n == 0 {
                    break;
                }
                digest.update(&buffer[..n]);
            }
            sums.push_str(&format!("{}  {}\n", hex::encode(digest.finalize()), path));
        }
        let mut checksum_file = File::create(self.dir.join("SHA256SUMS"))?;
        checksum_file.write_all(sums.as_bytes())?;
        checksum_file.sync_all()?;
        fs::rename(self.dir.join("seed.json.tmp"), self.dir.join("seed.json"))?;
        File::open(&self.dir)?.sync_all()?;
        eprintln!("{}", report);
        Ok(())
    }
}
fn write_json(path: PathBuf, v: &Value) -> Result<()> {
    let mut f = File::create(path)?;
    serde_json::to_writer_pretty(&mut f, v)?;
    f.write_all(b"\n")?;
    f.sync_all()?;
    Ok(())
}
/// A finished seed JSONL line for one accepted entity.
struct Compressed {
    line: Vec<u8>,
    seconds: f64,
}
/// Gzips the payload, then builds the import-ready line off the ordered writer.
fn compress(entity: Entity, payload_hash: [u8; 32], owner: &str) -> Result<Compressed> {
    let start = Instant::now();
    let mut enc = GzEncoder::new(Vec::new(), Compression::new(6));
    enc.write_all(&entity.payload)?;
    let zipped = enc.finish()?;
    ensure!(
        zipped.len().min(entity.payload.len()) <= 120000,
        "prepared payload exceeds wire limit"
    );
    let seconds = start.elapsed().as_secs_f64();
    let line = seed_line(entity, zipped, payload_hash, owner)?;
    Ok(Compressed { line, seconds })
}
fn seed_line(e: Entity, zipped: Vec<u8>, hash: [u8; 32], owner: &str) -> Result<Vec<u8>> {
    let (payload, encoding, ctype) = if zipped.len() < e.payload.len() {
        (zipped, "gzip", "application/gzip")
    } else {
        (e.payload.clone(), "identity", e.content_type.as_str())
    };
    ensure!(
        payload.len() <= 120000,
        "prepared payload exceeds wire limit"
    );
    let mut wire = json!({"$key":entity_key(&e.id),"$creator":owner,"$owner":owner,"$expiresAt":"never","$contentType":ctype,"$payload":STANDARD.encode(payload),"$creationFlags":[]});
    let obj = wire.as_object_mut().unwrap();
    for (name, a) in &e.attributes {
        let t = a["type"].as_str().context("attribute type missing")?;
        let v = &a["value"];
        let value = if t == "key" {
            let raw = hex::decode(v.as_str().context("key missing")?)?;
            entity_key(&raw.try_into().map_err(|_| anyhow::anyhow!("invalid key"))?)
        } else if let Some(s) = v.as_str() {
            s.into()
        } else {
            v.to_string()
        };
        obj.insert(
            if name == "projectId" {
                "projectid".into()
            } else {
                name.clone()
            },
            json!(format!("{t}:{value}")),
        );
    }
    for (k, v) in [
        ("contentencoding", format!("str:{encoding}")),
        ("originalcontenttype", format!("str:{}", e.content_type)),
        ("payloadsha256", format!("bytes32:{}", hx(hash))),
        ("stagingid", format!("bytes32:{}", hx(e.id))),
        ("uploadschema", "str:sourcify-arkiv.upload.v1".into()),
    ] {
        obj.insert(k.into(), json!(v));
    }
    let mut line = bytes(&wire);
    line.push(b'\n');
    Ok(line)
}
/// Submitted minus committed is bounded across queued, active and reordered work.
/// Only unique entities enter this pool; file writes and hash indexes stay ordered.
struct Compressors {
    jobs: Option<mpsc::SyncSender<(u64, Entity, [u8; 32])>>,
    results: Option<mpsc::Receiver<(u64, Result<Compressed>)>>,
    threads: Vec<thread::JoinHandle<()>>,
}
impl Compressors {
    fn new(count: usize, owner: &str) -> Self {
        let capacity = count * ENTITIES_PER_COMPRESSOR;
        let (jobs, receiver) = mpsc::sync_channel::<(u64, Entity, [u8; 32])>(capacity);
        let receiver = Arc::new(Mutex::new(receiver));
        let (sender, results) = mpsc::sync_channel(capacity);
        let threads = (0..count)
            .map(|_| {
                let receiver = Arc::clone(&receiver);
                let sender = sender.clone();
                let owner = owner.to_owned();
                thread::spawn(move || loop {
                    let job = receiver.lock().unwrap().recv();
                    let Ok((sequence, entity, hash)) = job else {
                        break;
                    };
                    let result = std::panic::catch_unwind(|| compress(entity, hash, &owner))
                        .unwrap_or_else(|_| Err(anyhow::anyhow!("compression worker panicked")));
                    if sender.send((sequence, result)).is_err() {
                        break;
                    }
                })
            })
            .collect();
        Self {
            jobs: Some(jobs),
            results: Some(results),
            threads,
        }
    }
    fn submit(&self, sequence: u64, entity: Entity, hash: [u8; 32]) -> Result<()> {
        self.jobs
            .as_ref()
            .unwrap()
            .send((sequence, entity, hash))
            .map_err(|_| anyhow::anyhow!("compression workers disconnected"))
    }
    fn receive(&self) -> Result<(u64, Result<Compressed>)> {
        self.results
            .as_ref()
            .unwrap()
            .recv()
            .context("compression workers disconnected")
    }
}
impl Drop for Compressors {
    fn drop(&mut self) {
        // Disconnect results before joining even on source/write/compression failure.
        self.jobs.take();
        self.results.take();
        for worker in self.threads.drain(..) {
            let _ = worker.join();
        }
    }
}
type Job = (u64, Vec<u8>);
type Outcome = (u64, Result<Line>);

/// Workers parse and transform raw input lines.
/// The caller limits submitted-but-uncommitted lines, not just queue sizes.
/// This also bounds out-of-order results when an early contract is expensive.
struct Workers {
    jobs: Option<mpsc::SyncSender<Job>>,
    results: Option<mpsc::Receiver<Outcome>>,
    threads: Vec<thread::JoinHandle<()>>,
}
impl Workers {
    fn new(count: usize, capacity: usize) -> Self {
        let (jobs, receiver) = mpsc::sync_channel::<Job>(capacity);
        let receiver = Arc::new(Mutex::new(receiver));
        let (sender, results) = mpsc::sync_channel(capacity);
        let threads = (0..count)
            .map(|_| {
                let receiver = Arc::clone(&receiver);
                let sender = sender.clone();
                thread::spawn(move || loop {
                    let job = receiver.lock().unwrap().recv();
                    let Ok((sequence, line)) = job else { break };
                    let outcome = std::panic::catch_unwind(|| transform_line(line))
                        .unwrap_or_else(|_| Err(anyhow::anyhow!("transform worker panicked")))
                        .with_context(|| format!("contract record {}", sequence + 1));
                    if sender.send((sequence, outcome)).is_err() {
                        break;
                    }
                })
            })
            .collect();
        Self {
            jobs: Some(jobs),
            results: Some(results),
            threads,
        }
    }
    fn submit(&self, sequence: u64, line: Vec<u8>) -> Result<()> {
        self.jobs
            .as_ref()
            .unwrap()
            .send((sequence, line))
            .map_err(|_| anyhow::anyhow!("transform workers disconnected"))
    }
    fn receive(&self) -> Result<Outcome> {
        self.results
            .as_ref()
            .unwrap()
            .recv()
            .context("transform workers disconnected")
    }
}
impl Drop for Workers {
    fn drop(&mut self) {
        // Disconnect both directions before joining, including on malformed input
        // or writer failure, so workers cannot block on an undrained result queue.
        self.jobs.take();
        self.results.take();
        for worker in self.threads.drain(..) {
            let _ = worker.join();
        }
    }
}
fn commit(
    writer: &mut Writer,
    entities: Vec<Entity>,
    records: &mut u64,
    start: Instant,
) -> Result<()> {
    let commit_start = Instant::now();
    for entity in entities {
        writer.write(entity)?;
    }
    writer.commit_seconds += commit_start.elapsed().as_secs_f64();
    *records += 1;
    if (*records).is_multiple_of(1000) {
        eprintln!(
            "{}",
            json!({"contracts":records,"entities":writer.count,"payloadHashes":writer.payloads.len(),"payloadBytes":writer.pack_bytes,"elapsedSeconds":start.elapsed().as_secs_f64()})
        );
    }
    Ok(())
}
/// Applies transformed lines in input order and enforces completion-marker rules.
struct Progress {
    records: u64,
    complete: bool,
    limit: Option<u64>,
}
impl Progress {
    /// Returns false once `--limit` records are committed.
    fn apply(&mut self, line: Line, writer: &mut Writer, start: Instant) -> Result<bool> {
        ensure!(!self.complete, "data after source completion marker");
        match line {
            Line::Complete { records, skipped } => {
                ensure!(
                    records == Some(self.records),
                    "source completion count differs"
                );
                if let Some(skipped) = skipped {
                    writer.source_skipped = Some(skipped_records(skipped)?);
                }
                self.complete = true;
            }
            Line::Record(entities) => commit(writer, entities, &mut self.records, start)?,
        }
        Ok(self.limit != Some(self.records))
    }
}
/// Validates the completion marker's optional `skipped` list: objects with string `matchId` and `reason`.
fn skipped_records(value: Value) -> Result<Vec<Value>> {
    let Value::Array(items) = value else {
        bail!("source completion skipped list is not an array");
    };
    for item in &items {
        let field = |name| item.get(name).is_some_and(Value::is_string);
        ensure!(
            item.is_object() && field("matchId") && field("reason"),
            "source completion skipped entry is malformed"
        );
    }
    Ok(items)
}
/// Next non-blank line without its line ending.
fn read_line(input: &mut dyn BufRead) -> io::Result<Option<Vec<u8>>> {
    loop {
        let mut line = Vec::new();
        if input.read_until(b'\n', &mut line)? == 0 {
            return Ok(None);
        }
        if line.ends_with(b"\n") {
            line.pop();
            if line.ends_with(b"\r") {
                line.pop();
            }
        }
        // `str::trim` semantics; a leading printable byte settles almost every line.
        let blank = line.first().is_none_or(|b| !b.is_ascii_graphic())
            && std::str::from_utf8(&line).is_ok_and(|l| l.trim().is_empty());
        if !blank {
            return Ok(Some(line));
        }
    }
}
type Input = Box<dyn BufRead + Send>;
/// Reads raw lines on a dedicated thread so input I/O overlaps transformation and commit.
/// The channel holds at most `capacity` lines; `Ok(None)` marks the end of input.
fn spawn_reader(
    mut input: Input,
    capacity: usize,
    max_lines: Option<u64>,
) -> mpsc::Receiver<io::Result<Option<Vec<u8>>>> {
    let (sender, lines) = mpsc::sync_channel(capacity);
    // Detached: on early failure it stops at its next send; a blocked read ends with the process.
    thread::spawn(move || {
        for count in 0.. {
            let line = if max_lines.is_some_and(|max| count >= max) {
                Ok(None)
            } else {
                read_line(&mut *input)
            };
            let more = matches!(line, Ok(Some(_)));
            if sender.send(line).is_err() || !more {
                break;
            }
        }
    });
    lines
}
/// Parallel mode: a reader thread, `workers` parse/transform threads and this ordered writer.
/// At most `capacity` lines are submitted but not yet applied (queued, active or reordered),
/// plus the reader's bounded read-ahead and the line it is reading.
fn run_parallel(
    a: &ProcessArgs,
    input: Input,
    progress: &mut Progress,
    writer: &mut Writer,
    start: Instant,
) -> Result<()> {
    let capacity = a.workers * RECORDS_PER_WORKER;
    // With --limit, one extra line decides whether a completion marker is followed by data.
    let read_ahead = if a.input == "-" {
        PIPE_READ_AHEAD_LINES
    } else {
        a.workers * 2
    };
    let lines = spawn_reader(input, read_ahead, a.limit.map(|l| l.saturating_add(1)));
    let workers = Workers::new(a.workers, capacity);
    let mut pending = BTreeMap::new();
    let (mut submitted, mut applied) = (0u64, 0u64);
    let mut input_open = true;
    let mut input_error = None;
    loop {
        // Feed workers first; block on input only when no result is outstanding.
        while input_open && submitted - applied < capacity as u64 {
            let line = if submitted == applied {
                lines.recv().context("input reader stopped")?
            } else {
                match lines.try_recv() {
                    Ok(line) => line,
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => bail!("input reader stopped"),
                }
            };
            match line {
                Ok(Some(line)) => {
                    workers.submit(submitted, line)?;
                    submitted += 1;
                }
                Ok(None) => input_open = false,
                // Raised only if every earlier line is applied without reaching --limit.
                Err(e) => (input_open, input_error) = (false, Some(e)),
            }
        }
        if applied == submitted {
            break;
        }
        let (sequence, outcome) = workers.receive()?;
        pending.insert(sequence, outcome);
        // Errors surface in input order, like the serial path.
        while let Some(outcome) = pending.remove(&applied) {
            applied += 1;
            if !progress.apply(outcome?, writer, start)? {
                return Ok(());
            }
        }
    }
    input_error.map_or(Ok(()), |e| Err(e.into()))
}
fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Process(a) => process(&a),
        Command::Hydrate(a) => hydrate_cli::hydrate(&a),
        Command::Verify(v) => {
            let summary = verify::verify(&v.dir)?;
            println!("OK {}: {summary}", v.dir.display());
            Ok(())
        }
        Command::Pack(a) => pack::pack(&a),
        Command::Upload(a) => upload::upload(&a),
    }
}
/// `process`: the offline importer. Writes the bundle and publishes seed.json last.
fn process(a: &ProcessArgs) -> Result<()> {
    ensure!(
        !(a.require_source_complete && a.limit.is_some()),
        "limit must be applied at source with required completion marker"
    );
    ensure!(a.limit != Some(0), "limit must be positive");
    ensure!(
        a.workers > 0 && a.workers <= 256,
        "workers must be between 1 and 256"
    );
    ensure!(
        a.compression_workers > 0 && a.compression_workers <= 256,
        "compression-workers must be between 1 and 256"
    );
    let mut input: Input = if a.input == "-" {
        Box::new(BufReader::with_capacity(INPUT_BUFFER, io::stdin()))
    } else {
        Box::new(BufReader::with_capacity(
            INPUT_BUFFER,
            File::open(&a.input)?,
        ))
    };
    let mut writer = Writer::new(a)?;
    let start = Instant::now();
    let mut progress = Progress {
        records: 0,
        complete: false,
        limit: a.limit,
    };
    if a.workers > 1 {
        run_parallel(a, input, &mut progress, &mut writer, start)?;
    } else {
        while let Some(line) = read_line(&mut *input)? {
            let line = transform_line(line)
                .with_context(|| format!("contract record {}", progress.records + 1))?;
            if !progress.apply(line, &mut writer, start)? {
                break;
            }
        }
    }
    let records = progress.records;
    ensure!(records > 0, "empty source");
    ensure!(
        !a.require_source_complete || progress.complete,
        "source ended without completion marker; incomplete output has no seed.json"
    );
    let drain_start = Instant::now();
    writer.drain_compression()?;
    writer.commit_seconds += drain_start.elapsed().as_secs_f64();
    writer.finish(a, records, start.elapsed().as_secs_f64())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn args(name: &str) -> ProcessArgs {
        ProcessArgs {
            input: "-".into(),
            output: std::env::temp_dir()
                .join(format!("rust-import-test-{}-{name}", std::process::id())),
            owner: "0x1111111111111111111111111111111111111111".into(),
            chain_id: 7706815,
            part_entities: 1,
            require_source_complete: false,
            limit: None,
            workers: 1,
            compression_workers: 1,
        }
    }
    #[test]
    fn dedup_preserves_semantic_variants() {
        let a = args("dedup");
        let mut w = Writer::new(&a).unwrap();
        let e = Entity::new(
            "code",
            vec![1, 2, 3],
            "application/octet-stream",
            serde_json::Map::new(),
        );
        w.write(e.clone()).unwrap();
        w.write(e).unwrap();
        w.write(Entity::new(
            "blob",
            vec![1, 2, 3],
            "application/octet-stream",
            serde_json::Map::new(),
        ))
        .unwrap();
        assert_eq!(w.count, 2);
        assert_eq!(w.payloads.len(), 1);
        assert_eq!(w.pack_bytes, 3);
        w.finish(&a, 1, 1.).unwrap();
        assert!(a.output.join("seed.json").exists());
        assert!(a.output.join("SHA256SUMS").exists());
        fs::remove_dir_all(a.output).unwrap();
    }
    #[test]
    fn unresolved_key_fails() {
        let a = args("missing");
        let mut w = Writer::new(&a).unwrap();
        let mut attrs = serde_json::Map::new();
        attrs.insert(
            "compilationref".into(),
            json!({"type":"key","value":"00".repeat(32)}),
        );
        assert!(w
            .write(Entity::new(
                "verified_contract",
                vec![],
                "application/json",
                attrs
            ))
            .is_err());
        assert!(!a.output.join("seed.json").exists());
        drop(w);
        fs::remove_dir_all(a.output).unwrap();
    }
    #[test]
    fn compression_window_dedups_and_accepts_pending_dependencies() {
        let mut a = args("compression-window");
        a.compression_workers = 2;
        let mut w = Writer::new(&a).unwrap();
        let parent = Entity::new(
            "code",
            vec![7; 100000],
            "application/octet-stream",
            serde_json::Map::new(),
        );
        w.write(parent.clone()).unwrap();
        assert_eq!(w.count, 0); // Accepted but not yet committed.
        w.write(parent.clone()).unwrap();
        assert_eq!(w.accepted, 1); // Duplicate never enters gzip queue.
        let mut attrs = serde_json::Map::new();
        attrs.insert(
            "parent".into(),
            json!({"type":"key","value":hex::encode(parent.id)}),
        );
        for i in 0..30 {
            attrs.insert("index".into(), json!({"type":"str","value":i.to_string()}));
            w.write(Entity::new(
                "blob",
                parent.payload.clone(),
                "application/octet-stream",
                attrs.clone(),
            ))
            .unwrap();
            assert!(w.accepted - w.count < w.compression_capacity as u64);
        }
        w.drain_compression().unwrap();
        assert_eq!(w.count, 31);
        assert_eq!(w.payloads.len(), 1);
        assert_eq!(w.peak_compression_in_flight, w.compression_capacity);
        w.finish(&a, 1, 1.).unwrap();
        fs::remove_dir_all(a.output).unwrap();
    }
    #[test]
    fn compression_and_disk_errors_leave_no_manifest() {
        for failure in ["gzip", "disk"] {
            let mut a = args(failure);
            a.compression_workers = 2;
            let mut w = Writer::new(&a).unwrap();
            if failure == "disk" {
                // Fails the ordered seed write while compression workers exist.
                fs::remove_dir(a.output.join("entities")).unwrap();
            }
            let payload = if failure == "gzip" {
                // Deterministic high entropy exceeds both raw and compressed wire limits.
                (0..6000u64).flat_map(|i| sha(i.to_le_bytes())).collect()
            } else {
                vec![1; 100]
            };
            w.write(Entity::new(
                "code",
                payload,
                "application/octet-stream",
                serde_json::Map::new(),
            ))
            .unwrap();
            assert!(w.drain_compression().is_err());
            drop(w); // Must disconnect and join without a blocked result sender.
            assert!(!a.output.join("seed.json").exists());
            fs::remove_dir_all(a.output).unwrap();
        }
    }
    #[test]
    fn cli_is_consistent() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
    fn parse_process(args: &[&str]) -> ProcessArgs {
        let cli = Cli::try_parse_from(
            ["arkiv-entity-processor", "process"]
                .iter()
                .chain(args)
                .copied(),
        )
        .unwrap();
        let Command::Process(a) = cli.command else {
            panic!("process expected");
        };
        a
    }
    #[test]
    fn legacy_importer_flags_still_parse() {
        // Former `sourcify-offline-importer` invocation (run.sh) plus renamed flags.
        let a = parse_process(&[
            "--workers",
            "4",
            "--compression-workers",
            "3",
            "--input",
            "-",
            "--output",
            "out",
            "--require-source-complete",
            "--owner",
            "0x1111111111111111111111111111111111111111",
            "--chain-id",
            "5",
            "--part-entities",
            "7",
        ]);
        assert_eq!((a.workers, a.compression_workers), (4, 3));
        assert_eq!((a.input.as_str(), a.output.to_str()), ("-", Some("out")));
        assert!(a.require_source_complete);
        assert_eq!(a.owner, "0x1111111111111111111111111111111111111111");
        assert_eq!((a.chain_id, a.part_entities, a.limit), (5, 7, None));
    }
    #[test]
    fn new_flags_and_defaults() {
        let a = parse_process(&["-o", "out"]);
        assert_eq!(a.input, "-");
        assert_eq!(a.owner, "0x0000000000000000000000000000000000000001");
        assert_eq!((a.chain_id, a.part_entities), (7706815, 10000));
        assert_eq!((a.workers, a.compression_workers), (1, 1));
        assert!(!a.require_source_complete);
        let a = parse_process(&[
            "-i",
            "in.ndjson",
            "-o",
            "out",
            "--destination-chain-id",
            "9",
            "--part-size",
            "3",
            "-w",
            "2",
            "--limit",
            "10",
        ]);
        assert_eq!(a.input, "in.ndjson");
        assert_eq!((a.chain_id, a.part_entities, a.workers), (9, 3, 2));
        assert_eq!(a.limit, Some(10));
        assert!(Cli::try_parse_from(["arkiv-entity-processor", "process"]).is_err());
    }
    #[test]
    fn verify_accepts_complete_and_rejects_tampered_bundles() {
        let a = args("verify");
        let mut w = Writer::new(&a).unwrap();
        for kind in ["code", "blob"] {
            w.write(Entity::new(
                kind,
                vec![9; 64],
                "application/octet-stream",
                serde_json::Map::new(),
            ))
            .unwrap();
        }
        w.finish(&a, 1, 1.).unwrap();
        assert!(verify::verify(&a.output).is_ok());
        let part = a.output.join("entities/000002.jsonl");
        let original = fs::read(&part).unwrap();
        fs::write(&part, b"tampered\n").unwrap();
        let error = verify::verify(&a.output).unwrap_err().to_string();
        assert!(
            error.contains("entities/000002.jsonl: checksum mismatch"),
            "{error}"
        );
        fs::write(&part, original).unwrap();
        fs::remove_file(a.output.join("seed.json")).unwrap();
        assert!(verify::verify(&a.output).is_err());
        fs::remove_dir_all(a.output).unwrap();
    }
}
