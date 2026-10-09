//! Hydrate actual import wire entities. Only compact locators remain in memory;
//! payloads are read on demand, one contract and its dependencies at a time.
use anyhow::{bail, ensure, Context, Result};
use arkiv_entity_processor::{entity_key, hx, sha, Entity};
use base64::{engine::general_purpose::STANDARD, Engine};
use clap::Args;
use flate2::read::GzDecoder;
use serde_json::{json, Map, Value};
use std::{
    collections::HashMap,
    fs::File,
    io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write},
    path::{Component, Path, PathBuf},
};

pub const LONG_ABOUT: &str = "Reconstruct full contract JSON from a bundle's import-ready entities.\n\nWrites one hydrated contract per line to stdout, in bundle order. Reads seed.json and its\nentity parts, including gzip payloads and spill blobs; auxiliary payloads.bin and metadata.jsonl\nare not used. Validates entity identities, payload checksums and shared content hashes.\nRuns offline without RPC. Memory contains an index proportional to entity count and the\ncurrent contract/dependencies, never all bundle payloads. Output may be partial on error.";
#[derive(Args, Debug)]
pub struct HydrateArgs {
    /// Bundle directory (also supports extracted prefill archives)
    #[arg(value_name = "BUNDLE_DIR")]
    pub dir: PathBuf,
}
#[derive(Clone, Copy)]
struct Location {
    file: usize,
    offset: u64,
    length: usize,
}
type Reference = (String, String, Option<u64>);
struct Index {
    files: Vec<PathBuf>,
    refs: HashMap<Reference, Location>,
    ids: HashMap<String, [u8; 32]>,
    contracts: Vec<Location>,
}
fn string<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v[key]
        .as_str()
        .with_context(|| format!("missing string {key}"))
}
fn typed<'a>(v: &'a Value, key: &str, prefix: &str) -> Result<&'a str> {
    string(v, key)?
        .strip_prefix(prefix)
        .with_context(|| format!("invalid {key}: expected {prefix}"))
}
impl Index {
    fn new(dir: &Path) -> Result<Self> {
        let seed: Value = serde_json::from_reader(File::open(dir.join("seed.json"))?)?;
        let mut index = Self {
            files: vec![],
            refs: HashMap::new(),
            ids: HashMap::new(),
            contracts: vec![],
        };
        for part in seed["entities"]
            .as_array()
            .context("seed has no entity parts")?
        {
            let path = string(part, "path")?;
            ensure!(
                !path.is_empty()
                    && Path::new(path)
                        .components()
                        .all(|p| matches!(p, Component::Normal(_))),
                "unsafe entity part path"
            );
            let file = index.files.len();
            index.files.push(dir.join(path));
            let mut reader = BufReader::new(File::open(&index.files[file])?);
            let mut offset = 0;
            let mut line = String::new();
            loop {
                line.clear();
                let length = reader.read_line(&mut line)?;
                if length == 0 {
                    break;
                }
                let location = Location {
                    file,
                    offset,
                    length,
                };
                offset += length as u64;
                if line.trim().is_empty() {
                    continue;
                }
                let wire: Value = serde_json::from_str(&line)
                    .with_context(|| format!("{path} at byte {}", location.offset))?;
                let id: [u8; 32] = hex::decode(typed(&wire, "stagingid", "bytes32:0x")?)?
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("invalid staging ID"))?;
                let key = string(&wire, "$key")?;
                ensure!(
                    key == entity_key(&id),
                    "entity key does not match staging ID"
                );
                ensure!(
                    index.ids.insert(key.to_owned(), id).is_none(),
                    "duplicate entity key {key}"
                );
                let kind = typed(&wire, "kind", "str:")?;
                if kind == "verified_contract" {
                    index.contracts.push(location);
                }
                if let Some(hash) = wire.get("hash") {
                    let hash = hash
                        .as_str()
                        .and_then(|s| s.strip_prefix("bytes32:"))
                        .context("invalid content hash")?;
                    let part = if kind == "blob" {
                        Some(typed(&wire, "part", "i32:")?.parse()?)
                    } else {
                        None
                    };
                    ensure!(
                        index
                            .refs
                            .insert((kind.to_owned(), hash.to_owned(), part), location)
                            .is_none(),
                        "duplicate content reference {kind} {hash}"
                    );
                }
            }
        }
        Ok(index)
    }
    fn read(&self, location: Location) -> Result<Entity> {
        let mut file = File::open(&self.files[location.file])?;
        file.seek(SeekFrom::Start(location.offset))?;
        let mut line = vec![0; location.length];
        file.read_exact(&mut line)?;
        let wire: Value = serde_json::from_slice(&line)?;
        let encoded = STANDARD.decode(string(&wire, "$payload")?)?;
        let payload = match typed(&wire, "contentencoding", "str:")? {
            "identity" => encoded,
            "gzip" => {
                let mut out = vec![];
                GzDecoder::new(encoded.as_slice())
                    .take((arkiv_entity_processor::BUDGET + 1) as u64)
                    .read_to_end(&mut out)?;
                out
            }
            encoding => bail!("unsupported content encoding {encoding}"),
        };
        ensure!(
            payload.len() <= arkiv_entity_processor::BUDGET,
            "decoded wire payload exceeds limit"
        );
        ensure!(
            typed(&wire, "payloadsha256", "bytes32:")? == hx(sha(&payload)),
            "payload checksum mismatch"
        );
        let mut attributes = Map::new();
        for (key, value) in wire.as_object().context("entity must be object")? {
            if key.starts_with('$')
                || [
                    "contentencoding",
                    "originalcontenttype",
                    "payloadsha256",
                    "stagingid",
                    "uploadschema",
                ]
                .contains(&key.as_str())
            {
                continue;
            }
            let (kind, raw) = value
                .as_str()
                .and_then(|v| v.split_once(':'))
                .context("invalid typed attribute")?;
            let val = match kind {
                "key" => json!(hex::encode(
                    self.ids
                        .get(raw)
                        .with_context(|| format!("missing key dependency {raw}"))?
                )),
                "i32" => json!(raw.parse::<i32>()?),
                "u64" => {
                    raw.parse::<u64>()?;
                    json!(raw)
                }
                "bool" => json!(raw.parse::<bool>()?),
                "str" | "addr" | "bytes32" => json!(raw),
                _ => bail!("unsupported attribute type {kind}"),
            };
            attributes.insert(
                if key == "projectid" {
                    "projectId".into()
                } else {
                    key.clone()
                },
                json!({"type":kind,"value":val}),
            );
        }
        let entity = Entity::new(
            typed(&wire, "kind", "str:")?,
            payload,
            typed(&wire, "originalcontenttype", "str:")?,
            attributes,
        );
        ensure!(
            hx(entity.id) == typed(&wire, "stagingid", "bytes32:")?,
            "entity staging identity mismatch"
        );
        Ok(entity)
    }
    fn resolve(&self, kind: &str, hash: &str, part: Option<u64>) -> Result<Entity> {
        let location = self
            .refs
            .get(&(kind.to_owned(), hash.to_owned(), part))
            .with_context(|| format!("missing dependency {kind} {hash} part {part:?}"))?;
        self.read(*location)
    }
}
pub fn hydrate(args: &HydrateArgs) -> Result<()> {
    let index = Index::new(&args.dir)?;
    let stdout = std::io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    for location in &index.contracts {
        let entity = index.read(*location)?;
        let record = arkiv_entity_processor::hydrate::hydrate_with(&entity, |kind, hash, part| {
            index.resolve(kind, hash, part)
        })?;
        serde_json::to_writer(&mut out, &record)?;
        out.write_all(b"\n")?;
    }
    out.flush()?;
    Ok(())
}
