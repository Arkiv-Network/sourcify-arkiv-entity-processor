//! `pack`: one deterministic `.tar.zst` archive (PAX tar, zstd) of a verified bundle, plus a
//! JSON record of its size and digests for `upload` and for reusing it later.
use crate::verify;
use anyhow::{anyhow, bail, ensure, Context, Result};
use clap::Args;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fmt::Write as _,
    fs::{self, File, OpenOptions},
    io::{self, BufWriter, Read, Write},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};

pub const LONG_ABOUT: &str = "\
Pack a verified bundle into one deterministic tar.zst archive, ready for `upload`.

The bundle is verified first (as `verify` does). Symlinks, special files and files not listed in
SHA256SUMS are refused, and every file is hashed again while it is packed: the archive holds
exactly what its SHA256SUMS describes.

--contents prefill (default) packs what arkiv-prefill loads: seed.json, the entity parts and
report.json, with a SHA256SUMS listing exactly those files. The audit files payloads.bin and
metadata.jsonl (more than half of a full bundle) stay in the local bundle only. --contents full
packs every bundle file with the bundle's own SHA256SUMS.

Archive: PAX tar compressed with zstd (multithreaded, with a content checksum). Files sit at the
archive root (SHA256SUMS, entities/NNNNNN.jsonl, report.json, seed.json, ...), sorted by path,
with mode 0644, uid/gid 0, empty owner names and mtime 0, so the same bundle, --contents and
--level always give a byte-identical archive, for any --threads.

The archive is written to <FILE>.partial, synced and renamed to <FILE> only on success. Next to
it goes the record <FILE>.json:
  {\"schema\":\"arkiv-entity-processor.archive.v1\",\"archive\":\"<file name>\",\"bytes\":N,
   \"sha256\":\"<archive SHA-256>\",\"sourceSha256\":\"<SHA-256 of the bundle's SHA256SUMS>\",
   \"contents\":\"prefill\",\"files\":N,\"bundleBytes\":N,\"zstdLevel\":N}
(bundleBytes: the packed files' total size.)
The record is also printed to stdout as one JSON line; progress goes to stderr as JSON lines.

An existing <FILE> is never overwritten. If its record matches (same sourceSha256, size, contents
and level;
--verify-existing also re-hashes the archive) it is reused and the command succeeds; otherwise
the command fails.";

pub const EXAMPLES: &str = "\
Examples:
  arkiv-entity-processor pack bundle --output sourcify-2026-10-05.tar.zst
  arkiv-entity-processor pack bundle -o /data/seed.tar.zst --level 6 --threads 16
  arkiv-entity-processor pack bundle -o /data/full.tar.zst --contents full   # incl. audit files

  # Inspect: files at the archive root; check them after extracting
  tar -I zstd -tvf sourcify-2026-10-05.tar.zst
  mkdir seed && tar -I zstd -xf sourcify-2026-10-05.tar.zst -C seed && (cd seed && sha256sum -c SHA256SUMS)";

pub const SCHEMA: &str = "arkiv-entity-processor.archive.v1";
/// How often long-running steps print a progress line.
pub const PROGRESS_INTERVAL: Duration = Duration::from_secs(10);
const BLOCK: usize = 512;
/// Archives end padded to a 20-block record, like tar(1) and Python's tarfile.
const RECORD: usize = 20 * BLOCK;
const ZEROS: [u8; RECORD] = [0; RECORD];
/// Read size for bundle files; at most `BUFFERS` chunks are in flight between reader and encoder.
const CHUNK: usize = 1 << 20;
const BUFFERS: usize = 8;
const MIB: f64 = (1 << 20) as f64;
/// Audit files of a bundle that arkiv-prefill does not read; left out by `--contents prefill`.
pub const AUDIT_FILES: [&str; 2] = ["metadata.jsonl", "payloads.bin"];

/// Which bundle files go into the archive.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Contents {
    /// seed.json, entity parts and report.json, with a SHA256SUMS of exactly these files
    Prefill,
    /// every bundle file, with the bundle's own SHA256SUMS
    Full,
}

impl Contents {
    fn name(self) -> &'static str {
        match self {
            Contents::Prefill => "prefill",
            Contents::Full => "full",
        }
    }
}

#[derive(Args, Debug)]
pub struct PackArgs {
    /// Bundle directory written by `process`
    #[arg(value_name = "BUNDLE_DIR")]
    pub dir: PathBuf,
    /// Archive to write; must end in .tar.zst
    ///
    /// Written as <FILE>.partial and renamed on success, with the record <FILE>.json. An
    /// existing <FILE> is reused if its record matches the bundle, else refused.
    #[arg(short, long, value_name = "FILE.tar.zst")]
    pub output: PathBuf,
    /// Files to pack: `prefill` leaves out the audit files payloads.bin and metadata.jsonl
    #[arg(long, value_enum, value_name = "WHAT", default_value = "prefill")]
    pub contents: Contents,
    /// zstd compression level, 1-19
    #[arg(long, value_name = "N", default_value_t = 3)]
    pub level: i32,
    /// zstd worker threads [default: available CPUs, at most 8]
    ///
    /// Does not change the archive bytes.
    #[arg(long, value_name = "N")]
    pub threads: Option<u32>,
    /// Re-hash an existing archive before reusing it
    ///
    /// Without it, reuse only checks the record against SHA256SUMS and the archive size.
    #[arg(long)]
    pub verify_existing: bool,
}

/// One archive file, in archive order: a bundle file, or generated content (the SHA256SUMS of a
/// prefill archive).
struct Entry {
    path: String,
    size: u64,
    sha256: String,
    data: Option<Vec<u8>>,
}

pub fn pack(a: &PackArgs) -> Result<()> {
    let record = pack_record(a)?;
    println!("{record}");
    Ok(())
}

/// Packs (or reuses) the archive and returns its record.
fn pack_record(a: &PackArgs) -> Result<Value> {
    ensure!(
        (1..=19).contains(&a.level),
        "level must be between 1 and 19"
    );
    let threads = a
        .threads
        .unwrap_or_else(|| thread::available_parallelism().map_or(1, |n| n.get().min(8) as u32));
    ensure!(
        (1..=256).contains(&threads),
        "threads must be between 1 and 256"
    );
    let name = a
        .output
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| n.len() > ".tar.zst".len() && n.ends_with(".tar.zst"))
        .context("--output must name a file ending in .tar.zst")?;
    let parent = match a.output.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let bundle = fs::canonicalize(&a.dir)
        .with_context(|| format!("bundle directory {}", a.dir.display()))?;
    ensure!(
        !fs::canonicalize(parent)
            .with_context(|| format!("output directory {}", parent.display()))?
            .starts_with(&bundle),
        "--output must be outside the bundle directory"
    );
    let record_path = record_path(&a.output);
    if fs::symlink_metadata(&a.output).is_ok() {
        return reuse(a, name, &record_path);
    }

    let start = Instant::now();
    let summary = verify::verify(&a.dir)?;
    eprintln!(
        "{}",
        json!({"phase": "verified", "bundle": a.dir.display().to_string(), "summary": summary.to_string()})
    );
    let sums = fs::read(a.dir.join("SHA256SUMS")).context("reading SHA256SUMS")?;
    let source_sha256 = hex::encode(Sha256::digest(&sums));
    let entries = select(entries(&a.dir, &sums, &source_sha256)?, &sums, a.contents)?;
    let bundle_bytes: u64 = entries.iter().map(|e| e.size).sum();

    // One writer per output: the lock is held until the archive is published.
    let partial_path = with_suffix(&a.output, ".partial");
    let partial = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&partial_path)
        .with_context(|| format!("creating {}", partial_path.display()))?;
    partial
        .try_lock()
        .map_err(|_| anyhow!("{} is locked by another pack", partial_path.display()))?;
    partial.set_len(0)?;
    let written = write_archive(
        &a.dir,
        &entries,
        &partial,
        a.level,
        threads,
        bundle_bytes,
        start,
    );
    let (bytes, sha256) = match written {
        Ok(result) => result,
        Err(e) => {
            let _ = fs::remove_file(&partial_path);
            return Err(e);
        }
    };
    let record = json!({
        "schema": SCHEMA,
        "archive": name,
        "bytes": bytes,
        "sha256": sha256,
        "sourceSha256": source_sha256,
        "contents": a.contents.name(),
        "files": entries.len(),
        "bundleBytes": bundle_bytes,
        "zstdLevel": a.level,
    });
    // Record first: a crash before the rename leaves a stale record but no archive, which the
    // next run simply replaces.
    let record_partial = with_suffix(&record_path, ".partial");
    let mut text = serde_json::to_string_pretty(&record)?;
    text.push('\n');
    fs::write(&record_partial, text)?;
    File::open(&record_partial)?.sync_all()?;
    fs::rename(&record_partial, &record_path)?;
    publish(&partial_path, &a.output)?;
    File::open(parent)?.sync_all()?;
    let seconds = start.elapsed().as_secs_f64();
    eprintln!(
        "{}",
        json!({"phase": "packed", "archive": a.output.display().to_string(), "files": entries.len(), "bundleBytes": bundle_bytes, "bytes": bytes, "seconds": seconds, "mibPerSecond": bundle_bytes as f64 / MIB / seconds})
    );
    Ok(record)
}

/// `<archive>.json`, the record `pack` writes next to an archive.
pub fn record_path(archive: &Path) -> PathBuf {
    with_suffix(archive, ".json")
}

fn with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    name.into()
}

/// Reads a record and checks its schema and field types.
pub fn read_record(path: &Path) -> Result<Value> {
    let text = fs::read_to_string(path)?;
    let record: Value = serde_json::from_str(&text).context("invalid JSON")?;
    ensure!(record["schema"] == SCHEMA, "schema is not {SCHEMA}");
    let is_digest = |v: &Value| {
        v.as_str()
            .is_some_and(|s| s.len() == 64 && hex::decode(s).is_ok())
    };
    ensure!(
        record["archive"].is_string()
            && record["bytes"].is_u64()
            && is_digest(&record["sha256"])
            && is_digest(&record["sourceSha256"]),
        "missing or invalid archive/bytes/sha256/sourceSha256"
    );
    Ok(record)
}

/// An archive that already exists is reused only with a record matching the current bundle.
fn reuse(a: &PackArgs, name: &str, record_path: &Path) -> Result<Value> {
    let output = a.output.display();
    let refuse = |why: String| {
        anyhow!("{output} already exists and {why}; refusing to overwrite it (remove it or choose another --output)")
    };
    let meta = fs::symlink_metadata(&a.output)?;
    if !meta.is_file() {
        return Err(refuse("is not a regular file".into()));
    }
    let record = read_record(record_path).map_err(|e| {
        refuse(format!(
            "{} is not a usable record: {e:#}",
            record_path.display()
        ))
    })?;
    let sums = fs::read(a.dir.join("SHA256SUMS"))
        .with_context(|| format!("reading {}/SHA256SUMS", a.dir.display()))?;
    if record["archive"] != name {
        return Err(refuse(format!("its record names {}", record["archive"])));
    }
    if record["bytes"].as_u64() != Some(meta.len()) {
        return Err(refuse(format!(
            "its size {} differs from its record ({})",
            meta.len(),
            record["bytes"]
        )));
    }
    // Records written before --contents existed describe full archives.
    let contents = record["contents"].as_str().unwrap_or("full");
    if contents != a.contents.name() {
        return Err(refuse(format!(
            "holds --contents {contents} (requested {})",
            a.contents.name()
        )));
    }
    if record["zstdLevel"].as_i64() != Some(i64::from(a.level)) {
        return Err(refuse(format!(
            "was packed with zstd level {} (requested {})",
            record["zstdLevel"], a.level
        )));
    }
    if record["sourceSha256"] != hex::encode(Sha256::digest(&sums)).as_str() {
        return Err(refuse(format!(
            "was packed from another bundle (SHA256SUMS of {} differs)",
            a.dir.display()
        )));
    }
    if a.verify_existing {
        let (sha256, _) = hash_file(&a.output, "verify-existing", None)?;
        if record["sha256"] != sha256.as_str() {
            return Err(refuse(format!(
                "its SHA-256 {sha256} differs from its record ({})",
                record["sha256"]
            )));
        }
    }
    eprintln!(
        "{}",
        json!({"phase": "reused", "archive": output.to_string(), "verifiedExisting": a.verify_existing})
    );
    Ok(record)
}

/// The files to pack, sorted by path: exactly SHA256SUMS and the files it lists, all regular.
fn entries(dir: &Path, sums: &[u8], source_sha256: &str) -> Result<Vec<Entry>> {
    let sums = std::str::from_utf8(sums).context("SHA256SUMS is not UTF-8")?;
    let mut listed = BTreeMap::new();
    for line in sums.lines() {
        let (digest, path) = line
            .split_once("  ")
            .context("SHA256SUMS: malformed line")?;
        listed.insert(path.to_owned(), digest.to_ascii_lowercase());
    }
    ensure!(
        listed
            .insert("SHA256SUMS".into(), source_sha256.into())
            .is_none(),
        "SHA256SUMS lists itself"
    );
    let mut found = BTreeMap::new();
    let mut problems = Vec::new();
    walk(dir, "", &mut found, &mut problems)?;
    for path in found.keys().filter(|p| !listed.contains_key(*p)) {
        problems.push(format!("{path}: not listed in SHA256SUMS"));
    }
    for path in listed.keys().filter(|p| !found.contains_key(*p)) {
        problems.push(format!(
            "{path}: listed in SHA256SUMS but not a regular file"
        ));
    }
    if !problems.is_empty() {
        bail!(
            "refusing to pack {}:\n  {}",
            dir.display(),
            problems.join("\n  ")
        );
    }
    // BTreeMap order is byte order of the relative paths.
    Ok(listed
        .into_iter()
        .map(|(path, sha256)| Entry {
            size: found[&path],
            path,
            sha256,
            data: None,
        })
        .collect())
}

/// The archive files for `contents`. A prefill archive leaves out [`AUDIT_FILES`] and carries a
/// generated SHA256SUMS: the bundle's lines for the packed files, in their original order.
fn select(entries: Vec<Entry>, sums: &[u8], contents: Contents) -> Result<Vec<Entry>> {
    if contents == Contents::Full {
        return Ok(entries);
    }
    let sums = std::str::from_utf8(sums).context("SHA256SUMS is not UTF-8")?;
    let mut listing = String::new();
    for line in sums.lines() {
        let (_, path) = line
            .split_once("  ")
            .context("SHA256SUMS: malformed line")?;
        if !AUDIT_FILES.contains(&path) {
            listing.push_str(line);
            listing.push('\n');
        }
    }
    let listing = listing.into_bytes();
    Ok(entries
        .into_iter()
        .filter(|e| !AUDIT_FILES.contains(&e.path.as_str()))
        .map(|e| {
            if e.path == "SHA256SUMS" {
                Entry {
                    size: listing.len() as u64,
                    sha256: hex::encode(Sha256::digest(&listing)),
                    data: Some(listing.clone()),
                    path: e.path,
                }
            } else {
                e
            }
        })
        .collect())
}

/// Collects regular files (relative `/`-separated path -> size) below `dir/prefix`, without
/// following symlinks.
fn walk(
    dir: &Path,
    prefix: &str,
    found: &mut BTreeMap<String, u64>,
    problems: &mut Vec<String>,
) -> Result<()> {
    for item in fs::read_dir(dir.join(prefix))
        .with_context(|| format!("listing {}", dir.join(prefix).display()))?
    {
        let item = item?;
        let Ok(name) = item.file_name().into_string() else {
            problems.push(format!("{}: non-UTF-8 file name", item.path().display()));
            continue;
        };
        let path = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        let kind = item.file_type()?;
        if !path.bytes().all(|b| (b' '..=b'~').contains(&b)) {
            problems.push(format!("{path:?}: not a printable ASCII path"));
        } else if kind.is_dir() {
            walk(dir, &path, found, problems)?;
        } else if kind.is_file() {
            found.insert(path, item.metadata()?.len());
        } else if kind.is_symlink() {
            problems.push(format!("{path}: symlink"));
        } else {
            problems.push(format!("{path}: not a regular file"));
        }
    }
    Ok(())
}

/// Counts and hashes the compressed archive as it is written.
struct Hashing<W> {
    inner: W,
    digest: Sha256,
    bytes: u64,
}

impl<W: Write> Write for Hashing<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.digest.update(&buf[..n]);
        self.bytes += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// File bytes in archive order; `None` ends a file whose size and SHA-256 were confirmed.
type Chunk = Result<Option<Vec<u8>>>;

/// Streams the tar stream of `entries` through multithreaded zstd into `file`, which is synced.
/// A reader thread reads and re-hashes the files; returns the archive size and SHA-256.
fn write_archive(
    dir: &Path,
    entries: &[Entry],
    file: &File,
    level: i32,
    threads: u32,
    bundle_bytes: u64,
    start: Instant,
) -> Result<(u64, String)> {
    let output = Hashing {
        inner: BufWriter::with_capacity(CHUNK, file),
        digest: Sha256::new(),
        bytes: 0,
    };
    let mut zstd = zstd::stream::write::Encoder::new(output, level)?;
    // With at least one worker the frame depends only on the input and level, not on the
    // number of workers.
    zstd.multithread(threads)?;
    zstd.include_checksum(true)?;
    thread::scope(|scope| -> Result<()> {
        let (chunk_sender, chunks) = mpsc::sync_channel(BUFFERS);
        let (pool, pool_receiver) = mpsc::channel();
        scope.spawn(move || read_entries(dir, entries, chunk_sender, pool_receiver));
        // Returns the tar and compressed bytes written so far.
        let mut tar_bytes = 0;
        let mut write = |data: &[u8]| -> Result<(usize, u64)> {
            zstd.write_all(data).context("writing archive")?;
            tar_bytes += data.len();
            Ok((tar_bytes, zstd.get_ref().bytes))
        };
        let (mut data_bytes, packing, mut last) = (0u64, Instant::now(), Instant::now());
        for (done, entry) in entries.iter().enumerate() {
            write(&file_header(&entry.path, entry.size))?;
            while let Some(buffer) = chunks.recv().context("file reader stopped")?? {
                let (_, bytes_out) = write(&buffer)?;
                data_bytes += buffer.len() as u64;
                let _ = pool.send(buffer);
                if last.elapsed() >= PROGRESS_INTERVAL {
                    last = Instant::now();
                    let seconds = start.elapsed().as_secs_f64();
                    let rate = data_bytes as f64 / packing.elapsed().as_secs_f64();
                    eprintln!(
                        "{}",
                        json!({"phase": "pack", "files": done, "filesTotal": entries.len(), "bytesIn": data_bytes, "bundleBytes": bundle_bytes, "bytesOut": bytes_out, "mibPerSecond": rate / MIB, "etaSeconds": ((bundle_bytes - data_bytes) as f64 / rate).round(), "elapsedSeconds": seconds})
                    );
                }
            }
            write(&ZEROS[..(BLOCK - (entry.size % BLOCK as u64) as usize) % BLOCK])?;
        }
        // End of archive: two zero blocks, then padding to a whole record.
        let (tar_bytes, _) = write(&ZEROS[..2 * BLOCK])?;
        write(&ZEROS[..(RECORD - tar_bytes % RECORD) % RECORD])?;
        Ok(())
    })?;
    let Hashing {
        inner,
        digest,
        bytes,
    } = zstd.finish().context("finishing zstd stream")?;
    inner.into_inner().map_err(|e| e.into_error())?.sync_all()?;
    Ok((bytes, hex::encode(digest.finalize())))
}

/// Reader thread of [`write_archive`]: sends each file in at most [`BUFFERS`] recycled chunks
/// and checks that it still has its listed size and SHA-256.
fn read_entries(
    dir: &Path,
    entries: &[Entry],
    chunks: mpsc::SyncSender<Chunk>,
    pool: mpsc::Receiver<Vec<u8>>,
) {
    let mut allocated = 0;
    for entry in entries {
        let result = (|| -> Result<bool> {
            if let Some(data) = &entry.data {
                for chunk in data.chunks(CHUNK) {
                    if chunks.send(Ok(Some(chunk.to_vec()))).is_err() {
                        return Ok(false);
                    }
                }
                return Ok(chunks.send(Ok(None)).is_ok());
            }
            let mut file = File::open(dir.join(&entry.path))
                .with_context(|| format!("opening {}", entry.path))?;
            let meta = file.metadata()?;
            ensure!(
                meta.is_file() && meta.len() == entry.size,
                "{}: changed while packing",
                entry.path
            );
            let mut digest = Sha256::new();
            let mut remaining = entry.size;
            while remaining > 0 {
                let mut buffer = match pool.try_recv() {
                    Ok(buffer) => buffer,
                    Err(_) if allocated < BUFFERS => {
                        allocated += 1;
                        Vec::with_capacity(CHUNK)
                    }
                    // Only fails once the writer is gone.
                    Err(_) => match pool.recv() {
                        Ok(buffer) => buffer,
                        Err(_) => return Ok(false),
                    },
                };
                let n = remaining.min(CHUNK as u64) as usize;
                buffer.resize(n, 0);
                file.read_exact(&mut buffer)
                    .with_context(|| format!("{}: shrank while packing", entry.path))?;
                digest.update(&buffer);
                remaining -= n as u64;
                if chunks.send(Ok(Some(buffer))).is_err() {
                    return Ok(false);
                }
            }
            ensure!(
                file.read(&mut [0])? == 0,
                "{}: grew while packing",
                entry.path
            );
            ensure!(
                hex::encode(digest.finalize()) == entry.sha256,
                "{}: content no longer matches SHA256SUMS",
                entry.path
            );
            Ok(chunks.send(Ok(None)).is_ok())
        })();
        match result {
            Ok(true) => {}
            Ok(false) => return,
            Err(e) => {
                let _ = chunks.send(Err(e));
                return;
            }
        }
    }
}

/// Header block(s) of a regular file, byte-identical to Python's tarfile in PAX format for mode
/// 0644, uid/gid 0, empty owner names and mtime 0: a ustar header, preceded by a PAX extended
/// header with `path` / `size` records when the path exceeds 100 bytes or the size 8 GiB - 1.
fn file_header(path: &str, size: u64) -> Vec<u8> {
    let mut pax = String::new();
    if path.len() > 100 {
        pax_record(&mut pax, "path", path);
    }
    let large = size >= 1 << 33;
    if large {
        pax_record(&mut pax, "size", &size.to_string());
    }
    let mut out = Vec::with_capacity(4 * BLOCK);
    if !pax.is_empty() {
        out.extend(ustar_header("././@PaxHeader", 0, pax.len() as u64, b'x'));
        out.extend(pax.as_bytes());
        out.extend(&ZEROS[..(BLOCK - pax.len() % BLOCK) % BLOCK]);
    }
    // Like tarfile: the ustar name is truncated and the size zeroed when PAX carries them.
    out.extend(ustar_header(
        path,
        0o644,
        if large { 0 } else { size },
        b'0',
    ));
    out
}

/// A ustar header block; numeric fields in zero-padded octal, uid/gid/mtime 0, no owner names.
fn ustar_header(name: &str, mode: u64, size: u64, kind: u8) -> [u8; BLOCK] {
    let mut header = [0u8; BLOCK];
    let name = &name.as_bytes()[..name.len().min(100)];
    header[..name.len()].copy_from_slice(name);
    for (range, value) in [
        (100..108, mode),
        (108..116, 0), // uid
        (116..124, 0), // gid
        (124..136, size),
        (136..148, 0), // mtime
    ] {
        let digits = range.len() - 1;
        header[range.start..range.start + digits]
            .copy_from_slice(format!("{value:0digits$o}").as_bytes());
    }
    header[148..156].fill(b' ');
    header[156] = kind;
    header[257..265].copy_from_slice(b"ustar\x0000");
    let checksum: u32 = header.iter().map(|&b| u32::from(b)).sum();
    header[148..155].copy_from_slice(format!("{checksum:06o}\0").as_bytes());
    header
}

/// `"<length> <key>=<value>\n"`, where the length counts the whole record including itself.
fn pax_record(out: &mut String, key: &str, value: &str) {
    let base = key.len() + value.len() + 3;
    let mut length = 0;
    // Same fixed-point iteration as Python's tarfile.
    loop {
        let next = base + length.to_string().len();
        if next == length {
            break;
        }
        length = next;
    }
    let _ = writeln!(out, "{length} {key}={value}");
}

/// Streaming SHA-256 and size of a file, with progress lines labelled `phase`; `stop` cancels.
pub fn hash_file(path: &Path, phase: &str, stop: Option<&AtomicBool>) -> Result<(String, u64)> {
    let mut file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let total = file.metadata()?.len();
    let mut digest = Sha256::new();
    let mut buffer = vec![0u8; CHUNK];
    let mut bytes = 0u64;
    let (start, mut last) = (Instant::now(), Instant::now());
    loop {
        if stop.is_some_and(|s| s.load(Ordering::Relaxed)) {
            bail!("{phase}: cancelled");
        }
        let n = file
            .read(&mut buffer)
            .with_context(|| format!("reading {}", path.display()))?;
        if n == 0 {
            break;
        }
        digest.update(&buffer[..n]);
        bytes += n as u64;
        if last.elapsed() >= PROGRESS_INTERVAL {
            last = Instant::now();
            let seconds = start.elapsed().as_secs_f64();
            eprintln!(
                "{}",
                json!({"phase": phase, "bytes": bytes, "totalBytes": total, "elapsedSeconds": seconds, "mibPerSecond": bytes as f64 / MIB / seconds})
            );
        }
    }
    Ok((hex::encode(digest.finalize()), bytes))
}

/// Moves the finished archive into place without ever replacing an existing file.
fn publish(partial: &Path, output: &Path) -> Result<()> {
    match fs::hard_link(partial, output) {
        Ok(()) => fs::remove_file(partial)?,
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => bail!(
            "{} appeared while packing; refusing to overwrite it (new archive left at {})",
            output.display(),
            partial.display()
        ),
        // No hard links on this filesystem: rename after a final check.
        Err(_) => {
            ensure!(
                fs::symlink_metadata(output).is_err(),
                "{} appeared while packing; refusing to overwrite it",
                output.display()
            );
            fs::rename(partial, output)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cli, Command, ProcessArgs, Writer};
    use arkiv_entity_processor::Entity;
    use clap::Parser;

    #[test]
    fn headers_match_python_tarfile_pax_output() {
        // sha256 of tarfile.TarInfo(name, size=..., mode=0o644, mtime=0, uid/gid 0, no owner
        // names).tobuf(tarfile.PAX_FORMAT), CPython 3.12.
        for (path, size, expected) in [
            (
                "seed.json".to_owned(),
                1234,
                "15e9c7dc046db1bc59af9a57458a18e9002b8d0a2cf0f6c7ce5a6fab412e4688",
            ),
            (
                "entities/000001.jsonl".to_owned(),
                0,
                "d8bb125ed9b4b97c4b8a45daf74b2806a373d617e84e6277ccf4547c2fdb12ee",
            ),
            (
                format!("d/{}", "x".repeat(120)),
                9 << 30,
                "7f2e09f12b0ec1bf18fd819742a302470cbe179666051a0463aab2bd00c64734",
            ),
            (
                "payloads.bin".to_owned(),
                114_000_000_000,
                "7389cc4c541cf024e87634d435da53cb61a4f7cff5b6ac136a7184542167a6fa",
            ),
        ] {
            let header = file_header(&path, size);
            assert_eq!(header.len() % BLOCK, 0);
            assert_eq!(hex::encode(Sha256::digest(&header)), expected, "{path}");
        }
        let header = file_header("payloads.bin", 114_000_000_000);
        assert_eq!(&header[BLOCK..BLOCK + 22], b"21 size=114000000000\n\0");
    }

    #[test]
    fn pax_record_length_counts_itself() {
        for value_len in 0..2000 {
            let mut record = String::new();
            pax_record(&mut record, "path", &"p".repeat(value_len));
            let (length, _) = record.split_once(' ').unwrap();
            assert_eq!(length.parse::<usize>().unwrap(), record.len());
        }
    }

    fn bundle(name: &str) -> PathBuf {
        let a = ProcessArgs {
            input: "-".into(),
            output: std::env::temp_dir().join(format!("ep-pack-{}-{name}", std::process::id())),
            owner: "0x1111111111111111111111111111111111111111".into(),
            chain_id: 7706815,
            part_entities: 1,
            require_source_complete: false,
            limit: None,
            workers: 1,
            compression_workers: 1,
        };
        let _ = fs::remove_dir_all(&a.output);
        let mut w = Writer::new(&a).unwrap();
        for (kind, byte) in [("code", 1u8), ("blob", 2)] {
            w.write(Entity::new(
                kind,
                vec![byte; 5000],
                "application/octet-stream",
                serde_json::Map::new(),
            ))
            .unwrap();
        }
        w.finish(&a, 1, 1.).unwrap();
        a.output
    }

    fn args(dir: &Path, output: &Path, threads: u32) -> PackArgs {
        PackArgs {
            dir: dir.into(),
            output: output.into(),
            contents: Contents::Full,
            level: 3,
            threads: Some(threads),
            verify_existing: false,
        }
    }

    /// (path, mode, size, contents) of each entry of an uncompressed ustar stream.
    fn untar(tar: &[u8]) -> Vec<(String, String, u64, Vec<u8>)> {
        let mut out = Vec::new();
        let mut at = 0;
        while tar[at..at + BLOCK].iter().any(|&b| b != 0) {
            let h = &tar[at..at + BLOCK];
            let field = |r: std::ops::Range<usize>| {
                String::from_utf8(h[r].iter().copied().take_while(|&b| b != 0).collect()).unwrap()
            };
            let size = u64::from_str_radix(&field(124..136), 8).unwrap();
            assert_eq!(field(257..263), "ustar");
            assert_eq!((h[156], field(136..148).as_str()), (b'0', "00000000000"));
            let data = tar[at + BLOCK..at + BLOCK + size as usize].to_vec();
            out.push((field(0..100), field(100..108), size, data));
            at += BLOCK + (size as usize).div_ceil(BLOCK) * BLOCK;
        }
        assert_eq!(tar.len() % RECORD, 0);
        out
    }

    #[test]
    fn pack_is_reproducible_complete_and_never_overwrites() {
        let dir = bundle("reproducible");
        let out = dir.with_extension("out");
        let _ = fs::remove_dir_all(&out);
        fs::create_dir_all(&out).unwrap();
        let first = out.join("a.tar.zst");
        let second = out.join("b.tar.zst");
        let record = pack_record(&args(&dir, &first, 1)).unwrap();
        let other = pack_record(&args(&dir, &second, 4)).unwrap();
        let bytes = fs::read(&first).unwrap();
        assert_eq!(bytes, fs::read(&second).unwrap());
        assert_eq!(
            record["sha256"],
            hex::encode(Sha256::digest(&bytes)).as_str()
        );
        assert_eq!(record["sha256"], other["sha256"]);
        assert_eq!(record["bytes"], bytes.len());
        assert_eq!(record["archive"], "a.tar.zst");
        assert_eq!(
            (record["files"].as_u64(), record["zstdLevel"].as_i64()),
            (Some(7), Some(3))
        );
        assert_eq!(read_record(&record_path(&first)).unwrap(), record);
        assert!(!with_suffix(&first, ".partial").exists());

        // Every bundle file at the root, sorted, with deterministic metadata and its content.
        let entries = untar(&zstd::decode_all(&bytes[..]).unwrap());
        let names: Vec<_> = entries.iter().map(|e| e.0.as_str()).collect();
        assert_eq!(
            names,
            [
                "SHA256SUMS",
                "entities/000001.jsonl",
                "entities/000002.jsonl",
                "metadata.jsonl",
                "payloads.bin",
                "report.json",
                "seed.json"
            ]
        );
        for (name, mode, size, data) in &entries {
            assert_eq!(mode, "0000644");
            assert_eq!(*size, data.len() as u64);
            assert_eq!(data, &fs::read(dir.join(name)).unwrap(), "{name}");
        }
        let total: u64 = entries.iter().map(|e| e.2).sum();
        assert_eq!(record["bundleBytes"], total);
        assert_eq!(
            record["sourceSha256"],
            hex::encode(Sha256::digest(fs::read(dir.join("SHA256SUMS")).unwrap())).as_str()
        );

        // Existing archive: reused with a matching record, refused otherwise.
        let mut reuse = args(&dir, &first, 2);
        reuse.verify_existing = true;
        assert_eq!(pack_record(&reuse).unwrap(), record);
        let mut other_level = args(&dir, &first, 2);
        other_level.level += 1;
        let error = pack_record(&other_level).unwrap_err().to_string();
        assert!(error.contains("was packed with zstd level"), "{error}");
        fs::write(&first, b"tampered").unwrap();
        let error = pack_record(&reuse).unwrap_err().to_string();
        assert!(error.contains("refusing to overwrite"), "{error}");
        fs::remove_file(record_path(&first)).unwrap();
        assert!(pack_record(&args(&dir, &first, 1)).is_err());
        assert_eq!(fs::read(&first).unwrap(), b"tampered");

        // Unlisted files and symlinks are refused; nothing is published.
        let third = out.join("c.tar.zst");
        fs::write(dir.join("extra.txt"), b"x").unwrap();
        std::os::unix::fs::symlink("seed.json", dir.join("entities/link")).unwrap();
        let error = pack_record(&args(&dir, &third, 1)).unwrap_err().to_string();
        assert!(
            error.contains("extra.txt: not listed in SHA256SUMS"),
            "{error}"
        );
        assert!(error.contains("entities/link: symlink"), "{error}");
        assert!(!third.exists() && !with_suffix(&third, ".partial").exists());
        // Output inside the bundle and a wrong extension are refused up front.
        assert!(pack_record(&args(&dir, &dir.join("x.tar.zst"), 1)).is_err());
        assert!(pack_record(&args(&dir, &out.join("x.tar"), 1)).is_err());
        fs::remove_dir_all(&dir).unwrap();
        fs::remove_dir_all(&out).unwrap();
    }

    #[test]
    fn prefill_archive_leaves_out_audit_files_with_matching_sha256sums() {
        let dir = bundle("prefill");
        let out = dir.with_extension("out");
        let _ = fs::remove_dir_all(&out);
        fs::create_dir_all(&out).unwrap();
        let archive = out.join("p.tar.zst");
        let prefill = PackArgs {
            contents: Contents::Prefill,
            ..args(&dir, &archive, 2)
        };
        let record = pack_record(&prefill).unwrap();
        assert_eq!(
            (record["contents"].as_str(), record["files"].as_u64()),
            (Some("prefill"), Some(5))
        );
        let entries = untar(&zstd::decode_all(&fs::read(&archive).unwrap()[..]).unwrap());
        let names: Vec<_> = entries.iter().map(|e| e.0.as_str()).collect();
        assert_eq!(
            names,
            [
                "SHA256SUMS",
                "entities/000001.jsonl",
                "entities/000002.jsonl",
                "report.json",
                "seed.json"
            ]
        );
        // SHA256SUMS lists exactly the other packed files, with their contents' digests, in the
        // bundle's order; those files are the bundle's own.
        let sums = String::from_utf8(entries[0].3.clone()).unwrap();
        let bundle_sums = fs::read_to_string(dir.join("SHA256SUMS")).unwrap();
        let expected: Vec<_> = bundle_sums
            .lines()
            .filter(|l| !l.ends_with("  payloads.bin") && !l.ends_with("  metadata.jsonl"))
            .collect();
        assert_eq!(sums.lines().collect::<Vec<_>>(), expected);
        for (name, _, _, data) in &entries[1..] {
            assert_eq!(data, &fs::read(dir.join(name)).unwrap(), "{name}");
            let line = format!("{}  {name}", hex::encode(Sha256::digest(data)));
            assert!(sums.lines().any(|l| l == line), "{line}");
        }
        let total: u64 = entries.iter().map(|e| e.2).sum();
        assert_eq!(record["bundleBytes"], total);
        // The local bundle is untouched; asking for the other contents is refused, not reused.
        assert!(verify::verify(&dir).is_ok());
        let error = pack_record(&args(&dir, &archive, 2))
            .unwrap_err()
            .to_string();
        assert!(error.contains("holds --contents prefill"), "{error}");
        assert_eq!(pack_record(&prefill).unwrap(), record);
        fs::remove_dir_all(&dir).unwrap();
        fs::remove_dir_all(&out).unwrap();
    }

    #[test]
    fn pack_refuses_unverified_bundles() {
        let dir = bundle("unverified");
        let out = dir.with_extension("tar.zst");
        fs::write(dir.join("payloads.bin"), b"changed").unwrap();
        let error = format!("{:#}", pack_record(&args(&dir, &out, 1)).unwrap_err());
        assert!(error.contains("payloads.bin: checksum mismatch"), "{error}");
        assert!(!out.exists());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn pack_cli_defaults() {
        let cli = Cli::try_parse_from(["arkiv-entity-processor", "pack", "b", "-o", "x.tar.zst"])
            .unwrap();
        let Command::Pack(a) = cli.command else {
            panic!("pack expected");
        };
        assert_eq!(
            (a.contents, a.level, a.threads, a.verify_existing),
            (Contents::Prefill, 3, None, false)
        );
        assert!(Cli::try_parse_from(["arkiv-entity-processor", "pack", "b"]).is_err());
    }
}
