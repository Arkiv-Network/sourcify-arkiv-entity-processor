//! Prototype: the Sourcify dataset on Arkiv's user-defined-columns (UDC) entity model.
//!
//! The format is specified in `docs/udc-proposal.md`. An entity is a set of typed attributes,
//! some indexed; binary content lives in unindexed `bytes` attributes. Five kinds, each tagged
//! by the indexed `arkiv.schema` string `<prefix>.<kind>`:
//!
//! - `contract`: one per verified address. Query attributes (indexed), the contract-local JSON
//!   (`data`) and `key` references to its compilation and on-chain code.
//! - `compilation`: compiler-owned fields, shared by content hash. `abi` and `compilation` are
//!   separate attributes (the hot `fields=abi,compilation` read); the other fields share one
//!   `artifacts` attribute. `manifest` holds digests, intra-compilation references and source
//!   hashes; `sourcerefs` the source keys in manifest order.
//! - `source`, `code`: shared content, addressed by SHA-256 of the UTF-8 text / Keccak-256 of
//!   the bytecode.
//! - `chunk`: one slice of a value too large for its entity.
//!
//! Every `bytes` value except a chunk's `data` starts with a codec byte: 0 identity, 1 raw
//! DEFLATE (RFC 1951), 2 chunk manifest. Digests cover uncompressed bytes, so compression and
//! chunking never change identities.
//!
//! The decomposition is the v3 processor's (`split.rs`): this module re-lays out its logical
//! envelopes, so hydration restores exactly what v3 hydration restores, in the same key order.
use crate::hydrate::{insert, payload_with, validate_destination, validate_pointer, within};
use crate::split::json_bytes;
use crate::{hx, sha, Entity};
use anyhow::{bail, ensure, Context, Result};
use flate2::{read::DeflateDecoder, write::DeflateEncoder, Compression};
use serde_json::{json, Map, Value};
use sha3::{Digest, Keccak256};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{Read, Write};

/// Bytes values one entity may carry together. Below the node's 128 KiB so that a create also
/// fits a reth transaction (Jalebi's pool refuses transactions over 128 KiB) with its ABI framing.
pub const BUDGET: usize = 120_000;
/// Bytes in one chunk.
pub const CHUNK: usize = 120_000;
/// Bound on one decoded value.
pub const MAX_VALUE: usize = 64 << 20;
/// `expiresAt` of a permanent entity.
pub const PERMANENT: u64 = u64::MAX;
/// The Arkiv operations address, part of the entity key domain.
pub const ARKIV_ADDRESS: [u8; 20] = [
    0x44, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0x44,
];
const IDENTITY: u8 = 0;
const DEFLATE: u8 = 1;
const CHUNKED: u8 = 2;
/// Compiler-owned top-level fields, in hydration order.
pub const TOP_FIELDS: [&str; 13] = [
    "compilation",
    "additionalInput",
    "sources",
    "abi",
    "signatures",
    "metadata",
    "storageLayout",
    "transientStorageLayout",
    "userdoc",
    "devdoc",
    "sourceIds",
    "stdJsonInput",
    "stdJsonOutput",
];
pub const LANES: [&str; 2] = ["creationBytecode", "runtimeBytecode"];
/// Compiler-owned members of each bytecode lane, in hydration order.
pub const LANE_FIELDS: [&str; 5] = [
    "recompiledBytecode",
    "sourceMap",
    "linkReferences",
    "immutableReferences",
    "cborAuxdata",
];
/// Compilation fields with an attribute of their own; every other field is in `artifacts`.
pub const HOT: [&str; 2] = ["abi", "compilation"];
const ONCHAIN: [(&str, &str, &str); 2] = [
    ("creationBytecode", "creationcoderef", "creationcodehash"),
    ("runtimeBytecode", "runtimecoderef", "runtimecodehash"),
];

/// A typed attribute value. JSON follows the node's RPC encoding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Val {
    Bool(bool),
    I32(i32),
    U64(u64),
    Bytes32([u8; 32]),
    Bytes(Vec<u8>),
    Str(String),
    Addr([u8; 20]),
    Key([u8; 32]),
}
impl Val {
    pub fn tag(&self) -> &'static str {
        match self {
            Val::Bool(_) => "bool",
            Val::I32(_) => "i32",
            Val::U64(_) => "u64",
            Val::Bytes32(_) => "bytes32",
            Val::Bytes(_) => "bytes",
            Val::Str(_) => "str",
            Val::Addr(_) => "addr",
            Val::Key(_) => "key",
        }
    }
    /// The SDK/ABI type id (bit 7, the indexed flag, is separate).
    pub fn type_id(&self) -> u8 {
        match self {
            Val::Bool(_) => 1,
            Val::I32(_) => 2,
            Val::U64(_) => 3,
            Val::Bytes32(_) => 6,
            Val::Bytes(_) => 7,
            Val::Str(_) => 8,
            Val::Addr(_) => 9,
            Val::Key(_) => 10,
        }
    }
    /// Value bytes as the node's storage units count them.
    pub fn encoded_len(&self) -> usize {
        match self {
            Val::Bool(_) => 1,
            Val::I32(_) => 4,
            Val::U64(_) => 8,
            Val::Bytes32(_) | Val::Key(_) => 32,
            Val::Addr(_) => 20,
            Val::Bytes(b) => b.len(),
            Val::Str(s) => s.len(),
        }
    }
    pub fn to_json(&self) -> Value {
        match self {
            Val::Bool(b) => json!(b),
            Val::I32(n) => json!(n),
            Val::U64(n) => json!(format!("0x{n:x}")),
            Val::Bytes32(b) | Val::Key(b) => json!(hx(b)),
            Val::Addr(b) => json!(hx(b)),
            Val::Bytes(b) => json!(hx(b)),
            Val::Str(s) => json!(s),
        }
    }
    pub fn from_json(tag: &str, v: &Value) -> Result<Val> {
        let text = || {
            v.as_str()
                .with_context(|| format!("{tag} value must be a string"))
        };
        Ok(match tag {
            "bool" => Val::Bool(v.as_bool().context("bool value")?),
            "i32" => Val::I32(i32::try_from(v.as_i64().context("i32 value")?)?),
            "u64" => Val::U64(quantity(text()?)?),
            "bytes32" => Val::Bytes32(fixed(text()?)?),
            "key" => Val::Key(fixed(text()?)?),
            "addr" => Val::Addr(fixed(text()?)?),
            "bytes" => Val::Bytes(unhex(text()?)?),
            "str" => Val::Str(text()?.to_owned()),
            _ => bail!("unsupported attribute type {tag}"),
        })
    }
}
fn unhex(s: &str) -> Result<Vec<u8>> {
    Ok(hex::decode(s.strip_prefix("0x").context("hex needs 0x")?)?)
}
fn fixed<const N: usize>(s: &str) -> Result<[u8; N]> {
    unhex(s)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("expected {N} bytes"))
}
fn quantity(s: &str) -> Result<u64> {
    let digits = s.strip_prefix("0x").context("quantity needs 0x")?;
    ensure!(
        !digits.is_empty() && (digits == "0" || !digits.starts_with('0')),
        "non-compact quantity {s}"
    );
    Ok(u64::from_str_radix(digits, 16)?)
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Attr {
    pub name: String,
    pub indexed: bool,
    pub value: Val,
}
fn indexed(name: &str, value: Val) -> Attr {
    Attr {
        name: name.into(),
        indexed: true,
        value,
    }
}
fn stored(name: &str, value: Val) -> Attr {
    Attr {
        name: name.into(),
        indexed: false,
        value,
    }
}

/// One entity: its key, owner, expiry and attributes (strictly ascending by name bytes).
/// `unselected` names attributes a query's `attributeSchema` listed but did not return.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UdcEntity {
    pub key: [u8; 32],
    pub owner: [u8; 20],
    pub expires_at: u64,
    pub attributes: Vec<Attr>,
    pub unselected: Vec<String>,
}
impl UdcEntity {
    pub fn get(&self, name: &str) -> Option<&Val> {
        self.attributes
            .iter()
            .find(|a| a.name == name)
            .map(|a| &a.value)
    }
    fn bytes(&self, name: &str) -> Result<Option<&[u8]>> {
        match self.get(name) {
            None if self.unselected.iter().any(|n| n == name) => {
                bail!("attribute {name} of {} was not selected", hx(self.key))
            }
            None => Ok(None),
            Some(Val::Bytes(b)) => Ok(Some(b)),
            Some(v) => bail!("attribute {name} is {}, expected bytes", v.tag()),
        }
    }
    fn key_attr(&self, name: &str) -> Result<Option<[u8; 32]>> {
        match self.get(name) {
            None => Ok(None),
            Some(Val::Key(k)) => Ok(Some(*k)),
            Some(v) => bail!("attribute {name} is {}, expected key", v.tag()),
        }
    }
    fn bytes32_attr(&self, name: &str) -> Result<Option<[u8; 32]>> {
        match self.get(name) {
            None => Ok(None),
            Some(Val::Bytes32(k)) => Ok(Some(*k)),
            Some(v) => bail!("attribute {name} is {}, expected bytes32", v.tag()),
        }
    }
    pub fn schema(&self) -> Option<&str> {
        match self.get("arkiv.schema") {
            Some(Val::Str(s)) => Some(s),
            _ => None,
        }
    }
    /// Sum of the user cells' storage units (name + value + 2 tag bytes, +128 when indexed).
    pub fn user_units(&self) -> u64 {
        self.attributes
            .iter()
            .map(|a| {
                (a.name.len() + a.value.encoded_len() + 2 + if a.indexed { 128 } else { 0 }) as u64
            })
            .sum()
    }
    pub fn bytes_len(&self) -> usize {
        self.attributes
            .iter()
            .map(|a| match &a.value {
                Val::Bytes(b) => b.len(),
                _ => 0,
            })
            .sum()
    }
    /// The node's admission rules for a create, plus this format's per-entity budget.
    pub fn validate(&self) -> Result<()> {
        ensure!(self.attributes.len() <= 32, "more than 32 attributes");
        for pair in self.attributes.windows(2) {
            ensure!(
                pair[0].name.as_bytes() < pair[1].name.as_bytes(),
                "attributes not strictly ascending"
            );
        }
        for a in &self.attributes {
            let n = &a.name;
            ensure!(
                n.len() <= 32
                    && n.starts_with(|c: char| c.is_ascii_alphabetic())
                    && n.chars()
                        .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c))
                    && !n.contains("--"),
                "invalid attribute name {n}"
            );
            match &a.value {
                Val::Str(s) => ensure!(
                    s.len() <= 128 && !s.chars().any(|c| c <= '\u{1f}' || c == '\u{7f}'),
                    "invalid str {n}"
                ),
                Val::Bytes(b) => {
                    ensure!(!a.indexed, "bytes attribute {n} cannot be indexed");
                    ensure!(b.len() <= 128 * 1024, "bytes attribute {n} too long");
                }
                _ => {}
            }
        }
        ensure!(
            self.bytes_len() <= BUDGET,
            "entity exceeds the bytes budget"
        );
        Ok(())
    }
    /// JSON in the node's RPC encoding, with each attribute's indexed flag.
    pub fn to_json(&self) -> Value {
        json!({
            "key": hx(self.key),
            "owner": hx(self.owner),
            "creator": hx(self.owner),
            "expiresAt": format!("0x{:x}", self.expires_at),
            "attributes": self.attributes.iter().map(|a| json!({
                "name": a.name, "type": a.value.tag(), "indexed": a.indexed, "value": a.value.to_json()
            })).collect::<Vec<_>>(),
        })
    }
    /// Reads `to_json` output, or an `arkiv_query` result entity (`attributeSchema` + `attributes`).
    pub fn from_json(v: &Value) -> Result<Self> {
        let flags: HashMap<&str, bool> = match v.get("attributeSchema") {
            Some(schema) => schema
                .as_array()
                .context("attributeSchema")?
                .iter()
                .map(|s| {
                    Ok((
                        s["name"].as_str().context("schema name")?,
                        s["indexed"].as_bool().context("schema indexed")?,
                    ))
                })
                .collect::<Result<_>>()?,
            None => HashMap::new(),
        };
        let mut attributes = vec![];
        for a in v["attributes"].as_array().context("attributes")? {
            let name = a["name"].as_str().context("attribute name")?;
            let tag = a["type"].as_str().context("attribute type")?;
            let indexed = match a.get("indexed") {
                Some(flag) => flag.as_bool().context("indexed")?,
                None => *flags.get(name).context("attribute without indexed flag")?,
            };
            attributes.push(Attr {
                name: name.into(),
                indexed,
                value: Val::from_json(tag, &a["value"])?,
            });
        }
        attributes.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
        let mut unselected: Vec<String> = flags
            .keys()
            .filter(|n| !attributes.iter().any(|a| a.name == **n))
            .map(|n| n.to_string())
            .collect();
        unselected.sort();
        let owner = v["owner"].as_str().context("owner")?;
        if let Some(creator) = v.get("creator").and_then(Value::as_str) {
            ensure!(
                creator.eq_ignore_ascii_case(owner),
                "creator differs from owner"
            );
        }
        Ok(Self {
            key: fixed(v["key"].as_str().context("key")?)?,
            owner: fixed(owner)?,
            expires_at: quantity(v["expiresAt"].as_str().context("expiresAt")?)?,
            attributes,
            unselected,
        })
    }
}

/// The node's entity key derivation: Keccak-256 of chain ID (32 bytes), the Arkiv address,
/// owner, the owner's entity nonce and the salt (`predictEntityKey` in the SDK).
pub fn entity_key(chain_id: u64, owner: &[u8; 20], nonce: u64, salt: u128) -> [u8; 32] {
    let mut b = vec![0u8; 24];
    b.extend_from_slice(&chain_id.to_be_bytes());
    b.extend_from_slice(&ARKIV_ADDRESS);
    b.extend_from_slice(owner);
    b.extend_from_slice(&nonce.to_be_bytes());
    b.extend_from_slice(&salt.to_be_bytes());
    Keccak256::digest(b).into()
}
fn keccak(data: &[u8]) -> [u8; 32] {
    Keccak256::digest(data).into()
}

/// A bytes value: raw DEFLATE when that is smaller, else identity.
pub fn encode(raw: &[u8]) -> Vec<u8> {
    let mut z = DeflateEncoder::new(vec![DEFLATE], Compression::best());
    z.write_all(raw).expect("in-memory deflate");
    let deflated = z.finish().expect("in-memory deflate");
    if deflated.len() < raw.len() + 1 {
        deflated
    } else {
        let mut v = Vec::with_capacity(raw.len() + 1);
        v.push(IDENTITY);
        v.extend_from_slice(raw);
        v
    }
}
/// Decodes an identity or DEFLATE value; chunk manifests are resolved by the reader first.
pub fn decode(value: &[u8]) -> Result<Vec<u8>> {
    let (&codec, body) = value.split_first().context("empty bytes value")?;
    match codec {
        IDENTITY => Ok(body.to_vec()),
        DEFLATE => {
            let mut out = Vec::new();
            DeflateDecoder::new(body)
                .take(MAX_VALUE as u64 + 1)
                .read_to_end(&mut out)?;
            ensure!(out.len() <= MAX_VALUE, "decoded value exceeds limit");
            Ok(out)
        }
        CHUNKED => bail!("chunked value must be joined first"),
        _ => bail!("unknown codec {codec}"),
    }
}
/// A chunk manifest: `02 ‖ sha256(joined) ‖ u32 length ‖ keys`.
/// Digest and length of the joined value, and the chunk keys in order.
type ChunkList = ([u8; 32], usize, Vec<[u8; 32]>);
fn chunk_manifest(value: &[u8]) -> Result<Option<ChunkList>> {
    if value.first() != Some(&CHUNKED) {
        return Ok(None);
    }
    ensure!(
        value.len() >= 37 && (value.len() - 37).is_multiple_of(32),
        "malformed chunk manifest"
    );
    let digest: [u8; 32] = value[1..33].try_into()?;
    let len = u32::from_be_bytes(value[33..37].try_into()?) as usize;
    let keys: Vec<[u8; 32]> = value[37..]
        .chunks(32)
        .map(|k| k.try_into().unwrap())
        .collect();
    ensure!(
        !keys.is_empty() && keys.len() == len.div_ceil(CHUNK),
        "chunk count mismatch"
    );
    Ok(Some((digest, len, keys)))
}

/// Where entities go: schema prefix, chain and owner (the creator).
#[derive(Clone, Debug)]
pub struct Profile {
    pub prefix: String,
    pub chain_id: u64,
    pub owner: [u8; 20],
}
impl Profile {
    pub fn tag(&self, kind: &str) -> String {
        format!("{}.{kind}", self.prefix)
    }
}

/// Builds entities in creation order and deduplicates shared content across contracts.
/// Keys are those the node assigns to creates with salt 0, starting at `next_nonce`.
pub struct Writer {
    pub profile: Profile,
    pub next_nonce: u64,
    sources: HashMap<[u8; 32], [u8; 32]>,
    codes: HashMap<[u8; 32], [u8; 32]>,
    compilations: HashMap<[u8; 32], [u8; 32]>,
    out: Vec<UdcEntity>,
}
impl Writer {
    pub fn new(profile: Profile, first_nonce: u64) -> Self {
        Self {
            profile,
            next_nonce: first_nonce,
            sources: HashMap::new(),
            codes: HashMap::new(),
            compilations: HashMap::new(),
            out: vec![],
        }
    }
    fn create(&mut self, kind: &str, mut attributes: Vec<Attr>) -> Result<[u8; 32]> {
        attributes.push(indexed("arkiv.schema", Val::Str(self.profile.tag(kind))));
        attributes.sort_by(|a, b| a.name.as_bytes().cmp(b.name.as_bytes()));
        let key = entity_key(
            self.profile.chain_id,
            &self.profile.owner,
            self.next_nonce,
            0,
        );
        self.next_nonce += 1;
        let entity = UdcEntity {
            key,
            owner: self.profile.owner,
            expires_at: PERMANENT,
            attributes,
            unselected: vec![],
        };
        entity.validate()?;
        self.out.push(entity);
        Ok(key)
    }
    /// Encoded bytes fields as attributes. While the entity is over budget, the largest
    /// chunkable value moves into chunk entities (created first) and keeps a chunk manifest.
    fn place(&mut self, fields: Vec<(&str, Vec<u8>)>, pinned: &[&str]) -> Result<Vec<Attr>> {
        let mut fields: Vec<(&str, Vec<u8>, bool)> =
            fields.into_iter().map(|(n, v)| (n, v, false)).collect();
        loop {
            let total: usize = fields.iter().map(|f| f.1.len()).sum();
            if total <= BUDGET {
                break;
            }
            let pick = fields
                .iter()
                .enumerate()
                .filter(|(_, f)| !f.2)
                .max_by_key(|(_, f)| (!pinned.contains(&f.0), f.1.len()))
                .map(|(i, _)| i)
                .context("entity cannot fit its budget")?;
            let joined = std::mem::take(&mut fields[pick].1);
            let mut manifest = vec![CHUNKED];
            manifest.extend_from_slice(&sha(&joined));
            manifest.extend_from_slice(&u32::try_from(joined.len())?.to_be_bytes());
            for part in joined.chunks(CHUNK) {
                let key = self.create("chunk", vec![stored("data", Val::Bytes(part.to_vec()))])?;
                manifest.extend_from_slice(&key);
            }
            fields[pick].1 = manifest;
            fields[pick].2 = true;
        }
        Ok(fields
            .into_iter()
            .map(|(n, v, _)| stored(n, Val::Bytes(v)))
            .collect())
    }
    fn source(&mut self, content: &str) -> Result<[u8; 32]> {
        let hash = sha(content);
        if let Some(key) = self.sources.get(&hash) {
            return Ok(*key);
        }
        let mut attrs = self.place(vec![("content", encode(content.as_bytes()))], &[])?;
        attrs.push(indexed("hash", Val::Bytes32(hash)));
        let key = self.create("source", attrs)?;
        self.sources.insert(hash, key);
        Ok(key)
    }
    fn code(&mut self, bytecode: &[u8]) -> Result<[u8; 32]> {
        let hash = keccak(bytecode);
        if let Some(key) = self.codes.get(&hash) {
            return Ok(*key);
        }
        let mut attrs = self.place(vec![("code", encode(bytecode))], &[])?;
        attrs.push(indexed("hash", Val::Bytes32(hash)));
        let key = self.create("code", attrs)?;
        self.codes.insert(hash, key);
        Ok(key)
    }
    /// Re-lays out the v3 entities of one contract (`crate::transform` output, dependencies
    /// first, the verified contract last). Returns the entities created for it; shared content
    /// created for an earlier contract is referenced, not repeated.
    pub fn contract(&mut self, v3: &[Entity]) -> Result<Vec<UdcEntity>> {
        let start = self.out.len();
        let find = |kind: &str, hash: &str, part: Option<u64>| -> Result<Entity> {
            v3.iter()
                .find(|e| {
                    e.kind == kind
                        && e.attributes["hash"]["value"] == hash
                        && part.is_none_or(|p| e.attributes["part"]["value"] == p)
                })
                .cloned()
                .with_context(|| format!("missing v3 {kind} {hash}"))
        };
        let logical = |e: &Entity| payload_with(e, &mut |k, h, p| find(k, h, p));
        let mut code_bytes: HashMap<String, Vec<u8>> = HashMap::new();
        let mut source_text: HashMap<String, String> = HashMap::new();
        let mut compilation = None;
        let mut runs = None;
        for e in v3 {
            match e.kind.as_str() {
                "blob" => {}
                "sourcefile" => {
                    let v: Value = serde_json::from_slice(&logical(e)?)?;
                    let content = v["content"].as_str().context("source content")?;
                    self.source(content)?;
                    source_text.insert(hx(sha(content)), content.into());
                }
                "code" => {
                    let raw = logical(e)?;
                    self.code(&raw)?;
                    code_bytes.insert(hx(keccak(&raw)), raw);
                }
                "compilation" => {
                    let env: Value = serde_json::from_slice(&logical(e)?)?;
                    runs = e.attributes.get("optimizerruns").cloned();
                    compilation = Some(self.compilation(env)?);
                }
                "verified_contract" => {
                    let env: Value = serde_json::from_slice(&logical(e)?)?;
                    let (key, v3hash) =
                        compilation.clone().context("contract before compilation")?;
                    self.verified(
                        e,
                        env,
                        key,
                        &v3hash,
                        runs.as_ref(),
                        &code_bytes,
                        &source_text,
                    )?;
                }
                kind => bail!("unsupported v3 kind {kind}"),
            }
        }
        Ok(self.out[start..].to_vec())
    }
    /// Returns the v4 key and the v3 fingerprint the contract's references name.
    fn compilation(&mut self, env: Value) -> Result<([u8; 32], String)> {
        ensure!(
            env["schema"] == "sourcify.compilation.v3",
            "compilation schema"
        );
        let v3hash = hx(sha(json_bytes(&env)));
        let data = env["data"].as_object().context("compilation data")?;
        let refs = env["references"].as_array().context("references")?;
        let code_at = |path: &str| {
            refs.iter()
                .find(|r| r["path"] == path && r["kind"] == "code")
                .and_then(|r| r["hash"].as_str().map(String::from))
        };
        let slots: Vec<(&str, String)> = [
            ("creation", code_at("/creationBytecode/recompiledBytecode")),
            ("runtime", code_at("/runtimeBytecode/recompiledBytecode")),
        ]
        .into_iter()
        .filter_map(|(slot, hash)| hash.map(|h| (slot, h)))
        .collect();
        let mut sources: Vec<String> = vec![];
        let mut out_refs = vec![];
        for r in refs {
            let path = r["path"].as_str().context("reference path")?;
            out_refs.push(match r["kind"].as_str().context("reference kind")? {
                "local" => json!({"kind":"local","path":path,"pointer":r["pointer"]}),
                "sourcefile" => {
                    let hash = r["hash"].as_str().context("source hash")?;
                    let index = match sources.iter().position(|s| s == hash) {
                        Some(i) => i,
                        None => {
                            sources.push(hash.into());
                            sources.len() - 1
                        }
                    };
                    json!({"kind":"source","path":path,"source":index})
                }
                "code" => {
                    let hash = r["hash"].as_str().context("code hash")?;
                    let slot = slots
                        .iter()
                        .find(|(_, h)| h == hash)
                        .map(|(s, _)| *s)
                        .context("compilation code reference outside its recompiled bytecode")?;
                    json!({"kind":"code","path":path,"code":slot,"encoding":r["encoding"]})
                }
                kind => bail!("unsupported compilation reference {kind}"),
            });
        }
        let mut groups: BTreeMap<&str, Value> = BTreeMap::new();
        let mut artifacts = Map::new();
        for (name, value) in data {
            if HOT.contains(&name.as_str()) {
                groups.insert(name, value.clone());
            } else {
                artifacts.insert(name.clone(), value.clone());
            }
        }
        if !artifacts.is_empty() {
            groups.insert("artifacts", Value::Object(artifacts));
        }
        let group_bytes: Vec<(&str, Vec<u8>)> =
            groups.iter().map(|(n, v)| (*n, json_bytes(v))).collect();
        let digests: Map<String, Value> = group_bytes
            .iter()
            .map(|(n, b)| ((*n).to_string(), json!(hx(sha(b)))))
            .collect();
        let codes: Map<String, Value> = slots
            .iter()
            .map(|(s, h)| ((*s).to_string(), json!(h)))
            .collect();
        let manifest = json!({"codes":codes,"digests":digests,"refs":out_refs,"sources":sources});
        let manifest_bytes = json_bytes(&manifest);
        let hash = sha(&manifest_bytes);
        if let Some(key) = self.compilations.get(&hash) {
            return Ok((*key, v3hash));
        }
        let mut fields: Vec<(&str, Vec<u8>)> = vec![("manifest", encode(&manifest_bytes))];
        fields.extend(group_bytes.iter().map(|(n, b)| (*n, encode(b))));
        if !sources.is_empty() {
            let mut keys = vec![IDENTITY];
            for s in &sources {
                let hash: [u8; 32] = fixed(s)?;
                keys.extend_from_slice(self.sources.get(&hash).context("source not created")?);
            }
            fields.push(("sourcerefs", keys));
        }
        let mut attrs = self.place(fields, &["manifest", "sourcerefs"])?;
        attrs.push(indexed("hash", Val::Bytes32(hash)));
        for (slot, h) in &slots {
            let key = self
                .codes
                .get(&fixed::<32>(h)?)
                .context("code not created")?;
            attrs.push(stored(&format!("{slot}coderef"), Val::Key(*key)));
        }
        let key = self.create("compilation", attrs)?;
        self.compilations.insert(hash, key);
        Ok((key, v3hash))
    }
    #[allow(clippy::too_many_arguments)]
    fn verified(
        &mut self,
        e: &Entity,
        env: Value,
        compilation: [u8; 32],
        v3hash: &str,
        runs: Option<&Value>,
        codes: &HashMap<String, Vec<u8>>,
        sources: &HashMap<String, String>,
    ) -> Result<()> {
        ensure!(
            env["schema"] == "sourcify.verified-contract.v3",
            "contract schema"
        );
        let mut data = env["data"].clone();
        let mut code_refs: Vec<(&str, [u8; 32])> = vec![];
        let mut seen_compilation = HashSet::new();
        for r in env["references"].as_array().context("references")? {
            let path = r["path"].as_str().context("reference path")?;
            match r["kind"].as_str().context("reference kind")? {
                "compilation" => {
                    ensure!(r["hash"] == v3hash, "contract names another compilation");
                    ensure!(r["pointer"] == path, "compilation reference moves a field");
                    let field = path.trim_start_matches('/');
                    let known = TOP_FIELDS.contains(&field)
                        || LANES
                            .iter()
                            .any(|lane| LANE_FIELDS.iter().any(|f| field == format!("{lane}/{f}")));
                    ensure!(known, "unexpected compilation field {path}");
                    seen_compilation.insert(path.to_string());
                }
                "code" => {
                    let hash = r["hash"].as_str().context("code hash")?;
                    let bytecode = codes.get(hash).context("code missing")?;
                    let canonical = r["encoding"] == json!({"prefix":true,"uppercase":[]});
                    match ONCHAIN
                        .iter()
                        .find(|(lane, _, _)| path == format!("/{lane}/onchainBytecode"))
                    {
                        Some((_, attr, _)) if canonical => {
                            code_refs.push((
                                attr,
                                *self.codes.get(&fixed::<32>(hash)?).context("code key")?,
                            ));
                        }
                        // Anything else stays literal contract data.
                        _ => insert(&mut data, path, json!(spell(bytecode, &r["encoding"])?))?,
                    }
                }
                "sourcefile" => {
                    let text = sources
                        .get(r["hash"].as_str().context("source hash")?)
                        .context("source missing")?;
                    insert(&mut data, path, json!(text))?;
                }
                kind => bail!("unsupported contract reference {kind}"),
            }
        }
        let a = &e.attributes;
        let s = |name: &str| {
            a[name]["value"]
                .as_str()
                .map(String::from)
                .with_context(|| format!("v3 attribute {name}"))
        };
        let u = |name: &str| -> Result<u64> { Ok(s(name)?.parse()?) };
        let mut attrs = vec![
            indexed("address", Val::Addr(fixed(&s("address")?)?)),
            indexed("chainid", Val::U64(u("chainid")?)),
            indexed("matchid", Val::U64(u("matchid")?)),
            indexed("verifiedat", Val::U64(u("verifiedat")?)),
            indexed("match", Val::Str(s("match")?)),
            indexed("name", Val::Str(s("name")?)),
            indexed("compilerversion", Val::Str(s("compilerversion")?)),
            indexed("language", Val::Str(s("language")?)),
            indexed("evmversion", Val::Str(s("evmversion")?)),
            indexed(
                "optimizer",
                Val::Bool(a["optimizer"]["value"].as_bool().context("optimizer")?),
            ),
            indexed(
                "viair",
                Val::Bool(a["viair"]["value"].as_bool().context("viair")?),
            ),
            indexed("deployer", Val::Addr(fixed(&s("deployer")?)?)),
            indexed("blocknumber", Val::U64(u("blocknumber")?)),
            indexed("abihash", Val::Bytes32(fixed(&s("abihash")?)?)),
            indexed("compilationref", Val::Key(compilation)),
        ];
        if let Some(runs) = runs {
            attrs.push(indexed(
                "optimizerruns",
                Val::I32(i32::try_from(runs["value"].as_i64().context("runs")?)?),
            ));
        }
        for name in ["creationcodehash", "runtimecodehash"] {
            if a.contains_key(name) {
                attrs.push(indexed(name, Val::Bytes32(fixed(&s(name)?)?)));
            }
        }
        if a.contains_key("isproxy") {
            attrs.push(indexed(
                "isproxy",
                Val::Bool(a["isproxy"]["value"].as_bool().context("isproxy")?),
            ));
            attrs.push(indexed("proxytype", Val::Str(s("proxytype")?)));
        }
        for (attr, key) in code_refs {
            attrs.push(stored(attr, Val::Key(key)));
        }
        attrs.extend(self.place(vec![("data", encode(&json_bytes(&data)))], &[])?);
        self.create("contract", attrs)?;
        Ok(())
    }
    pub fn entities(&self) -> &[UdcEntity] {
        &self.out
    }
}
/// Hex spelling of `bytecode` under a v3 code encoding (prefix flag, uppercase offsets).
fn spell(bytecode: &[u8], encoding: &Value) -> Result<String> {
    let mut text = hex::encode(bytecode).into_bytes();
    let mut used = HashSet::new();
    for n in encoding["uppercase"]
        .as_array()
        .context("hex case encoding")?
    {
        let i = usize::try_from(n.as_u64().context("hex case offset")?)?;
        ensure!(used.insert(i), "duplicate hex case offset");
        let c = text.get_mut(i).context("hex case offset out of bounds")?;
        ensure!(c.is_ascii_lowercase(), "invalid hex uppercase offset");
        c.make_ascii_uppercase();
    }
    let prefix = encoding["prefix"]
        .as_bool()
        .context("hex prefix encoding")?;
    Ok(format!(
        "{}{}",
        if prefix { "0x" } else { "" },
        String::from_utf8(text)?
    ))
}

/// One dependency read during hydration: entity kind, key and the attributes used.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadStep {
    pub kind: String,
    pub key: [u8; 32],
    pub attributes: Vec<String>,
}

/// Reconstructs contracts. `fetch` returns the entity with a key (from a node query or a
/// local list); the reader checks owner, kind and every digest before using a value.
pub struct Reader<'a> {
    pub prefix: String,
    pub owner: Option<[u8; 20]>,
    fetch: &'a mut dyn FnMut(&[u8; 32]) -> Result<UdcEntity>,
    cache: HashMap<[u8; 32], UdcEntity>,
    pub reads: Vec<ReadStep>,
}
impl<'a> Reader<'a> {
    pub fn new(
        prefix: &str,
        owner: Option<[u8; 20]>,
        fetch: &'a mut dyn FnMut(&[u8; 32]) -> Result<UdcEntity>,
    ) -> Self {
        Self {
            prefix: prefix.into(),
            owner,
            fetch,
            cache: HashMap::new(),
            reads: vec![],
        }
    }
    fn entity(&mut self, key: &[u8; 32], kind: &str, attribute: &str) -> Result<UdcEntity> {
        let entity = match self.cache.get(key) {
            Some(e) => e.clone(),
            None => {
                let e = (self.fetch)(key)?;
                ensure!(&e.key == key, "fetched another key");
                self.cache.insert(*key, e.clone());
                e
            }
        };
        self.check(&entity, kind)?;
        match self.reads.iter_mut().find(|r| &r.key == key) {
            Some(r) if !r.attributes.iter().any(|a| a == attribute) => {
                r.attributes.push(attribute.into())
            }
            Some(_) => {}
            None => self.reads.push(ReadStep {
                kind: kind.into(),
                key: *key,
                attributes: vec![attribute.into()],
            }),
        }
        Ok(entity)
    }
    fn check(&self, e: &UdcEntity, kind: &str) -> Result<()> {
        ensure!(
            e.schema() == Some(&format!("{}.{kind}", self.prefix)),
            "entity is not a {kind}"
        );
        if let Some(owner) = self.owner {
            ensure!(e.owner == owner, "entity has another owner");
        }
        Ok(())
    }
    /// A bytes attribute's decoded value, joining and verifying chunks.
    fn value(&mut self, e: &UdcEntity, name: &str) -> Result<Option<Vec<u8>>> {
        let Some(raw) = e.bytes(name)? else {
            return Ok(None);
        };
        let raw = raw.to_vec();
        let joined = match chunk_manifest(&raw)? {
            None => raw,
            Some((digest, len, keys)) => {
                let mut joined = Vec::with_capacity(len);
                for key in keys {
                    let chunk = self.entity(&key, "chunk", "data")?;
                    joined.extend_from_slice(chunk.bytes("data")?.context("chunk data")?);
                }
                ensure!(
                    joined.len() == len && sha(&joined) == digest,
                    "chunk integrity mismatch"
                );
                joined
            }
        };
        Ok(Some(decode(&joined)?))
    }
    fn code(&mut self, key: &[u8; 32], hash: &[u8; 32]) -> Result<Vec<u8>> {
        let e = self.entity(key, "code", "code")?;
        ensure!(
            e.bytes32_attr("hash")?.as_ref() == Some(hash),
            "code hash attribute mismatch"
        );
        let code = self.value(&e, "code")?.context("code value")?;
        ensure!(&keccak(&code) == hash, "code hash mismatch");
        Ok(code)
    }
    /// The complete contract (`fields` None) or minimal fields plus the selected top-level fields.
    pub fn hydrate(&mut self, contract: &UdcEntity, fields: Option<&[&str]>) -> Result<Value> {
        self.check(contract, "contract")?;
        let want = |f: &str| fields.is_none_or(|fs| fs.contains(&f));
        let raw = self.value(contract, "data")?.context("contract data")?;
        let mut data: Value = serde_json::from_slice(&raw)?;
        ensure!(data.is_object(), "contract data must be an object");
        // Query attributes are projections of the stored record: they must agree with it.
        let address: [u8; 20] = fixed(data["address"].as_str().context("address")?)?;
        ensure!(
            contract.get("address") == Some(&Val::Addr(address)),
            "address attribute mismatch"
        );
        let match_id = match &data["matchId"] {
            Value::String(s) => s.parse::<u64>()?,
            other => other.as_u64().context("matchId")?,
        };
        ensure!(
            contract.get("matchid") == Some(&Val::U64(match_id)),
            "matchid attribute mismatch"
        );
        for (lane, attr, hash_attr) in ONCHAIN {
            if !want(lane) {
                continue;
            }
            if let Some(key) = contract.key_attr(attr)? {
                let hash = contract
                    .bytes32_attr(hash_attr)?
                    .context("code hash attribute")?;
                let code = self.code(&key, &hash)?;
                insert(
                    &mut data,
                    &format!("/{lane}/onchainBytecode"),
                    json!(format!("0x{}", hex::encode(code))),
                )?;
            }
        }
        let needs_compilation = TOP_FIELDS.iter().chain(&LANES).any(|f| want(f));
        if needs_compilation {
            let key = contract
                .key_attr("compilationref")?
                .context("compilationref")?;
            let mut comp = Compilation::load(self, &key)?;
            // The contract's abihash is the digest of the compilation's abi attribute.
            if let (Some(abi), Some(hash)) =
                (comp.digests.get("abi"), contract.bytes32_attr("abihash")?)
            {
                ensure!(
                    abi == &json!(hx(hash)),
                    "abihash does not match the compilation"
                );
            }
            for field in TOP_FIELDS {
                if want(field) && comp.exists(self, &format!("/{field}"))? {
                    let value = comp
                        .expand(self, &format!("/{field}"), 0)?
                        .context("missing compilation field")?;
                    insert(&mut data, &format!("/{}", escape(field)), value)?;
                }
            }
            for lane in LANES {
                if !want(lane) {
                    continue;
                }
                for f in LANE_FIELDS {
                    let path = format!("/{lane}/{f}");
                    if comp.exists(self, &path)? {
                        let value = comp.expand(self, &path, 0)?.context("missing lane field")?;
                        insert(&mut data, &path, value)?;
                    }
                }
            }
        }
        if let Some(fields) = fields {
            let keep: HashSet<&str> = MINIMAL
                .iter()
                .copied()
                .chain(fields.iter().copied())
                .collect();
            data.as_object_mut()
                .unwrap()
                .retain(|k, _| keep.contains(k.as_str()));
        }
        Ok(data)
    }
}
/// Fields every response carries.
pub const MINIMAL: [&str; 7] = [
    "match",
    "creationMatch",
    "runtimeMatch",
    "chainId",
    "address",
    "verifiedAt",
    "matchId",
];
fn escape(s: &str) -> String {
    s.replace('~', "~0").replace('/', "~1")
}

/// A compilation being expanded: its manifest, and its attribute groups loaded on first use.
struct Compilation {
    entity: UdcEntity,
    refs: Vec<Value>,
    digests: Map<String, Value>,
    sources: Vec<[u8; 32]>,
    source_keys: Vec<[u8; 32]>,
    codes: Map<String, Value>,
    groups: HashMap<String, Option<Value>>,
    texts: HashMap<usize, String>,
    hexes: HashMap<String, String>,
}
impl Compilation {
    fn load(reader: &mut Reader, key: &[u8; 32]) -> Result<Self> {
        let entity = reader.entity(key, "compilation", "manifest")?;
        let manifest_bytes = reader.value(&entity, "manifest")?.context("manifest")?;
        ensure!(
            entity.bytes32_attr("hash")? == Some(sha(&manifest_bytes)),
            "compilation hash mismatch"
        );
        let manifest: Value = serde_json::from_slice(&manifest_bytes)?;
        ensure!(
            json_bytes(&manifest) == manifest_bytes,
            "manifest is not canonical"
        );
        let refs = manifest["refs"]
            .as_array()
            .context("manifest refs")?
            .clone();
        let sources: Vec<[u8; 32]> = manifest["sources"]
            .as_array()
            .context("manifest sources")?
            .iter()
            .map(|s| fixed(s.as_str().context("source hash")?))
            .collect::<Result<_>>()?;
        let mut paths = HashSet::new();
        for r in &refs {
            let path = r["path"].as_str().context("reference path")?;
            validate_pointer(path, false)?;
            ensure!(paths.insert(path), "duplicate reference path");
            match r["kind"].as_str().context("reference kind")? {
                "local" => {
                    let pointer = r["pointer"].as_str().context("local pointer")?;
                    validate_pointer(pointer, false)?;
                    let std = |s: &str| within(s, "/stdJsonInput") || within(s, "/stdJsonOutput");
                    ensure!(
                        std(path) && !std(pointer),
                        "invalid or cyclic local reference"
                    );
                }
                "source" => {
                    let i = r["source"].as_u64().context("source index")? as usize;
                    ensure!(i < sources.len(), "source index out of range");
                }
                "code" => {
                    let slot = r["code"].as_str().context("code slot")?;
                    ensure!(manifest["codes"].get(slot).is_some(), "unknown code slot");
                }
                kind => bail!("unsupported reference kind {kind}"),
            }
        }
        for path in &paths {
            let mut parent = *path;
            while let Some((prefix, _)) = parent.rsplit_once('/') {
                ensure!(!paths.contains(prefix), "overlapping references");
                parent = prefix;
            }
        }
        if !entity.unselected.iter().any(|n| n == "sourcerefs") {
            ensure!(
                entity.bytes("sourcerefs")?.is_some() != sources.is_empty(),
                "sourcerefs do not match the manifest"
            );
        }
        Ok(Self {
            entity,
            refs,
            digests: manifest["digests"].as_object().context("digests")?.clone(),
            sources,
            source_keys: vec![],
            codes: manifest["codes"].as_object().context("codes")?.clone(),
            groups: HashMap::new(),
            texts: HashMap::new(),
            hexes: HashMap::new(),
        })
    }
    /// The group attribute holding top-level `field`.
    fn group(field: &str) -> &str {
        if HOT.contains(&field) {
            field
        } else {
            "artifacts"
        }
    }
    /// The stored (stripped) value at `pointer`, loading its group.
    fn stored(&mut self, reader: &mut Reader, pointer: &str) -> Result<Option<Value>> {
        let field = pointer
            .strip_prefix('/')
            .context("pointer below the compilation root")?
            .split('/')
            .next()
            .unwrap()
            .replace("~1", "/")
            .replace("~0", "~");
        let group = Self::group(&field).to_string();
        if !self.groups.contains_key(&group) {
            let key = self.entity.key;
            reader.entity(&key, "compilation", &group)?;
            let raw = reader.value(&self.entity, &group)?;
            let value = match raw {
                None => {
                    ensure!(!self.digests.contains_key(&group), "group {group} missing");
                    None
                }
                Some(raw) => {
                    let digest = self.digests.get(&group).context("group digest")?;
                    ensure!(
                        digest == &json!(hx(sha(&raw))),
                        "group {group} digest mismatch"
                    );
                    Some(serde_json::from_slice(&raw)?)
                }
            };
            self.groups.insert(group.clone(), value);
        }
        let Some(value) = &self.groups[&group] else {
            return Ok(None);
        };
        let local = if HOT.contains(&field.as_str()) {
            // Hot groups hold the field's value itself.
            let rest = &pointer[1 + escape(&field).len()..];
            value.pointer(rest)
        } else {
            value.pointer(pointer)
        };
        Ok(local.cloned())
    }
    fn exists(&mut self, reader: &mut Reader, pointer: &str) -> Result<bool> {
        Ok(self.refs.iter().any(|r| r["path"] == pointer)
            || self.stored(reader, pointer)?.is_some())
    }
    /// v3 `expand_at`: the subtree at `pointer` with every reference inside it restored,
    /// non-local references first, each in manifest order.
    fn expand(
        &mut self,
        reader: &mut Reader,
        pointer: &str,
        depth: usize,
    ) -> Result<Option<Value>> {
        ensure!(depth < 64, "reference depth exceeded");
        validate_pointer(pointer, true)?;
        let refs = self.refs.clone();
        for r in &refs {
            let path = r["path"].as_str().unwrap();
            if within(pointer, path) {
                let value = self.reference(reader, r, depth)?;
                return Ok(value.pointer(&pointer[path.len()..]).cloned());
            }
        }
        let Some(mut data) = self.stored(reader, pointer)? else {
            ensure!(
                !refs
                    .iter()
                    .any(|r| within(r["path"].as_str().unwrap(), pointer)),
                "reference parent missing"
            );
            return Ok(None);
        };
        for r in refs
            .iter()
            .filter(|r| r["kind"] != "local")
            .chain(refs.iter().filter(|r| r["kind"] == "local"))
        {
            let path = r["path"].as_str().unwrap();
            if within(path, pointer) {
                let value = self.reference(reader, r, depth)?;
                insert(&mut data, &path[pointer.len()..], value)?;
            }
        }
        Ok(Some(data))
    }
    fn reference(&mut self, reader: &mut Reader, r: &Value, depth: usize) -> Result<Value> {
        let path = r["path"].as_str().unwrap();
        let (parent, _) = path.rsplit_once('/').context("reference path")?;
        // The destination must be free in the stored value of its parent.
        if parent.is_empty() {
            ensure!(
                self.stored(reader, path)?.is_none(),
                "reference overwrites literal"
            );
        } else {
            let parent_value = self
                .stored(reader, parent)?
                .context("reference parent missing")?;
            validate_destination(
                &json!({"p": parent_value}),
                &format!("/p{}", &path[parent.len()..]),
            )?;
        }
        match r["kind"].as_str().unwrap() {
            "local" => self
                .expand(reader, r["pointer"].as_str().unwrap(), depth + 1)?
                .context("missing local field"),
            "source" => {
                let i = r["source"].as_u64().unwrap() as usize;
                if let Some(text) = self.texts.get(&i) {
                    return Ok(json!(text));
                }
                if self.source_keys.is_empty() {
                    let key = self.entity.key;
                    reader.entity(&key, "compilation", "sourcerefs")?;
                    let raw = reader
                        .value(&self.entity, "sourcerefs")?
                        .context("sourcerefs")?;
                    ensure!(
                        raw.len() == 32 * self.sources.len(),
                        "sourcerefs do not match the manifest"
                    );
                    self.source_keys = raw.chunks(32).map(|k| k.try_into().unwrap()).collect();
                }
                let e = reader.entity(&self.source_keys[i], "source", "content")?;
                let hash = self.sources[i];
                ensure!(
                    e.bytes32_attr("hash")? == Some(hash),
                    "source hash attribute mismatch"
                );
                let text =
                    String::from_utf8(reader.value(&e, "content")?.context("source content")?)?;
                ensure!(sha(&text) == hash, "source hash mismatch");
                self.texts.insert(i, text.clone());
                Ok(json!(text))
            }
            "code" => {
                let slot = r["code"].as_str().unwrap().to_string();
                let lower = match self.hexes.get(&slot) {
                    Some(h) => h.clone(),
                    None => {
                        let hash: [u8; 32] =
                            fixed(self.codes[&slot].as_str().context("code hash")?)?;
                        let key = self
                            .entity
                            .key_attr(&format!("{slot}coderef"))?
                            .context("code reference attribute")?;
                        let code = reader.code(&key, &hash)?;
                        let h = hex::encode(code);
                        self.hexes.insert(slot.clone(), h.clone());
                        h
                    }
                };
                Ok(json!(spell(&hex::decode(lower)?, &r["encoding"])?))
            }
            kind => bail!("unsupported reference kind {kind}"),
        }
    }
}

/// Human-readable view of an entity: bytes attributes decoded (JSON, text, hex or chunk list).
pub fn decoded_view(e: &UdcEntity) -> Value {
    let mut v = e.to_json();
    for a in v["attributes"].as_array_mut().unwrap() {
        if a["type"] != "bytes" {
            continue;
        }
        let raw = unhex(a["value"].as_str().unwrap()).unwrap();
        let name = a["name"].as_str().unwrap().to_string();
        a["value"] = json!(format!("<{} bytes>", raw.len()));
        a["codec"] = json!(match raw.first() {
            Some(&IDENTITY) => "identity",
            Some(&DEFLATE) => "deflate-raw",
            Some(&CHUNKED) => "chunked",
            _ => "raw",
        });
        let chunk = name == "data" && e.schema().is_some_and(|s| s.ends_with(".chunk"));
        if chunk {
            a["codec"] = json!("raw slice");
        }
        let decoded = match chunk_manifest(&raw) {
            _ if chunk => json!(format!("<{} bytes of a chunked value>", raw.len())),
            Ok(Some((digest, len, keys))) => {
                json!({"sha256":hx(digest),"bytes":len,"chunks":keys.iter().map(hx).collect::<Vec<_>>()})
            }
            _ => match decode(&raw) {
                Ok(bytes) if name == "sourcerefs" => {
                    json!(bytes.chunks(32).map(hx).collect::<Vec<_>>())
                }
                Ok(bytes) if name == "code" => json!(hx(&bytes)),
                Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
                    Ok(json) if name != "content" => json,
                    _ => json!(String::from_utf8_lossy(&bytes)),
                },
                Err(err) => json!(format!("<undecodable: {err}>")),
            },
        };
        a["decoded"] = decoded;
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{canonical, hydrate, transform};

    fn profile() -> Profile {
        Profile {
            prefix: "sourcify.v4".into(),
            chain_id: 7706815,
            owner: [0x11; 20],
        }
    }
    fn roundtrip(record: &Value) -> (Value, Vec<UdcEntity>) {
        let v3 = transform(record).unwrap();
        let expected = hydrate::hydrate(v3.last().unwrap(), &v3).unwrap();
        let mut w = Writer::new(profile(), 0);
        let entities = w.contract(&v3).unwrap();
        let contract = entities.last().unwrap().clone();
        let mut fetch = |k: &[u8; 32]| {
            entities
                .iter()
                .find(|e| &e.key == k)
                .cloned()
                .context("missing")
        };
        let mut reader = Reader::new("sourcify.v4", Some([0x11; 20]), &mut fetch);
        let full = reader.hydrate(&contract, None).unwrap();
        assert_eq!(full, canonical(record), "value equality");
        // Same bytes, key order included, as v3 hydration.
        assert_eq!(
            serde_json::to_string(&full).unwrap(),
            serde_json::to_string(&expected).unwrap()
        );
        (full, entities)
    }
    fn fixture() -> Value {
        let abi = json!([{"type":"function","name":"a_function_name_long_enough_to_make_its_abi_a_useful_local_reference","inputs":[],"outputs":[]}]);
        json!({
            "chainId":"1", "address":"0x1111111111111111111111111111111111111111",
            "matchId":"1", "verifiedAt":"2020-01-01T00:00:00Z", "match":"exact_match",
            "abi": abi,
            "compilation":{"language":"Solidity","compiler":"solc","compilerVersion":"1","compilerSettings":{"optimizer":{"enabled":true,"runs":"200"}},"fullyQualifiedName":"A.sol:A","name":"A"},
            "sources":{"A.sol":{"content":"contract A {}"},"~/odd.sol":{"content":"contract A {}"}},
            "creationBytecode":{"onchainBytecode":null,"recompiledBytecode":null},
            "runtimeBytecode":{"onchainBytecode":"0xABcd","recompiledBytecode":"0xaBcD"},
            "stdJsonInput":{"sources":{"~/odd.sol":{"content":"contract A {}"}}},
            "stdJsonOutput":{"contracts":{"~/odd.sol":{"A":{"abi":abi,"evm":{"bytecode":{"object":"aBcD"}}}}}},
            "metadata":[null,"contract A {}","0xABcd",{"~key/":"contract A {}"}],
            "unknown~/field":{"literal":{"$ref":"not a reference"},"nested":[null,"contract A {}"]}
        })
    }

    #[test]
    fn tricky_fixture_roundtrips_like_v3() {
        let (_, entities) = roundtrip(&fixture());
        for e in &entities {
            e.validate().unwrap();
        }
        let kinds: Vec<_> = entities
            .iter()
            .map(|e| e.schema().unwrap().rsplit('.').next().unwrap().to_string())
            .collect();
        assert_eq!(kinds.last().unwrap(), "contract");
        // Non-canonical on-chain spelling (0xABcd) stays literal: no runtimecoderef.
        assert!(entities.last().unwrap().get("runtimecoderef").is_none());
    }

    #[test]
    fn processor_fixtures_roundtrip() {
        let base = json!({"chainId":"1","address":"0x1111111111111111111111111111111111111111","matchId":"1","verifiedAt":"2020-01-01T00:00:00Z","abi":[],"compilation":{"language":"Solidity","compiler":"solc","compilerVersion":"1","compilerSettings":{},"fullyQualifiedName":"A.sol:A"},"sources":{"A.sol":{"content":"contract A {}"}},"creationBytecode":{"onchainBytecode":null,"recompiledBytecode":null},"runtimeBytecode":{"onchainBytecode":"0x6000","recompiledBytecode":"0x6000"}});
        roundtrip(&base);
        let mut r = base.clone();
        r["sources"] = Value::Null;
        roundtrip(&r);
        let mut r = base.clone();
        r["chainId"] = json!(1);
        r["matchId"] = json!(9007199254740993u64);
        r["deployment"] = Value::Null;
        r["proxyResolution"] = Value::Null;
        r["metadata"] = json!({"nullable":null, "nested":[{"future":true}]});
        r["signatures"] = json!({"function":[{"signature":"f()"}], "error":[]});
        r["stdJsonInput"] = json!({"sources":{"A.sol":{"content":"contract A {}"}}});
        r["stdJsonOutput"] = json!({"future":{"values":[null,false,0,"",[]]}});
        r["futureField"] = json!({"unsigned":u64::MAX, "signed":i64::MIN, "fraction":0.125});
        r["$spill"] = json!({"hash":"source-owned-value", "parts":1, "bytes":7});
        roundtrip(&r);
        let mut r = base.clone();
        r["sources"]["~odd/path.sol"] = json!({"content":"contract A {}","$ref":{"literal":true}});
        r["runtimeBytecode"]["recompiledBytecode"] = json!("0xaBcD");
        r["runtimeBytecode"]["onchainBytecode"] = json!("0xABcd");
        r["creationBytecode"]["onchainBytecode"] = json!("0x");
        r["metadata"] = json!("  { \"exact\": true }\n");
        r["abi"] = json!([{"type":"function","name":"hello_this_function_name_is_deliberately_long_to_exceed_reference_manifest_size","inputs":[],"outputs":[]}]);
        r["stdJsonInput"] = json!({"sources":{"A.sol":{"content":"contract A {}"}}});
        r["stdJsonOutput"] = json!({"contracts":{"A.sol":{"A":{"abi":r["abi"],"evm":{"bytecode":{"object":"aBcD"}}}}}});
        r["literal"] = json!({"$ref":"literal","$spill":{"parts":1},"schema":"sourcify.compilation.v3","references":[]});
        roundtrip(&r);
        let numbers: Value = serde_json::from_str("[184467440737095516160001,0.12345678901234567890123456789,1e400,-184467440737095516160001]").unwrap();
        let mut r = base.clone();
        r["compilation"]["future"] = numbers.clone();
        r["abi"] = json!([{"future":numbers.clone()}]);
        r["unknown"] = numbers.clone();
        let (full, _) = roundtrip(&r);
        assert_eq!(full["unknown"].to_string(), numbers.to_string());
    }

    #[test]
    fn large_values_chunk_and_verify() {
        let mut r = fixture();
        // Incompressible content larger than one entity.
        let noise: String = (0..400_000u32)
            .map(|i| char::from(b'a' + (keccak(&i.to_be_bytes())[0] % 26)))
            .collect();
        r["sources"]["A.sol"]["content"] = json!(noise.clone());
        r["metadata"] = json!(noise.repeat(2));
        let (_, entities) = roundtrip(&r);
        let chunks = entities
            .iter()
            .filter(|e| e.schema() == Some("sourcify.v4.chunk"))
            .count();
        assert!(chunks >= 4, "chunks={chunks}");
        // A tampered chunk fails the read.
        let mut tampered = entities.clone();
        let i = tampered
            .iter()
            .position(|e| e.schema() == Some("sourcify.v4.chunk"))
            .unwrap();
        if let Some(Attr {
            value: Val::Bytes(b),
            ..
        }) = tampered[i].attributes.iter_mut().find(|a| a.name == "data")
        {
            b[0] ^= 1;
        }
        let contract = tampered.last().unwrap().clone();
        let mut fetch = |k: &[u8; 32]| {
            tampered
                .iter()
                .find(|e| &e.key == k)
                .cloned()
                .context("missing")
        };
        let mut reader = Reader::new("sourcify.v4", None, &mut fetch);
        assert!(reader.hydrate(&contract, None).is_err());
    }

    #[test]
    fn shared_content_is_created_once() {
        let a = fixture();
        let mut b = a.clone();
        b["address"] = json!("0x2222222222222222222222222222222222222222");
        b["matchId"] = json!("2");
        let mut w = Writer::new(profile(), 0);
        let first = w.contract(&transform(&a).unwrap()).unwrap();
        let second = w.contract(&transform(&b).unwrap()).unwrap();
        assert!(first.len() > 2);
        assert_eq!(second.len(), 1, "only the second contract is new");
        assert_eq!(
            second[0].get("compilationref"),
            first.last().unwrap().get("compilationref")
        );
    }

    #[test]
    fn selected_fields_read_only_what_they_need() {
        let record = fixture();
        let v3 = transform(&record).unwrap();
        let mut w = Writer::new(profile(), 0);
        let entities = w.contract(&v3).unwrap();
        let contract = entities.last().unwrap().clone();
        let mut fetch = |k: &[u8; 32]| {
            entities
                .iter()
                .find(|e| &e.key == k)
                .cloned()
                .context("missing")
        };
        let mut reader = Reader::new("sourcify.v4", None, &mut fetch);
        let selected = reader
            .hydrate(&contract, Some(&["abi", "compilation"]))
            .unwrap();
        assert_eq!(selected["abi"], record["abi"]);
        assert_eq!(selected["compilation"], canonical(&record["compilation"]));
        assert!(selected.get("sources").is_none());
        // Only the compilation's manifest, abi and compilation attributes were used.
        assert_eq!(reader.reads.len(), 1);
        let mut used = reader.reads[0].attributes.clone();
        used.sort();
        assert_eq!(used, ["abi", "compilation", "manifest"]);
        let minimal = Reader::new("sourcify.v4", None, &mut |_| bail!("no reads"))
            .hydrate(&contract, Some(&[]))
            .unwrap();
        // The fixture has no creationMatch or runtimeMatch.
        assert_eq!(minimal.as_object().unwrap().len(), 5);
    }

    #[test]
    fn tampering_is_detected() {
        let record = fixture();
        let mut w = Writer::new(profile(), 0);
        let entities = w.contract(&transform(&record).unwrap()).unwrap();
        for target in ["source", "compilation", "code"] {
            let mut bad = entities.clone();
            let i = bad
                .iter()
                .position(|e| e.schema() == Some(&format!("sourcify.v4.{target}")))
                .unwrap();
            let attr = bad[i]
                .attributes
                .iter_mut()
                .find(|a| matches!(a.value, Val::Bytes(_)) && a.name != "sourcerefs")
                .unwrap();
            let Val::Bytes(b) = &mut attr.value else {
                unreachable!()
            };
            *b = encode(b"{\"tampered\":true}");
            let contract = bad.last().unwrap().clone();
            let mut fetch =
                |k: &[u8; 32]| bad.iter().find(|e| &e.key == k).cloned().context("missing");
            let mut reader = Reader::new("sourcify.v4", None, &mut fetch);
            assert!(reader.hydrate(&contract, None).is_err(), "{target}");
        }
    }

    #[test]
    fn json_roundtrip() {
        let mut w = Writer::new(profile(), 5);
        for e in w.contract(&transform(&fixture()).unwrap()).unwrap() {
            assert_eq!(UdcEntity::from_json(&e.to_json()).unwrap(), e);
        }
    }
}
