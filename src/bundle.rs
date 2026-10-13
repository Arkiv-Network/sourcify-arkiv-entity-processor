//! Indexed, read-only access to processor bundles and extracted prefill archives.
//!
//! Opening a bundle scans and checks its seed, report, and entity parts. This is a
//! fixture/development backend: the scan and indexes have not been sized for a
//! production Mainnet import. Audit files (`payloads.bin`, `metadata.jsonl`) are
//! unnecessary. Completion here proves consistency with the local manifest, not
//! completeness of an upstream chain or source export.
//!
//! The directory must remain read-only for the lifetime of the bundle. Each read
//! opens an independent handle and checks the indexed line's digest before decoding,
//! so concurrent readers do not share a cursor and changed entity bytes fail closed.
//! This does not provide filesystem snapshots or detect edits to unaccessed files.
//! Startup validates contract envelope versions (including spills); full transitive
//! content validation remains the hydrator's responsibility.
use crate::{entity_key, sha, Entity};
use anyhow::{ensure, Context, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{HashMap, HashSet},
    fmt,
    fs::{self, File},
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    path::{Component, Path, PathBuf},
};

const UPLOAD_SCHEMA: &str = "sourcify-arkiv.upload.v1";
const REPORT_SCHEMA: &str = "sourcify-offline-import.v1";
const CONTRACT_SCHEMA: &str = "sourcify.verified-contract.v3";

/// Stable error classes for adapters. Preserved through `anyhow` contexts.
#[derive(Debug)]
pub enum BundleError {
    Unavailable(String),
    MissingDependency(String),
    Corrupt(String),
    Ambiguous(String),
    UnsupportedSchema(String),
}
impl fmt::Display for BundleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (kind, detail) = match self {
            Self::Unavailable(s) => ("bundle unavailable", s),
            Self::MissingDependency(s) => ("missing dependency", s),
            Self::Corrupt(s) => ("corrupt bundle", s),
            Self::Ambiguous(s) => ("ambiguous contract", s),
            Self::UnsupportedSchema(s) => ("unsupported schema", s),
        };
        write!(f, "{kind}: {detail}")
    }
}
impl std::error::Error for BundleError {}

fn classify(error: anyhow::Error) -> anyhow::Error {
    if error.downcast_ref::<BundleError>().is_some() {
        error
    } else {
        BundleError::Corrupt(format!("{error:#}")).into()
    }
}
fn io_error(path: &Path, error: std::io::Error) -> anyhow::Error {
    BundleError::Unavailable(format!("{}: {error}", path.display())).into()
}

/// Manifest identity and observed counts, separate from contract API responses.
#[derive(Debug, Clone)]
pub struct BundleMetadata {
    pub manifest_sha256: String,
    pub seed_sha256: String,
    pub seed_version: u64,
    pub schema_version: u64,
    pub upload_schema: String,
    pub report_schema: String,
    /// Destination Arkiv chain; distinct from the Ethereum source chain.
    pub arkiv_chain_id: u64,
    pub source_chain_id: u64,
    /// Unique indexed contract entities, in contrast with reported input records.
    pub contracts: u64,
    pub input_contracts: u64,
    pub entities: u64,
    pub parts: usize,
    pub source_complete_required: bool,
}
#[derive(Clone, Copy)]
struct Location {
    file: usize,
    offset: u64,
    length: usize,
    digest: [u8; 32],
}
type Reference = (String, String, Option<u64>);
pub struct Bundle {
    files: Vec<PathBuf>,
    refs: HashMap<Reference, Location>,
    ids: HashMap<String, [u8; 32]>,
    contracts: Vec<Location>,
    by_address: HashMap<(u64, String), Vec<Location>>,
    metadata: BundleMetadata,
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
fn digest(value: &str) -> Result<[u8; 32]> {
    hex::decode(value)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("invalid SHA-256 digest"))
}
fn chain(value: &str) -> Result<u64> {
    ensure!(
        !value.is_empty() && value.bytes().all(|b| b.is_ascii_digit()),
        "invalid chain ID"
    );
    let value: u64 = value.parse()?;
    ensure!(value > 0, "invalid chain ID");
    Ok(value)
}
fn address(value: &str) -> Result<String> {
    ensure!(
        value.starts_with("0x") && value.len() == 42 && hex::decode(&value[2..]).is_ok(),
        "invalid address"
    );
    Ok(value.to_ascii_lowercase())
}
fn read_file(path: &Path) -> Result<Vec<u8>> {
    fs::read(path).map_err(|e| io_error(path, e))
}
fn checked_json(
    dir: &Path,
    name: &str,
    sums: &HashMap<String, [u8; 32]>,
) -> Result<(Value, String)> {
    let expected = sums
        .get(name)
        .with_context(|| format!("missing {name} in SHA256SUMS"))?;
    let bytes = read_file(&dir.join(name))?;
    let actual = sha(&bytes);
    ensure!(&actual == expected, "{name}: checksum mismatch");
    Ok((
        serde_json::from_slice(&bytes).with_context(|| format!("invalid {name}"))?,
        hex::encode(actual),
    ))
}

impl Bundle {
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        Self::open_inner(dir.as_ref()).map_err(classify)
    }
    fn open_inner(dir: &Path) -> Result<Self> {
        let manifest = read_file(&dir.join("SHA256SUMS"))?;
        let mut sums = HashMap::new();
        for line in std::str::from_utf8(&manifest)?.lines() {
            let (sum, path) = line.split_once("  ").context("malformed SHA256SUMS")?;
            safe_path(path)?;
            ensure!(
                sums.insert(path.to_owned(), digest(sum)?).is_none(),
                "duplicate SHA256SUMS path {path}"
            );
        }
        let (seed, seed_sha256) = checked_json(dir, "seed.json", &sums)?;
        let (report, _) = checked_json(dir, "report.json", &sums)?;
        if seed["version"] != 1 || report["schema"] != REPORT_SCHEMA {
            return Err(BundleError::UnsupportedSchema(format!(
                "seed version {}, report {}",
                seed["version"], report["schema"]
            ))
            .into());
        }
        ensure!(
            report["status"] == "completed",
            "report.json: incomplete bundle"
        );
        let parts = seed["entities"]
            .as_array()
            .context("seed has no entity parts")?;
        let audits = report["parts"]
            .as_array()
            .context("report has no entity parts")?;
        ensure!(
            parts.len() == audits.len(),
            "missing or extra report entity parts"
        );
        let input_contracts = report["contracts"]
            .as_u64()
            .context("missing reported contracts")?;
        let entities = report["entities"]
            .as_u64()
            .context("missing reported entities")?;
        let mut index = Self {
            files: vec![],
            refs: HashMap::new(),
            ids: HashMap::new(),
            contracts: vec![],
            by_address: HashMap::new(),
            metadata: BundleMetadata {
                manifest_sha256: hex::encode(sha(&manifest)),
                seed_sha256,
                seed_version: 1,
                schema_version: 3,
                upload_schema: UPLOAD_SCHEMA.into(),
                report_schema: REPORT_SCHEMA.into(),
                arkiv_chain_id: seed["chainId"].as_u64().context("invalid Arkiv chain ID")?,
                source_chain_id: report["sourceChainId"]
                    .as_u64()
                    .context("invalid source chain ID")?,
                contracts: 0,
                input_contracts,
                entities,
                parts: parts.len(),
                source_complete_required: report["sourceCompleteRequired"]
                    .as_bool()
                    .context("missing source completion policy")?,
            },
        };
        let mut paths = HashSet::new();
        for (part, audit) in parts.iter().zip(audits) {
            let path = string(part, "path")?;
            safe_path(path)?;
            ensure!(paths.insert(path), "duplicate entity part {path}");
            ensure!(audit["path"] == path, "report entity part order mismatch");
            let expected = sums
                .get(path)
                .with_context(|| format!("missing entity part {path} in SHA256SUMS"))?;
            ensure!(
                &digest(string(audit, "sha256")?)? == expected,
                "{path}: report checksum mismatch"
            );
            let file = index.files.len();
            index.files.push(dir.join(path));
            let mut reader = BufReader::new(
                File::open(&index.files[file]).map_err(|e| io_error(&index.files[file], e))?,
            );
            let mut offset = 0;
            let mut line = String::new();
            let mut part_digest = Sha256::new();
            loop {
                line.clear();
                let length = reader
                    .read_line(&mut line)
                    .map_err(|e| io_error(&index.files[file], e))?;
                if length == 0 {
                    break;
                }
                part_digest.update(line.as_bytes());
                let location = Location {
                    file,
                    offset,
                    length,
                    digest: sha(line.as_bytes()),
                };
                offset += length as u64;
                if line.trim().is_empty() {
                    continue;
                }
                let wire: Value = serde_json::from_str(&line)
                    .with_context(|| format!("{path} at byte {}", location.offset))?;
                if typed(&wire, "uploadschema", "str:")? != UPLOAD_SCHEMA {
                    return Err(BundleError::UnsupportedSchema(
                        string(&wire, "uploadschema")?.into(),
                    )
                    .into());
                }
                // Entity::new installs projectId itself. Check the wire value
                // before that normalization could hide a changed attribute.
                ensure!(
                    typed(&wire, "projectid", "str:")? == "sourcify"
                        && wire.get("projectId").is_none(),
                    "invalid wire projectid attribute"
                );
                let id = digest(typed(&wire, "stagingid", "bytes32:0x")?)?;
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
                    let chain = chain(typed(&wire, "chainid", "u64:")?)?;
                    ensure!(
                        chain == index.metadata.source_chain_id,
                        "contract source chain differs from report"
                    );
                    let address = address(typed(&wire, "address", "addr:")?)?;
                    index.contracts.push(location);
                    index
                        .by_address
                        .entry((chain, address))
                        .or_default()
                        .push(location);
                }
                if wire.get("hash").is_some() {
                    let hash = typed(&wire, "hash", "bytes32:")?;
                    digest(hash.strip_prefix("0x").context("invalid content hash")?)?;
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
            // A shorter, well-formed JSONL file is still an incomplete import.
            ensure!(
                audit["bytes"].as_u64() == Some(offset),
                "{path}: checksum mismatch (missing or extra entity bytes)"
            );
            ensure!(
                part_digest.finalize().as_slice() == expected,
                "{path}: checksum mismatch (missing or corrupt entity data)"
            );
        }
        index.metadata.contracts = index.contracts.len() as u64;
        ensure!(
            index.ids.len() as u64 == entities,
            "missing or extra entities relative to report"
        );
        ensure!(
            index.metadata.contracts <= input_contracts
                && (input_contracts == 0 || !index.contracts.is_empty()),
            "missing or inconsistent reported contracts"
        );
        for location in &index.contracts {
            let entity = index.read(*location)?;
            let payload = crate::hydrate::payload_with(&entity, &mut |kind, hash, part| {
                index.resolve(kind, hash, part)
            })?;
            let envelope: Value = serde_json::from_slice(&payload)?;
            if envelope["schema"] != CONTRACT_SCHEMA {
                return Err(BundleError::UnsupportedSchema(format!(
                    "contract envelope {}",
                    envelope["schema"]
                ))
                .into());
            }
        }
        Ok(index)
    }

    pub fn metadata(&self) -> &BundleMetadata {
        &self.metadata
    }

    /// Deterministic input order, with one entity decoded at a time.
    pub fn contracts(&self) -> impl ExactSizeIterator<Item = Result<Entity>> + '_ {
        self.contracts.iter().map(|location| self.read(*location))
    }

    /// Case-insensitive address lookup; decimal chain IDs are canonicalized.
    /// An absent index entry performs no file or dependency reads.
    pub fn find_contract(&self, chain_id: &str, contract_address: &str) -> Result<Option<Entity>> {
        let key = (
            chain(chain_id).map_err(classify)?,
            address(contract_address).map_err(classify)?,
        );
        match self.by_address.get(&key).map(Vec::as_slice) {
            None | Some([]) => Ok(None),
            Some([location]) => self.read(*location).map(Some),
            Some(_) => {
                Err(BundleError::Ambiguous(format!("chain {} address {}", key.0, key.1)).into())
            }
        }
    }

    pub fn resolve(&self, kind: &str, hash: &str, part: Option<u64>) -> Result<Entity> {
        let location = self
            .refs
            .get(&(kind.to_owned(), hash.to_owned(), part))
            .ok_or_else(|| {
                BundleError::MissingDependency(format!("{kind} {hash} part {part:?}"))
            })?;
        self.read(*location)
    }

    fn read(&self, location: Location) -> Result<Entity> {
        self.read_inner(location).map_err(classify)
    }

    fn read_inner(&self, location: Location) -> Result<Entity> {
        let path = &self.files[location.file];
        let mut file = File::open(path).map_err(|e| io_error(path, e))?;
        file.seek(SeekFrom::Start(location.offset))
            .map_err(|e| io_error(path, e))?;
        let mut line = vec![0; location.length];
        file.read_exact(&mut line).map_err(|e| {
            if e.kind() == std::io::ErrorKind::UnexpectedEof {
                BundleError::Corrupt("entity file truncated after bundle indexing".into()).into()
            } else {
                io_error(path, e)
            }
        })?;
        ensure!(
            sha(&line) == location.digest,
            "entity bytes changed after bundle indexing"
        );
        let wire: Value = serde_json::from_slice(&line)?;
        crate::wire::decode(&wire, |key| {
            self.ids
                .get(key)
                .copied()
                .ok_or_else(|| BundleError::MissingDependency(format!("key {key}")).into())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hx;
    use base64::{engine::general_purpose::STANDARD, Engine};
    use flate2::{write::GzEncoder, Compression};
    use serde_json::json;
    use std::{
        io::Write,
        sync::{
            atomic::{AtomicU64, Ordering},
            Arc,
        },
    };

    static NEXT_DIR: AtomicU64 = AtomicU64::new(0);
    const ADDRESS: &str = "0xabcdefabcdefabcdefabcdefabcdefabcdefabcd";
    struct Fixture(PathBuf);
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn record(address: &str, id: &str) -> Value {
        json!({
            "chainId": "1", "address": address, "matchId": id,
            "verifiedAt": "2020-01-01T00:00:00Z", "abi": [],
            "compilation": {"language":"Solidity", "compiler":"solc", "compilerVersion":"1", "compilerSettings":{}, "fullyQualifiedName":"A.sol:A"},
            "sources": {"A.sol":{"content":"contract A {}"}},
            "creationBytecode":{"onchainBytecode":null,"recompiledBytecode":null},
            "runtimeBytecode":{"onchainBytecode":"0x6000","recompiledBytecode":"0x6000"}
        })
    }
    fn wires(records: &[Value], gzip: bool) -> Vec<Value> {
        let mut seen = HashSet::new();
        records
            .iter()
            .flat_map(|record| crate::transform(record).unwrap())
            .filter(|entity| seen.insert(entity.id))
            .map(|entity| {
                let payload = if gzip {
                    let mut encoder = GzEncoder::new(vec![], Compression::default());
                    encoder.write_all(&entity.payload).unwrap();
                    encoder.finish().unwrap()
                } else {
                    entity.payload.clone()
                };
                let mut wire = json!({
                    "$key": entity_key(&entity.id), "$payload": STANDARD.encode(payload),
                    "contentencoding": format!("str:{}", if gzip {"gzip"} else {"identity"}),
                    "originalcontenttype": format!("str:{}", entity.content_type),
                    "payloadsha256": format!("bytes32:{}", hx(sha(&entity.payload))),
                    "stagingid": format!("bytes32:{}", hx(entity.id)),
                    "uploadschema": format!("str:{UPLOAD_SCHEMA}")
                });
                for (name, attr) in entity.attributes {
                    let kind = attr["type"].as_str().unwrap();
                    let value = &attr["value"];
                    let value = if kind == "key" {
                        entity_key(
                            &hex::decode(value.as_str().unwrap())
                                .unwrap()
                                .try_into()
                                .unwrap(),
                        )
                    } else {
                        value
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| value.to_string())
                    };
                    wire[if name == "projectId" {
                        "projectid"
                    } else {
                        &name
                    }] = json!(format!("{kind}:{value}"));
                }
                wire
            })
            .collect()
    }
    impl Fixture {
        fn new(wires: &[Value], contracts: u64) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "processor-bundle-reader-{}-{}",
                std::process::id(),
                NEXT_DIR.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(dir.join("entities")).unwrap();
            let fixture = Self(dir);
            fixture.write(wires, contracts);
            fixture
        }
        fn write(&self, wires: &[Value], contracts: u64) {
            let part = wires.iter().map(|v| format!("{v}\n")).collect::<String>();
            let seed = json!({"version":1,"chainId":7706815,"entities":[{"path":"entities/000001.jsonl"}]}).to_string();
            let report = json!({"schema":REPORT_SCHEMA,"status":"completed","sourceChainId":1,"contracts":contracts,"entities":wires.len(),"sourceCompleteRequired":false,"parts":[{"path":"entities/000001.jsonl","bytes":part.len(),"sha256":hex::encode(sha(&part))}]}).to_string();
            let mut manifest = String::new();
            for (path, bytes) in [
                ("entities/000001.jsonl", part),
                ("seed.json", seed),
                ("report.json", report),
            ] {
                manifest.push_str(&format!("{}  {path}\n", hex::encode(sha(&bytes))));
                fs::write(self.0.join(path), bytes).unwrap();
            }
            fs::write(self.0.join("SHA256SUMS"), manifest).unwrap();
        }
        fn update_metadata(&self, name: &str, value: Value) {
            let bytes = value.to_string();
            fs::write(self.0.join(name), &bytes).unwrap();
            let path = self.0.join("SHA256SUMS");
            let sums = fs::read_to_string(&path)
                .unwrap()
                .lines()
                .map(|line| {
                    if line.ends_with(&format!("  {name}")) {
                        format!("{}  {name}\n", hex::encode(sha(&bytes)))
                    } else {
                        format!("{line}\n")
                    }
                })
                .collect::<String>();
            fs::write(path, sums).unwrap();
        }
    }
    fn class(error: &anyhow::Error) -> &BundleError {
        error.downcast_ref::<BundleError>().unwrap()
    }

    #[test]
    fn roundtrip_prefill_gzip_and_canonical_lookup() {
        let records = [
            record(ADDRESS, "1"),
            record("0x2222222222222222222222222222222222222222", "2"),
        ];
        let fixture = Fixture::new(&wires(&records, true), 2);
        let bundle = Bundle::open(&fixture.0).unwrap();
        let results: Vec<_> = bundle
            .contracts()
            .map(|entity| {
                crate::hydrate::hydrate_with(&entity.unwrap(), |kind, hash, part| {
                    bundle.resolve(kind, hash, part)
                })
                .unwrap()
            })
            .collect();
        assert_eq!(results, records);
        let uppercase = format!("0x{}", ADDRESS[2..].to_ascii_uppercase());
        let found = bundle.find_contract("001", &uppercase).unwrap().unwrap();
        assert_eq!(found.id, bundle.contracts().next().unwrap().unwrap().id);
        assert_eq!(bundle.metadata().contracts, 2);
        assert_eq!(bundle.metadata().schema_version, 3);
        assert_eq!(bundle.metadata().arkiv_chain_id, 7706815);
        assert!(!bundle.metadata().source_complete_required);
    }

    #[test]
    fn misses_and_ambiguity_do_not_open_entity_files() {
        let fixture = Fixture::new(
            &wires(&[record(ADDRESS, "1"), record(ADDRESS, "2")], false),
            2,
        );
        let bundle = Bundle::open(&fixture.0).unwrap();
        fs::remove_file(fixture.0.join("entities/000001.jsonl")).unwrap();
        assert!(bundle
            .find_contract("1", "0x2222222222222222222222222222222222222222")
            .unwrap()
            .is_none());
        assert!(matches!(
            class(&bundle.find_contract("1", ADDRESS).unwrap_err()),
            BundleError::Ambiguous(_)
        ));
        assert!(matches!(
            class(&bundle.resolve("code", &hx([0u8; 32]), None).unwrap_err()),
            BundleError::MissingDependency(_)
        ));
    }

    #[test]
    fn startup_rejects_truncated_parts_counts_and_incomplete_reports() {
        let entities = wires(&[record(ADDRESS, "1")], false);
        let fixture = Fixture::new(&entities, 1);
        let part_path = fixture.0.join("entities/000001.jsonl");
        let contents = fs::read_to_string(&part_path).unwrap();
        fs::write(&part_path, contents.lines().take(1).collect::<String>()).unwrap();
        assert!(matches!(
            class(&Bundle::open(&fixture.0).err().unwrap()),
            BundleError::Corrupt(_)
        ));
        fixture.write(&entities, 1);
        let mut report: Value =
            serde_json::from_slice(&fs::read(fixture.0.join("report.json")).unwrap()).unwrap();
        report["entities"] = json!(entities.len() + 1);
        fixture.update_metadata("report.json", report.clone());
        assert!(Bundle::open(&fixture.0)
            .err()
            .unwrap()
            .to_string()
            .contains("missing or extra entities"));
        report["status"] = json!("processing");
        fixture.update_metadata("report.json", report);
        assert!(Bundle::open(&fixture.0)
            .err()
            .unwrap()
            .to_string()
            .contains("incomplete bundle"));
    }

    #[test]
    fn corrupt_payload_and_schema_errors_remain_typed() {
        let mut entities = wires(&[record(ADDRESS, "1")], false);
        let contract = entities
            .iter_mut()
            .find(|e| e["kind"] == "str:verified_contract")
            .unwrap();
        contract["$payload"] = json!(STANDARD.encode(b"corrupt"));
        let fixture = Fixture::new(&entities, 1);
        assert!(Bundle::open(&fixture.0)
            .err()
            .unwrap()
            .to_string()
            .contains("payload checksum mismatch"));
        entities[0]["uploadschema"] = json!("str:future.v2");
        fixture.write(&entities, 1);
        assert!(matches!(
            class(&Bundle::open(&fixture.0).err().unwrap()),
            BundleError::UnsupportedSchema(_)
        ));
    }

    #[test]
    fn changed_project_identity_fails_before_normalization() {
        for kind in ["str:verified_contract", "str:sourcefile"] {
            for name in ["projectid", "projectId"] {
                let mut entities = wires(&[record(ADDRESS, "1")], false);
                entities
                    .iter_mut()
                    .find(|entity| entity["kind"] == kind)
                    .unwrap()[name] = json!("str:other");
                // Rewrite the file manifests so rejection proves semantic wire
                // validation, rather than a stale file checksum check.
                let fixture = Fixture::new(&entities, 1);
                let error = Bundle::open(&fixture.0).err().unwrap();
                assert!(matches!(class(&error), BundleError::Corrupt(_)));
                assert!(error.to_string().contains("invalid wire projectid"));
            }
        }
    }

    #[test]
    fn old_contract_envelope_fails_before_readiness() {
        let mut entities = wires(&[record(ADDRESS, "1")], false);
        let contract = entities
            .iter_mut()
            .find(|e| e["kind"] == "str:verified_contract")
            .unwrap();
        let old = STANDARD
            .decode(contract["$payload"].as_str().unwrap())
            .unwrap();
        let new = String::from_utf8(old)
            .unwrap()
            .replace(CONTRACT_SCHEMA, "sourcify.verified-contract.v2");
        contract["$payload"] = json!(STANDARD.encode(&new));
        contract["payloadsha256"] = json!(format!("bytes32:{}", hx(sha(&new))));
        // Preserve the original staging attributes, including remapped key references.
        let original = crate::transform(&record(ADDRESS, "1"))
            .unwrap()
            .pop()
            .unwrap();
        let entity = Entity::new(
            "verified_contract",
            new.into_bytes(),
            "application/json",
            original.attributes,
        );
        contract["stagingid"] = json!(format!("bytes32:{}", hx(entity.id)));
        contract["$key"] = json!(entity_key(&entity.id));
        let fixture = Fixture::new(&entities, 1);
        assert!(matches!(
            class(&Bundle::open(&fixture.0).err().unwrap()),
            BundleError::UnsupportedSchema(_)
        ));
    }

    #[test]
    fn indexed_reads_fail_when_bytes_change_or_files_disappear() {
        let mut entities = wires(&[record(ADDRESS, "1")], false);
        let fixture = Fixture::new(&entities, 1);
        let bundle = Bundle::open(&fixture.0).unwrap();
        entities
            .iter_mut()
            .find(|e| e["kind"] == "str:verified_contract")
            .unwrap()["address"] = json!("addr:0x2222222222222222222222222222222222222222");
        fixture.write(&entities, 1);
        assert!(matches!(
            class(&bundle.find_contract("1", ADDRESS).unwrap_err()),
            BundleError::Corrupt(_)
        ));
        fs::remove_file(fixture.0.join("entities/000001.jsonl")).unwrap();
        assert!(matches!(
            class(&bundle.find_contract("1", ADDRESS).unwrap_err()),
            BundleError::Unavailable(_)
        ));
    }

    #[test]
    fn concurrent_readers_preserve_order_and_dependencies() {
        let records = [
            record(ADDRESS, "1"),
            record("0x2222222222222222222222222222222222222222", "2"),
        ];
        let fixture = Fixture::new(&wires(&records, true), 2);
        let bundle = Arc::new(Bundle::open(&fixture.0).unwrap());
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let bundle = bundle.clone();
                let records = records.clone();
                std::thread::spawn(move || {
                    for expected in records {
                        let entity = bundle
                            .find_contract("1", expected["address"].as_str().unwrap())
                            .unwrap()
                            .unwrap();
                        let actual = crate::hydrate::hydrate_with(&entity, |kind, hash, part| {
                            bundle.resolve(kind, hash, part)
                        })
                        .unwrap();
                        assert_eq!(actual, expected);
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
    }
}
