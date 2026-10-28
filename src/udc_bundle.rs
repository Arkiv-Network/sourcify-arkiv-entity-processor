//! v4 bundles: the Sourcify dataset as UDC entities (docs/udc-proposal.md), one entity per line
//! in creation order, with the keys a node assigns to salt-0 creates of the bundle's owner on the
//! bundle's chain from nonce 0 on.
//!
//! ```text
//! entities/NNNNNN.jsonl  {"nonce":N,"key":"0x…","expiresAt":"0xffffffffffffffff","attributes":[…]}
//! report.json            sourcify-udc-import.v1: counts, kinds, bytes, storage units, parts
//! seed.json              version 2, sourcify-arkiv.udc-bundle.v1: chain, owner, prefix, parts
//! SHA256SUMS             the parts, report.json and seed.json
//! ```
//!
//! A line's attributes are in name order, each `{name, type, indexed, value}` in the node's RPC
//! encoding except `bytes`, which is base64. Every key an entity names (key attributes,
//! `sourcerefs`, chunk lists) is the key of an earlier line, so a bundle is importable in order.
use crate::udc::{entity_key, Profile, UdcEntity, Val, Writer, CHUNK, PERMANENT};
use crate::{hx, sha, Entity};
use anyhow::{bail, ensure, Context, Result};
#[cfg(test)]
use base64::Engine as _;
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{self, File},
    io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
};

/// `seed.json` version of a v4 bundle (version 1 is the v3 payload bundle).
pub const SEED_VERSION: u64 = 2;
pub const FORMAT: &str = "sourcify-arkiv.udc-bundle.v1";
pub const REPORT_SCHEMA: &str = "sourcify-udc-import.v1";
/// Storage units of an entity without user attributes (arkiv-state-db-experiment's
/// `EMPTY_ENTITY_UNITS`, confirmed by a receipt's rentFee).
pub const SYSTEM_UNITS: u64 = 1460;

/// Writes a v4 bundle: `record` takes one contract's v3 entities (`crate::transform` output, in
/// input order); `finish` publishes report.json, SHA256SUMS and, last, seed.json.
pub struct BundleWriter {
    dir: PathBuf,
    profile: Profile,
    writer: Writer,
    part_size: usize,
    part: Option<BufWriter<File>>,
    part_paths: Vec<String>,
    part_hash: Sha256,
    part_bytes: u64,
    part_entities: u64,
    part_first_nonce: u64,
    part_audit: Vec<Value>,
    pub entities: u64,
    pub contracts: u64,
    kinds: BTreeMap<String, u64>,
    bytes_values: u64,
    storage_units: u64,
    addresses: HashSet<[u8; 20]>,
}
impl BundleWriter {
    pub fn new(dir: &Path, profile: Profile, part_size: usize) -> Result<Self> {
        crate::udc::check_prefix(&profile.prefix)?;
        ensure!(part_size > 0, "part-size must be positive");
        ensure!(
            !dir.exists(),
            "output already exists; use a fresh directory (bundles are never resumed)"
        );
        fs::create_dir_all(dir.join("entities"))?;
        Ok(Self {
            dir: dir.to_owned(),
            writer: Writer::new(profile.clone(), 0),
            profile,
            part_size,
            part: None,
            part_paths: vec![],
            part_hash: Sha256::new(),
            part_bytes: 0,
            part_entities: 0,
            part_first_nonce: 0,
            part_audit: vec![],
            entities: 0,
            contracts: 0,
            kinds: BTreeMap::new(),
            bytes_values: 0,
            storage_units: 0,
            addresses: HashSet::new(),
        })
    }
    /// Distinct (sources, codes, compilations) so far.
    pub fn distinct(&self) -> (usize, usize, usize) {
        self.writer.distinct()
    }
    pub fn record(&mut self, v3: &[Entity]) -> Result<()> {
        let contract = v3
            .iter()
            .find(|e| e.kind == "verified_contract")
            .context("record without a verified contract")?;
        let address = contract.attributes["address"]["value"]
            .as_str()
            .context("contract address")?;
        let address: [u8; 20] = hex::decode(address.trim_start_matches("0x"))?
            .try_into()
            .map_err(|_| anyhow::anyhow!("invalid contract address"))?;
        // One contract entity per address: a second one would be ambiguous to every reader.
        ensure!(
            !self.addresses.contains(&address),
            "a second record for contract {}",
            hx(address)
        );
        let created = self.writer.contract(v3)?;
        self.addresses.insert(address);
        for e in created {
            self.write(&e)?;
        }
        self.contracts += 1;
        Ok(())
    }
    fn write(&mut self, e: &UdcEntity) -> Result<()> {
        let nonce = self.entities;
        ensure!(
            e.key == entity_key(self.profile.chain_id, &self.profile.owner, nonce, 0),
            "entity key does not follow the bundle's nonces"
        );
        if self.entities.is_multiple_of(self.part_size as u64) {
            self.close_part()?;
            let path = format!("entities/{:06}.jsonl", self.part_paths.len() + 1);
            self.part = Some(BufWriter::new(File::create(self.dir.join(&path))?));
            self.part_paths.push(path);
            self.part_hash = Sha256::new();
            self.part_bytes = 0;
            self.part_entities = 0;
            self.part_first_nonce = nonce;
        }
        let mut line = serde_json::to_vec(&e.to_line(nonce))?;
        line.push(b'\n');
        self.part.as_mut().unwrap().write_all(&line)?;
        self.part_hash.update(&line);
        self.part_bytes += line.len() as u64;
        self.part_entities += 1;
        self.entities += 1;
        let kind = e
            .kind(&self.profile.prefix)
            .context("entity of an unknown kind")?;
        *self.kinds.entry(kind.into()).or_default() += 1;
        self.bytes_values += e.bytes_len() as u64;
        self.storage_units += e.user_units() + SYSTEM_UNITS;
        Ok(())
    }
    fn close_part(&mut self) -> Result<()> {
        if let Some(mut f) = self.part.take() {
            f.flush()?;
            f.get_ref().sync_all()?;
            self.part_audit.push(json!({
                "path": self.part_paths.last(),
                "bytes": self.part_bytes,
                "sha256": hex::encode(self.part_hash.clone().finalize()),
                "entities": self.part_entities,
                "firstNonce": self.part_first_nonce,
            }));
        }
        Ok(())
    }
    /// Publishes the bundle; `extra` adds run fields (timings, workers, source policy) to the
    /// report. Returns the report.
    pub fn finish(mut self, records: u64, extra: Map<String, Value>) -> Result<Value> {
        ensure!(self.entities > 0, "empty bundle");
        self.close_part()?;
        let (sources, codes, compilations) = self.writer.distinct();
        let mut report = json!({
            "schema": REPORT_SCHEMA,
            "status": "completed",
            "format": FORMAT,
            "sourceChainId": 1,
            "arkivChainId": self.profile.chain_id,
            "owner": hx(self.profile.owner),
            "schemaPrefix": self.profile.prefix,
            "records": records,
            "contracts": self.contracts,
            "entities": self.entities,
            "kinds": self.kinds,
            "distinct": {"sources": sources, "codes": codes, "compilations": compilations},
            "bytesValues": self.bytes_values,
            "storageUnits": self.storage_units,
            "expiry": "permanent (expiresAt 2^64-1)",
            "parts": self.part_audit,
        });
        report.as_object_mut().unwrap().extend(extra);
        write_json(&self.dir.join("report.json"), &report)?;
        let seed = json!({
            "version": SEED_VERSION,
            "format": FORMAT,
            "chainId": self.profile.chain_id,
            "owner": hx(self.profile.owner),
            "schemaPrefix": self.profile.prefix,
            "firstNonce": 0,
            "entities": self.part_paths.iter().map(|p| json!({"path": p})).collect::<Vec<_>>(),
        });
        write_json(&self.dir.join("seed.json.tmp"), &seed)?;
        let mut sums = String::new();
        for path in self
            .part_paths
            .iter()
            .map(String::as_str)
            .chain(["report.json", "seed.json"])
        {
            let file = if path == "seed.json" {
                "seed.json.tmp"
            } else {
                path
            };
            let (digest, _) = sha256_file(&self.dir.join(file))?;
            sums.push_str(&format!("{digest}  {path}\n"));
        }
        let mut checksums = File::create(self.dir.join("SHA256SUMS"))?;
        checksums.write_all(sums.as_bytes())?;
        checksums.sync_all()?;
        fs::rename(self.dir.join("seed.json.tmp"), self.dir.join("seed.json"))?;
        File::open(&self.dir)?.sync_all()?;
        Ok(report)
    }
}
fn write_json(path: &Path, v: &Value) -> Result<()> {
    let mut f = File::create(path)?;
    serde_json::to_writer_pretty(&mut f, v)?;
    f.write_all(b"\n")?;
    f.sync_all()?;
    Ok(())
}
/// Lowercase hex SHA-256 and size of a file, streamed.
pub fn sha256_file(path: &Path) -> Result<(String, u64)> {
    let mut file = File::open(path).with_context(|| path.display().to_string())?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0u8; 1 << 16];
    let mut size = 0;
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        digest.update(&buffer[..n]);
        size += n as u64;
    }
    Ok((hex::encode(digest.finalize()), size))
}

/// What seed.json and report.json of a v4 bundle say.
#[derive(Clone, Debug)]
pub struct Manifest {
    pub chain_id: u64,
    pub owner: [u8; 20],
    pub prefix: String,
    pub first_nonce: u64,
    pub parts: Vec<Part>,
    pub entities: u64,
    pub contracts: u64,
    pub kinds: BTreeMap<String, u64>,
    /// report.json's totals: distinct (sources, codes, compilations), bytes values, storage units.
    pub distinct: (u64, u64, u64),
    pub bytes_values: u64,
    pub storage_units: u64,
}
#[derive(Clone, Debug)]
pub struct Part {
    pub path: String,
    pub sha256: String,
    pub bytes: u64,
    pub entities: u64,
    pub first_nonce: u64,
}
fn safe_path(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty()
            && Path::new(path)
                .components()
                .all(|p| matches!(p, Component::Normal(_))),
        "unsafe bundle path {path:?}"
    );
    Ok(())
}
fn address(value: &Value) -> Result<[u8; 20]> {
    let text = value.as_str().context("owner must be a string")?;
    ensure!(
        text.starts_with("0x") && text.len() == 42,
        "invalid owner {text}"
    );
    hex::decode(&text[2..])?
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid owner"))
}
/// The SHA256SUMS listing (path -> digest).
pub fn read_sums(dir: &Path) -> Result<HashMap<String, String>> {
    let text = fs::read_to_string(dir.join("SHA256SUMS")).context("SHA256SUMS")?;
    let mut sums = HashMap::new();
    for line in text.lines() {
        let (digest, path) = line.split_once("  ").context("malformed SHA256SUMS")?;
        safe_path(path)?;
        ensure!(
            digest.len() == 64 && hex::decode(digest).is_ok(),
            "invalid digest in SHA256SUMS"
        );
        ensure!(
            sums.insert(path.to_owned(), digest.to_ascii_lowercase())
                .is_none(),
            "{path} listed twice in SHA256SUMS"
        );
    }
    Ok(sums)
}
/// seed.json and report.json, checked against SHA256SUMS and each other.
pub fn manifest(dir: &Path, sums: &HashMap<String, String>) -> Result<Manifest> {
    let checked = |name: &str| -> Result<Value> {
        let expected = sums
            .get(name)
            .with_context(|| format!("{name} not listed in SHA256SUMS"))?;
        let bytes = fs::read(dir.join(name)).with_context(|| name.to_string())?;
        ensure!(
            &hex::encode(sha(&bytes)) == expected,
            "{name}: checksum mismatch"
        );
        serde_json::from_slice(&bytes).with_context(|| format!("invalid {name}"))
    };
    let seed = checked("seed.json")?;
    let report = checked("report.json")?;
    ensure!(
        seed["version"] == SEED_VERSION && seed["format"] == FORMAT,
        "seed.json is not a {FORMAT} manifest"
    );
    ensure!(
        report["schema"] == REPORT_SCHEMA && report["status"] == "completed",
        "report.json is not a completed {REPORT_SCHEMA} report"
    );
    let chain_id = seed["chainId"].as_u64().context("seed chainId")?;
    let owner = address(&seed["owner"])?;
    let prefix = seed["schemaPrefix"]
        .as_str()
        .context("seed schemaPrefix")?
        .to_owned();
    crate::udc::check_prefix(&prefix)?;
    ensure!(
        report["arkivChainId"] == chain_id
            && address(&report["owner"])? == owner
            && report["schemaPrefix"] == prefix.as_str(),
        "report.json describes another chain, owner or prefix than seed.json"
    );
    let first_nonce = seed["firstNonce"].as_u64().context("seed firstNonce")?;
    // A bundle's keys are those of its owner's first creates on a fresh chain.
    ensure!(first_nonce == 0, "seed.json firstNonce must be 0");
    let listed = seed["entities"].as_array().context("seed entities")?;
    let audits = report["parts"].as_array().context("report parts")?;
    ensure!(
        listed.len() == audits.len(),
        "seed.json and report.json list different parts"
    );
    let mut parts = vec![];
    let mut next = first_nonce;
    for (entry, audit) in listed.iter().zip(audits) {
        let path = entry["path"].as_str().context("part path")?.to_owned();
        safe_path(&path)?;
        ensure!(audit["path"] == path.as_str(), "report part order differs");
        let sha256 = audit["sha256"].as_str().context("part sha256")?.to_owned();
        ensure!(
            sums.get(&path) == Some(&sha256),
            "{path}: report checksum differs from SHA256SUMS"
        );
        let part = Part {
            path,
            sha256,
            bytes: audit["bytes"].as_u64().context("part bytes")?,
            entities: audit["entities"].as_u64().context("part entities")?,
            first_nonce: audit["firstNonce"].as_u64().context("part firstNonce")?,
        };
        ensure!(
            part.first_nonce == next,
            "{}: nonces do not continue",
            part.path
        );
        next += part.entities;
        parts.push(part);
    }
    let entities = report["entities"].as_u64().context("report entities")?;
    ensure!(
        next - first_nonce == entities,
        "parts hold another number of entities than the report"
    );
    let kinds: BTreeMap<String, u64> = serde_json::from_value(report["kinds"].clone())?;
    Ok(Manifest {
        chain_id,
        owner,
        prefix,
        first_nonce,
        parts,
        entities,
        contracts: report["contracts"].as_u64().context("report contracts")?,
        kinds,
        distinct: (
            report["distinct"]["sources"]
                .as_u64()
                .context("report distinct")?,
            report["distinct"]["codes"]
                .as_u64()
                .context("report distinct")?,
            report["distinct"]["compilations"]
                .as_u64()
                .context("report distinct")?,
        ),
        bytes_values: report["bytesValues"]
            .as_u64()
            .context("report bytesValues")?,
        storage_units: report["storageUnits"]
            .as_u64()
            .context("report storageUnits")?,
    })
}

/// Lines of the parts, in order, with each part's digest and size checked at its end.
fn lines(
    dir: &Path,
    m: &Manifest,
    mut each: impl FnMut(usize, u64, &[u8], u64) -> Result<()>,
) -> Result<()> {
    let mut nonce = m.first_nonce;
    for (file, part) in m.parts.iter().enumerate() {
        let mut reader =
            BufReader::new(File::open(dir.join(&part.path)).with_context(|| part.path.clone())?);
        let (mut offset, mut count) = (0u64, 0u64);
        let mut digest = Sha256::new();
        let mut line = Vec::new();
        loop {
            line.clear();
            let n = reader.read_until(b'\n', &mut line)?;
            if n == 0 {
                break;
            }
            digest.update(&line);
            ensure!(
                line.ends_with(b"\n"),
                "{}: last line unterminated",
                part.path
            );
            each(file, offset, &line, nonce)
                .with_context(|| format!("{} line {}", part.path, count + 1))?;
            offset += n as u64;
            count += 1;
            nonce += 1;
        }
        ensure!(
            offset == part.bytes && hex::encode(digest.finalize()) == part.sha256,
            "{}: checksum mismatch",
            part.path
        );
        ensure!(
            count == part.entities,
            "{}: entity count differs",
            part.path
        );
    }
    Ok(())
}
fn parse(m: &Manifest, line: &[u8], nonce: u64) -> Result<UdcEntity> {
    let value: Value = serde_json::from_slice(line).context("invalid JSON")?;
    let (n, e) = UdcEntity::from_line(&value, m.owner)?;
    ensure!(n == nonce, "nonce {n}, expected {nonce}");
    ensure!(
        e.key == entity_key(m.chain_id, &m.owner, nonce, 0),
        "key is not the salt-0 key of nonce {nonce}"
    );
    Ok(e)
}

/// Counts of a verified v4 bundle.
#[derive(Debug, Default)]
pub struct Checked {
    pub entities: u64,
    pub contracts: u64,
    pub references: u64,
    pub kinds: BTreeMap<String, u64>,
}
const KIND_IDS: [&str; 5] = crate::udc::KINDS;
/// The structural check of `verify`: every line in nonce order with its salt-0 key, permanent,
/// within the node's rules and the format's schema, every bytes value with a known codec; every
/// key it names an earlier line of the right kind, a contract's compilation and code with the
/// hashes it names; chunk lists as long as their chunks; one contract per address and per
/// `matchid`, one entity per content hash; counts and totals as reported. Values are not
/// decompressed (`hydrate` checks content).
pub fn check(dir: &Path, m: &Manifest) -> Result<Checked> {
    // key -> (kind, chunk data length)
    let mut seen: HashMap<[u8; 32], (u8, u32)> = HashMap::new();
    // compilation and code key -> content hash
    let mut content: HashMap<[u8; 32], [u8; 32]> = HashMap::new();
    let mut hashes: HashSet<(u8, [u8; 32])> = HashSet::new();
    let mut addresses: HashSet<[u8; 20]> = HashSet::new();
    let mut matchids: HashSet<u64> = HashSet::new();
    let (mut bytes_values, mut storage_units) = (0u64, 0u64);
    let mut out = Checked::default();
    lines(dir, m, |_, _, line, nonce| {
        let e = parse(m, line, nonce)?;
        ensure!(e.expires_at == PERMANENT, "entity is not permanent");
        let kind = e.check_schema(&m.prefix)?;
        let id = KIND_IDS.iter().position(|k| *k == kind).unwrap() as u8;
        for a in &e.attributes {
            if let Val::Bytes(b) = &a.value {
                ensure!(!b.is_empty(), "empty bytes value {}", a.name);
                ensure!(
                    kind == "chunk" || b[0] <= 2,
                    "bytes value {} has unknown codec {}",
                    a.name,
                    b[0]
                );
            }
        }
        bytes_values += e.bytes_len() as u64;
        storage_units += e.user_units() + SYSTEM_UNITS;
        for (key, target) in e.references()? {
            let (found, _) = seen
                .get(&key)
                .with_context(|| format!("{} names {} before it exists", kind, hx(key)))?;
            ensure!(
                KIND_IDS[*found as usize] == target,
                "{kind} names {} as a {target}, but it is a {}",
                hx(key),
                KIND_IDS[*found as usize]
            );
            out.references += 1;
        }
        if kind == "contract" {
            for (attr, hash_attr) in [
                ("compilationref", "compilationhash"),
                ("creationcoderef", "creationcodehash"),
                ("runtimecoderef", "runtimecodehash"),
            ] {
                if let Some(Val::Key(k)) = e.get(attr) {
                    let named = match e.get(hash_attr) {
                        Some(Val::Bytes32(h)) => *h,
                        _ => bail!("contract has {attr} without {hash_attr}"),
                    };
                    ensure!(
                        content.get(k) == Some(&named),
                        "contract's {attr} names an entity without its {hash_attr}"
                    );
                }
            }
            let Some(Val::U64(matchid)) = e.get("matchid") else {
                bail!("contract without matchid")
            };
            ensure!(
                matchids.insert(*matchid),
                "two contracts with matchid {matchid}"
            );
        }
        for a in &e.attributes {
            if let Val::Bytes(b) = &a.value {
                if kind != "chunk" && b.first() == Some(&2) {
                    let len = u32::from_be_bytes(b[33..37].try_into()?) as u64;
                    let total: u64 = b[37..]
                        .chunks(32)
                        .map(|k| seen[<&[u8; 32]>::try_from(k).unwrap()].1 as u64)
                        .sum();
                    ensure!(total == len, "chunk list of {} does not add up", a.name);
                }
            }
        }
        let chunk_len = match (kind, e.get("data")) {
            ("chunk", Some(Val::Bytes(b))) => {
                ensure!(!b.is_empty() && b.len() <= CHUNK, "chunk size out of range");
                b.len() as u32
            }
            _ => 0,
        };
        if let Some(Val::Bytes32(h)) = e.get("hash") {
            ensure!(
                hashes.insert((id, *h)),
                "two {kind} entities with hash {}",
                hx(h)
            );
            if kind == "compilation" || kind == "code" {
                content.insert(e.key, *h);
            }
        }
        if kind == "contract" {
            let Some(Val::Addr(a)) = e.get("address") else {
                bail!("contract without address")
            };
            ensure!(addresses.insert(*a), "two contracts for {}", hx(a));
            ensure!(
                e.get("chainid") == Some(&Val::U64(1)),
                "contract of another source chain"
            );
            out.contracts += 1;
        }
        ensure!(
            seen.insert(e.key, (id, chunk_len)).is_none(),
            "duplicate key"
        );
        *out.kinds.entry(kind.into()).or_default() += 1;
        out.entities += 1;
        Ok(())
    })?;
    ensure!(
        out.entities == m.entities && out.kinds == m.kinds && out.contracts == m.contracts,
        "entity, kind or contract counts differ from report.json"
    );
    let kind = |k: &str| out.kinds.get(k).copied().unwrap_or(0);
    ensure!(
        m.distinct == (kind("source"), kind("code"), kind("compilation"))
            && m.bytes_values == bytes_values
            && m.storage_units == storage_units,
        "distinct counts, bytes values or storage units differ from report.json"
    );
    Ok(out)
}

#[derive(Clone, Copy)]
struct Location {
    file: usize,
    offset: u64,
    length: usize,
    digest: [u8; 32],
}
/// Indexed read access to a v4 bundle (for `hydrate` and tests): entities by key, contracts in
/// bundle order. Opening checks the manifest and every part's digest; reads check each line's.
pub struct UdcBundle {
    pub manifest: Manifest,
    files: Vec<PathBuf>,
    index: HashMap<[u8; 32], Location>,
    contracts: Vec<[u8; 32]>,
    by_address: HashMap<[u8; 20], [u8; 32]>,
}
impl UdcBundle {
    pub fn open(dir: &Path) -> Result<Self> {
        let sums = read_sums(dir)?;
        let manifest = manifest(dir, &sums)?;
        let mut index = HashMap::new();
        let mut contracts = vec![];
        let mut by_address = HashMap::new();
        let tag = format!("{}.contract", manifest.prefix);
        lines(dir, &manifest, |file, offset, line, nonce| {
            let e = parse(&manifest, line, nonce)?;
            index.insert(
                e.key,
                Location {
                    file,
                    offset,
                    length: line.len(),
                    digest: sha(line),
                },
            );
            if e.schema() == Some(tag.as_str()) {
                if let Some(Val::Addr(a)) = e.get("address") {
                    ensure!(
                        by_address.insert(*a, e.key).is_none(),
                        "two contracts for {}",
                        hx(a)
                    );
                }
                contracts.push(e.key);
            }
            Ok(())
        })?;
        Ok(Self {
            files: manifest.parts.iter().map(|p| dir.join(&p.path)).collect(),
            manifest,
            index,
            contracts,
            by_address,
        })
    }
    pub fn entity(&self, key: &[u8; 32]) -> Result<UdcEntity> {
        let at = self
            .index
            .get(key)
            .with_context(|| format!("entity {} not in the bundle", hx(key)))?;
        let mut file = File::open(&self.files[at.file])?;
        file.seek(SeekFrom::Start(at.offset))?;
        let mut line = vec![0; at.length];
        file.read_exact(&mut line)?;
        ensure!(
            sha(&line) == at.digest,
            "entity bytes changed after indexing"
        );
        let value: Value = serde_json::from_slice(&line)?;
        Ok(UdcEntity::from_line(&value, self.manifest.owner)?.1)
    }
    /// Contract keys in bundle order.
    pub fn contracts(&self) -> &[[u8; 32]] {
        &self.contracts
    }
    pub fn find_contract(&self, address: &[u8; 20]) -> Result<Option<UdcEntity>> {
        self.by_address
            .get(address)
            .map(|k| self.entity(k))
            .transpose()
    }
}

/// The seed.json version of a bundle directory (1: v3 payload bundle, 2: v4 UDC bundle).
pub fn seed_version(dir: &Path) -> Result<u64> {
    let seed: Value =
        serde_json::from_slice(&fs::read(dir.join("seed.json")).context("seed.json")?)
            .context("invalid seed.json")?;
    seed["version"].as_u64().context("seed.json has no version")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transform;
    use serde_json::json;

    const OWNER: [u8; 20] = [0x11; 20];
    fn record(i: u64) -> Value {
        json!({"chainId":"1","address":format!("0x{:040x}", i),"matchId":i.to_string(),"verifiedAt":"2020-01-01T00:00:00Z",
            "abi":[],"compilation":{"language":"Solidity","compiler":"solc","compilerVersion":"1","compilerSettings":{"optimizer":{"enabled":true,"runs":200}},"fullyQualifiedName":"A.sol:A","name":"A"},
            "sources":{"A.sol":{"content":format!("contract A{} {{}}", i % 2)}},
            "creationBytecode":{"onchainBytecode":"0x6001","recompiledBytecode":"0x6001"},
            "runtimeBytecode":{"onchainBytecode":"0x6000","recompiledBytecode":"0x6000"},
            "deployment":{"deployer":"0x2222222222222222222222222222222222222222","blockNumber":"7"}})
    }
    fn bundle(name: &str, records: u64) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("udc-bundle-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let profile = Profile {
            prefix: "sourcify.v4".into(),
            chain_id: 7706815,
            owner: OWNER,
        };
        let mut w = BundleWriter::new(&dir, profile, 4).unwrap();
        for i in 1..=records {
            w.record(&transform(&record(i)).unwrap()).unwrap();
        }
        w.finish(records, Map::new()).unwrap();
        dir
    }
    fn checked(dir: &Path) -> Result<Checked> {
        let sums = read_sums(dir)?;
        check(dir, &manifest(dir, &sums)?)
    }
    /// Rewrites a bundle's lines (as JSON values) and re-signs it: parts, report.json, seed.json
    /// and SHA256SUMS agree again, so only the structural check can object.
    fn rewrite(dir: &Path, edit: impl Fn(&mut Vec<Value>), report_edit: impl Fn(&mut Value)) {
        let mut seed: Value =
            serde_json::from_slice(&fs::read(dir.join("seed.json")).unwrap()).unwrap();
        let mut report: Value =
            serde_json::from_slice(&fs::read(dir.join("report.json")).unwrap()).unwrap();
        let mut lines: Vec<Value> = vec![];
        for p in seed["entities"].as_array().unwrap() {
            let text = fs::read_to_string(dir.join(p["path"].as_str().unwrap())).unwrap();
            lines.extend(
                text.lines()
                    .map(|l| serde_json::from_str::<Value>(l).unwrap()),
            );
            fs::remove_file(dir.join(p["path"].as_str().unwrap())).unwrap();
        }
        edit(&mut lines);
        let mut body = Vec::new();
        for l in &lines {
            body.extend(serde_json::to_vec(l).unwrap());
            body.push(b'\n');
        }
        fs::write(dir.join("entities/000001.jsonl"), &body).unwrap();
        seed["entities"] = json!([{"path": "entities/000001.jsonl"}]);
        report["parts"] = json!([{"path": "entities/000001.jsonl", "bytes": body.len(), "sha256": hex::encode(sha(&body)), "entities": lines.len(), "firstNonce": 0}]);
        report["entities"] = json!(lines.len());
        report_edit(&mut report);
        fs::write(dir.join("seed.json"), serde_json::to_vec(&seed).unwrap()).unwrap();
        fs::write(
            dir.join("report.json"),
            serde_json::to_vec(&report).unwrap(),
        )
        .unwrap();
        let mut sums = format!("{}  entities/000001.jsonl\n", hex::encode(sha(&body)));
        for f in ["report.json", "seed.json"] {
            sums.push_str(&format!(
                "{}  {f}\n",
                hex::encode(sha(fs::read(dir.join(f)).unwrap()))
            ));
        }
        fs::write(dir.join("SHA256SUMS"), sums).unwrap();
    }
    fn attr<'a>(line: &'a mut Value, name: &str) -> &'a mut Value {
        line["attributes"]
            .as_array_mut()
            .unwrap()
            .iter_mut()
            .find(|a| a["name"] == name)
            .unwrap()
    }
    fn kind(line: &Value) -> String {
        line["attributes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|a| a["name"] == "arkiv.schema")
            .unwrap()["value"]
            .as_str()
            .unwrap()
            .rsplit('.')
            .next()
            .unwrap()
            .into()
    }

    #[test]
    fn a_written_bundle_checks_and_reads_back() {
        let dir = bundle("ok", 3);
        let c = checked(&dir).unwrap();
        assert_eq!(c.contracts, 3);
        assert!(c.references > 0);
        let b = UdcBundle::open(&dir).unwrap();
        assert_eq!(b.contracts().len(), 3);
        let contract = b
            .find_contract(
                &hex::decode(format!("{:040x}", 2))
                    .unwrap()
                    .try_into()
                    .unwrap(),
            )
            .unwrap()
            .unwrap();
        let mut fetch = |k: &[u8; 32]| b.entity(k);
        let record = crate::udc::Reader::new("sourcify.v4", Some(OWNER), &mut fetch)
            .hydrate(&contract, None)
            .unwrap();
        assert_eq!(record, crate::canonical(&record_value(2)));
        // An unchanged rewrite still checks (the helper itself is sound).
        rewrite(&dir, |_| {}, |_| {});
        checked(&dir).unwrap();
        fs::remove_dir_all(dir).unwrap();
    }
    fn record_value(i: u64) -> Value {
        record(i)
    }

    #[test]
    fn the_writer_refuses_a_second_record_for_an_address() {
        let dir = std::env::temp_dir().join(format!("udc-bundle-test-{}-dup", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        let profile = Profile {
            prefix: "sourcify.v4".into(),
            chain_id: 7706815,
            owner: OWNER,
        };
        let mut w = BundleWriter::new(&dir, profile, 4).unwrap();
        w.record(&transform(&record(1)).unwrap()).unwrap();
        let mut again = record(1);
        again["matchId"] = json!("9");
        assert!(w
            .record(&transform(&again).unwrap())
            .unwrap_err()
            .to_string()
            .contains("second record"));
        drop(w);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn tampered_bundles_fail_the_check() {
        type Edit = Box<dyn Fn(&mut Vec<Value>)>;
        let cases: Vec<(&str, Edit, &str)> = vec![
            (
                "forward reference",
                Box::new(|l: &mut Vec<Value>| {
                    // Move the first contract before its dependencies, renumbering nonces and keys.
                    let i = l.iter().position(|x| kind(x) == "contract").unwrap();
                    let c = l.remove(i);
                    l.insert(0, c);
                }),
                "nonce",
            ),
            (
                "not permanent",
                Box::new(|l: &mut Vec<Value>| l[0]["expiresAt"] = json!("0x10")),
                "permanent",
            ),
            (
                "wrong kind",
                Box::new(|l: &mut Vec<Value>| {
                    let source = l.iter().find(|x| kind(x) == "source").unwrap()["key"].clone();
                    let c = l.iter_mut().find(|x| kind(x) == "contract").unwrap();
                    attr(c, "compilationref")["value"] = source;
                }),
                "as a compilation",
            ),
            (
                "unknown codec",
                Box::new(|l: &mut Vec<Value>| {
                    let s = l.iter_mut().find(|x| kind(x) == "source").unwrap();
                    attr(s, "content")["value"] =
                        json!(base64::engine::general_purpose::STANDARD.encode([3u8, 1, 2]));
                }),
                "codec",
            ),
            (
                "compilationhash",
                Box::new(|l: &mut Vec<Value>| {
                    let c = l.iter_mut().find(|x| kind(x) == "contract").unwrap();
                    attr(c, "compilationhash")["value"] = json!(format!("0x{}", "ab".repeat(32)));
                }),
                "compilationhash",
            ),
            (
                "chunked sourcerefs",
                Box::new(|l: &mut Vec<Value>| {
                    let c = l.iter_mut().find(|x| kind(x) == "compilation").unwrap();
                    let mut list = vec![2u8];
                    list.extend_from_slice(&[0; 36]);
                    list.extend_from_slice(&[7; 32]);
                    attr(c, "sourcerefs")["value"] =
                        json!(base64::engine::general_purpose::STANDARD.encode(list));
                }),
                "sourcerefs",
            ),
        ];
        for (name, edit, message) in cases {
            let dir = bundle(&name.replace(' ', "-"), 2);
            rewrite(&dir, edit, |_| {});
            let error = format!("{:#}", checked(&dir).unwrap_err());
            assert!(error.contains(message), "{name}: {error}");
            fs::remove_dir_all(dir).unwrap();
        }
        let dir = bundle("totals", 2);
        rewrite(&dir, |_| {}, |r| r["storageUnits"] = json!(1));
        assert!(checked(&dir)
            .unwrap_err()
            .to_string()
            .contains("storage units"));
        fs::remove_dir_all(dir).unwrap();
    }
}
