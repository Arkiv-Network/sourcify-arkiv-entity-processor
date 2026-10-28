//! `verify`: re-checks a finished bundle written by `process`.
use anyhow::{bail, Context, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fmt,
    fs::{self, File},
    io::Read,
    path::{Component, Path},
};

pub const LONG_ABOUT: &str = "\
Verify a finished bundle written by `process`, of either format (seed.json version 1 or 2).

Checks that every file listed in SHA256SUMS exists and matches its SHA-256 digest, that
SHA256SUMS covers report.json, seed.json and every entity part named in seed.json (and, for a
v3 bundle, payloads.bin and metadata.jsonl), that report.json has \"status\": \"completed\", and
that seed.json exists.

A v4 (UDC) bundle is also checked line by line: nonces from seed.json's firstNonce in order, each
key the salt-0 key of the bundle's chain, owner and nonce, every entity permanent and within the
node's rules and the format's schema, every key an entity names (key attributes, sourcerefs,
chunk lists) an earlier line of the right kind, chunk lists as long as their chunks, one contract
per address and one entity per content hash, and the counts report.json gives. Values are not
decompressed; `hydrate` checks content.

Exits non-zero and lists every problem found otherwise.";

/// Files every completed bundle lists in SHA256SUMS, by seed.json version.
const REQUIRED_V3: [&str; 4] = ["payloads.bin", "metadata.jsonl", "report.json", "seed.json"];
const REQUIRED_V4: [&str; 2] = ["report.json", "seed.json"];

#[derive(Debug)]
pub struct Summary {
    files: usize,
    bytes: u64,
    parts: usize,
    entities: u64,
    /// seed.json version: 1 (v3 payload bundle) or 2 (v4 UDC bundle).
    pub version: u64,
    /// The v4 check's reference count, when it ran.
    references: Option<u64>,
}
impl Summary {
    /// The bundle format, as `pack` records it.
    pub fn format(&self) -> &'static str {
        match self.version {
            2 => arkiv_entity_processor::udc_bundle::FORMAT,
            _ => "sourcify-arkiv.upload.v1",
        }
    }
}

impl fmt::Display for Summary {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "{} files ({} bytes) match SHA256SUMS; report.json completed with {} entities; seed.json lists {} entity part(s)",
            self.files, self.bytes, self.entities, self.parts
        )?;
        if let Some(references) = self.references {
            write!(
                f,
                "; {}: every line in nonce order, {references} key references resolve to earlier lines",
                self.format()
            )?;
        }
        Ok(())
    }
}

/// Collects every problem instead of stopping at the first one.
pub fn verify(dir: &Path) -> Result<Summary> {
    if !dir.is_dir() {
        bail!("{} is not a directory", dir.display());
    }
    let mut problems = Vec::new();
    let mut listed = HashSet::new();
    let mut bytes = 0;
    // The version decides which files are required; an unreadable seed.json is reported below.
    let version = read_json(&dir.join("seed.json"))
        .ok()
        .and_then(|seed| seed["version"].as_u64())
        .unwrap_or(1);
    let required: &[&str] = if version == 2 {
        &REQUIRED_V4
    } else {
        &REQUIRED_V3
    };
    match fs::read_to_string(dir.join("SHA256SUMS")) {
        Err(e) => problems.push(format!("SHA256SUMS: {e}")),
        Ok(sums) => {
            for (n, line) in sums.lines().enumerate() {
                let Some((digest, path)) = line.split_once("  ") else {
                    problems.push(format!("SHA256SUMS line {}: malformed", n + 1));
                    continue;
                };
                if digest.len() != 64 || hex::decode(digest).is_err() {
                    problems.push(format!("SHA256SUMS line {}: invalid digest", n + 1));
                    continue;
                }
                // Only plain relative paths inside the bundle.
                if path.is_empty()
                    || !Path::new(path)
                        .components()
                        .all(|c| matches!(c, Component::Normal(_)))
                {
                    problems.push(format!("SHA256SUMS line {}: unsafe path {path:?}", n + 1));
                    continue;
                }
                if !listed.insert(path.to_owned()) {
                    problems.push(format!("{path}: listed more than once"));
                    continue;
                }
                match sha256_file(&dir.join(path)) {
                    Ok((actual, size)) if actual.eq_ignore_ascii_case(digest) => bytes += size,
                    Ok(_) => problems.push(format!("{path}: checksum mismatch")),
                    // A missing seed.json is reported once, below.
                    Err(_) if path == "seed.json" && !dir.join(path).exists() => {}
                    Err(e) => problems.push(format!("{path}: {e:#}")),
                }
            }
            for path in required {
                if !listed.contains(*path) {
                    problems.push(format!("{path}: not listed in SHA256SUMS"));
                }
            }
        }
    }
    let mut entities = 0;
    match read_json(&dir.join("report.json")) {
        Err(e) => problems.push(format!("report.json: {e:#}")),
        Ok(report) => {
            if report["status"] != "completed" {
                problems.push(format!(
                    "report.json: status is {}, expected \"completed\"",
                    report["status"]
                ));
            }
            entities = report["entities"].as_u64().unwrap_or(0);
        }
    }
    let mut parts = 0;
    let seed_path = dir.join("seed.json");
    match read_json(&seed_path) {
        Err(_) if !seed_path.exists() => {
            problems.push("seed.json: missing, the bundle is incomplete".into())
        }
        Err(e) => problems.push(format!("seed.json: {e:#}")),
        Ok(seed) => match seed["entities"].as_array() {
            None => problems.push("seed.json: missing entities list".into()),
            Some(list) => {
                parts = list.len();
                for part in list {
                    match part["path"].as_str() {
                        Some(path) if listed.contains(path) => {}
                        Some(path) => {
                            problems.push(format!("{path}: in seed.json but not in SHA256SUMS"))
                        }
                        None => problems.push("seed.json: entity part without path".into()),
                    }
                }
            }
        },
    }
    let mut references = None;
    if problems.is_empty() && version == 2 {
        use arkiv_entity_processor::udc_bundle;
        match udc_bundle::read_sums(dir)
            .and_then(|sums| udc_bundle::manifest(dir, &sums))
            .and_then(|m| udc_bundle::check(dir, &m))
        {
            Ok(checked) => references = Some(checked.references),
            Err(e) => problems.push(format!("{e:#}")),
        }
    } else if version != 1 && version != 2 {
        problems.push(format!("seed.json: unknown version {version}"));
    }
    if !problems.is_empty() {
        bail!(
            "bundle {} failed verification:\n  {}",
            dir.display(),
            problems.join("\n  ")
        );
    }
    Ok(Summary {
        files: listed.len(),
        bytes,
        parts,
        entities,
        version,
        references,
    })
}

fn read_json(path: &Path) -> Result<Value> {
    let text = fs::read_to_string(path)?;
    serde_json::from_str(&text).context("invalid JSON")
}

/// Lowercase hex SHA-256 and size of a file, streamed.
fn sha256_file(path: &Path) -> Result<(String, u64)> {
    let mut file = File::open(path)?;
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
