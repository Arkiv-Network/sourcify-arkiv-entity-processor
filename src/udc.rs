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
//! DEFLATE (RFC 1951), 2 chunk list. Digests cover uncompressed bytes, so compression and
//! chunking never change identities.
//!
//! The decomposition is the v3 processor's (`split.rs`): this module re-lays out its logical
//! envelopes, so hydration restores exactly what v3 hydration restores, in the same key order.
use crate::hydrate::{insert, payload_with, validate_destination, validate_pointer, within};
use crate::split::json_bytes;
use crate::{hx, sha, Entity};
use anyhow::{bail, ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use flate2::{read::DeflateDecoder, write::DeflateEncoder, Compression};
use serde_json::{json, Map, Value};
use sha3::{Digest, Keccak256};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::{Read, Write};
use std::rc::Rc;

/// Bytes values one entity may carry together. Below the node's 128 KiB so that a create also
/// fits a reth transaction (a reth pool refuses transactions over 128 KiB by default) with its
/// ABI framing.
pub const BUDGET: usize = 120_000;
/// Bytes in one chunk.
pub const CHUNK: usize = 120_000;
/// Bound on one value, decoded or encoded, on write and on read.
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
const SLOTS: [&str; 2] = ["creation", "runtime"];

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
/// An indexed `str` projection: control characters (which the node refuses in a `str`) become
/// U+FFFD, and more than 128 bytes become a 125-byte prefix and an ellipsis, as in v3.
pub fn index_str(s: &str) -> String {
    let clean: String = s
        .chars()
        .map(|c| {
            if c <= '\u{1f}' || c == '\u{7f}' {
                '\u{fffd}'
            } else {
                c
            }
        })
        .collect();
    if clean.len() <= 128 {
        return clean;
    }
    let mut n = 125;
    while !clean.is_char_boundary(n) {
        n -= 1;
    }
    format!("{}…", &clean[..n])
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
    /// An attribute that may be absent; an unselected one is an error, never an absence.
    fn attr(&self, name: &str) -> Result<Option<&Val>> {
        match self.get(name) {
            None if self.unselected.iter().any(|n| n == name) => {
                bail!("attribute {name} of {} was not selected", hx(self.key))
            }
            found => Ok(found),
        }
    }
    fn bytes(&self, name: &str) -> Result<Option<&[u8]>> {
        match self.attr(name)? {
            None => Ok(None),
            Some(Val::Bytes(b)) => Ok(Some(b)),
            Some(v) => bail!("attribute {name} is {}, expected bytes", v.tag()),
        }
    }
    fn key_attr(&self, name: &str) -> Result<Option<[u8; 32]>> {
        match self.attr(name)? {
            None => Ok(None),
            Some(Val::Key(k)) => Ok(Some(*k)),
            Some(v) => bail!("attribute {name} is {}, expected key", v.tag()),
        }
    }
    fn bytes32_attr(&self, name: &str) -> Result<Option<[u8; 32]>> {
        match self.attr(name)? {
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
        let mut flags: HashMap<&str, bool> = HashMap::new();
        if let Some(schema) = v.get("attributeSchema") {
            for s in schema.as_array().context("attributeSchema")? {
                let name = s["name"].as_str().context("schema name")?;
                let flag = s["indexed"].as_bool().context("schema indexed")?;
                ensure!(
                    flags.insert(name, flag).is_none(),
                    "duplicate attribute {name}"
                );
            }
        }
        let mut attributes = vec![];
        let mut names = HashSet::new();
        for a in v["attributes"].as_array().context("attributes")? {
            let name = a["name"].as_str().context("attribute name")?;
            ensure!(names.insert(name), "duplicate attribute {name}");
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
            .filter(|n| !names.contains(*n))
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

/// The kind of entity a `key` attribute names.
pub fn key_target(attribute: &str) -> Option<&'static str> {
    match attribute {
        "compilationref" => Some("compilation"),
        "creationcoderef" | "runtimecoderef" => Some("code"),
        _ => None,
    }
}
impl UdcEntity {
    /// One line of a v4 bundle (`entities/NNNNNN.jsonl`): `nonce`, `key`, `expiresAt` and the
    /// attributes in name order, each `{name, type, indexed, value}` in the node's RPC encoding
    /// except `bytes`, which is standard base64.
    pub fn to_line(&self, nonce: u64) -> Value {
        json!({
            "nonce": nonce,
            "key": hx(self.key),
            "expiresAt": format!("0x{:x}", self.expires_at),
            "attributes": self.attributes.iter().map(|a| json!({
                "name": a.name,
                "type": a.value.tag(),
                "indexed": a.indexed,
                "value": match &a.value {
                    Val::Bytes(b) => json!(STANDARD.encode(b)),
                    v => v.to_json(),
                },
            })).collect::<Vec<_>>(),
        })
    }
    /// Reads a bundle line written by [`UdcEntity::to_line`] (the owner comes from seed.json).
    pub fn from_line(line: &Value, owner: [u8; 20]) -> Result<(u64, Self)> {
        let object = line.as_object().context("bundle line must be an object")?;
        ensure!(
            object.len() == 4
                && ["nonce", "key", "expiresAt", "attributes"]
                    .iter()
                    .all(|k| object.contains_key(*k)),
            "bundle line must have exactly nonce, key, expiresAt and attributes"
        );
        let nonce = line["nonce"].as_u64().context("nonce")?;
        let mut attributes = vec![];
        for a in line["attributes"].as_array().context("attributes")? {
            ensure!(
                a.as_object().is_some_and(|o| o.len() == 4),
                "attribute must have exactly name, type, indexed and value"
            );
            let tag = a["type"].as_str().context("attribute type")?;
            let value = match tag {
                "bytes" => Val::Bytes(
                    STANDARD
                        .decode(a["value"].as_str().context("bytes value must be base64")?)
                        .context("invalid base64")?,
                ),
                _ => Val::from_json(tag, &a["value"])?,
            };
            attributes.push(Attr {
                name: a["name"].as_str().context("attribute name")?.into(),
                indexed: a["indexed"].as_bool().context("indexed flag")?,
                value,
            });
        }
        let entity = Self {
            key: fixed(line["key"].as_str().context("key")?)?,
            owner,
            expires_at: quantity(line["expiresAt"].as_str().context("expiresAt")?)?,
            attributes,
            unselected: vec![],
        };
        Ok((nonce, entity))
    }
    /// Every key the entity names, with the kind the named entity must have: its `key`
    /// attributes, its `sourcerefs` (codec 0) and the chunks of its chunk lists.
    pub fn references(&self) -> Result<Vec<([u8; 32], &'static str)>> {
        let chunk = self.schema().is_some_and(|s| s.ends_with(".chunk"));
        let mut out = vec![];
        for a in &self.attributes {
            match &a.value {
                Val::Key(k) => out.push((
                    *k,
                    key_target(&a.name)
                        .with_context(|| format!("unknown key attribute {}", a.name))?,
                )),
                Val::Bytes(b) if !chunk => {
                    if a.name == "sourcerefs" {
                        // Keys stay readable: sourcerefs is never chunked (re-keying rewrites it).
                        let (&codec, body) = b.split_first().context("empty sourcerefs")?;
                        ensure!(
                            codec == IDENTITY && body.len().is_multiple_of(32) && !body.is_empty(),
                            "malformed sourcerefs"
                        );
                        out.extend(body.chunks(32).map(|k| (k.try_into().unwrap(), "source")));
                    } else if let Some((_, _, keys)) = chunk_manifest(b)? {
                        out.extend(keys.into_iter().map(|k| (k, "chunk")));
                    }
                }
                _ => {}
            }
        }
        Ok(out)
    }
    /// The kind named by `arkiv.schema` under `prefix` (`contract`, `compilation`, ...).
    pub fn kind<'s>(&'s self, prefix: &str) -> Option<&'s str> {
        self.schema()?
            .strip_prefix(prefix)?
            .strip_prefix('.')
            .filter(|k| KINDS.contains(k))
    }
}
/// The five entity kinds.
pub const KINDS: [&str; 5] = ["contract", "compilation", "source", "code", "chunk"];
/// Every attribute a kind may carry besides `arkiv.schema` (an indexed `str` on every entity):
/// kind, name, type, indexed, required. This is the format's schema (docs/udc-proposal.md).
pub const ATTRIBUTES: &[(&str, &str, &str, bool, bool)] = &[
    ("contract", "abihash", "bytes32", true, true),
    ("contract", "address", "addr", true, true),
    ("contract", "blocknumber", "u64", true, true),
    ("contract", "chainid", "u64", true, true),
    ("contract", "compilationhash", "bytes32", true, true),
    ("contract", "compilationref", "key", false, true),
    ("contract", "compilerversion", "str", true, true),
    ("contract", "creationcodehash", "bytes32", true, false),
    ("contract", "creationcoderef", "key", false, false),
    ("contract", "data", "bytes", false, true),
    ("contract", "deployer", "addr", true, true),
    ("contract", "evmversion", "str", true, true),
    ("contract", "isproxy", "bool", true, false),
    ("contract", "language", "str", true, true),
    ("contract", "match", "str", true, true),
    ("contract", "matchid", "u64", true, true),
    ("contract", "name", "str", true, true),
    ("contract", "optimizer", "bool", true, true),
    ("contract", "optimizerruns", "i32", true, true),
    ("contract", "proxytype", "str", true, false),
    ("contract", "runtimecodehash", "bytes32", true, false),
    ("contract", "runtimecoderef", "key", false, false),
    ("contract", "verifiedat", "u64", true, true),
    ("contract", "viair", "bool", true, true),
    ("compilation", "abi", "bytes", false, true),
    ("compilation", "artifacts", "bytes", false, false),
    ("compilation", "compilation", "bytes", false, true),
    ("compilation", "creationcoderef", "key", false, false),
    ("compilation", "hash", "bytes32", true, true),
    ("compilation", "manifest", "bytes", false, true),
    ("compilation", "runtimecoderef", "key", false, false),
    ("compilation", "sourcerefs", "bytes", false, false),
    ("source", "content", "bytes", false, true),
    ("source", "hash", "bytes32", true, true),
    ("code", "code", "bytes", false, true),
    ("code", "hash", "bytes32", true, true),
    ("chunk", "data", "bytes", false, true),
];
impl UdcEntity {
    /// Checks the entity against [`ATTRIBUTES`] and the node's rules; returns its kind.
    pub fn check_schema(&self, prefix: &str) -> Result<&'static str> {
        self.validate()?;
        let tag = self.schema().context("no arkiv.schema")?;
        let kind = KINDS
            .iter()
            .copied()
            .find(|k| tag.strip_prefix(prefix).and_then(|t| t.strip_prefix('.')) == Some(k))
            .with_context(|| format!("arkiv.schema {tag} is not a {prefix} kind"))?;
        for a in &self.attributes {
            if a.name == "arkiv.schema" {
                ensure!(a.indexed, "arkiv.schema must be indexed");
                continue;
            }
            let spec = ATTRIBUTES
                .iter()
                .find(|(k, n, _, _, _)| *k == kind && *n == a.name)
                .with_context(|| format!("{kind} has no attribute {}", a.name))?;
            ensure!(
                spec.2 == a.value.tag() && spec.3 == a.indexed,
                "{kind} attribute {} must be {}{}",
                a.name,
                if spec.3 { "indexed " } else { "stored " },
                spec.2
            );
        }
        for (_, name, ..) in ATTRIBUTES
            .iter()
            .filter(|(k, .., required)| *k == kind && *required)
        {
            ensure!(self.get(name).is_some(), "{kind} is missing {name}");
        }
        Ok(kind)
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

/// A bytes value: raw DEFLATE when that is smaller, else identity. At most `MAX_VALUE` bytes.
pub fn encode(raw: &[u8]) -> Result<Vec<u8>> {
    ensure!(
        raw.len() <= MAX_VALUE,
        "value of {} bytes exceeds the {MAX_VALUE}-byte limit",
        raw.len()
    );
    let mut z = DeflateEncoder::new(vec![DEFLATE], Compression::best());
    z.write_all(raw)?;
    let deflated = z.finish()?;
    if deflated.len() < raw.len() + 1 {
        Ok(deflated)
    } else {
        let mut v = Vec::with_capacity(raw.len() + 1);
        v.push(IDENTITY);
        v.extend_from_slice(raw);
        Ok(v)
    }
}
/// Decodes an identity or DEFLATE value; chunk lists are resolved by the reader first.
pub fn decode(value: &[u8]) -> Result<Vec<u8>> {
    let (&codec, body) = value.split_first().context("empty bytes value")?;
    match codec {
        IDENTITY => {
            ensure!(body.len() <= MAX_VALUE, "value exceeds limit");
            Ok(body.to_vec())
        }
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
/// Digest and length of the joined value, and the chunk keys in order.
type ChunkList = ([u8; 32], usize, Vec<[u8; 32]>);
/// A chunk list: `02 ‖ sha256(joined) ‖ u32 length ‖ keys`, bounded before anything is fetched.
fn chunk_manifest(value: &[u8]) -> Result<Option<ChunkList>> {
    if value.first() != Some(&CHUNKED) {
        return Ok(None);
    }
    ensure!(
        value.len() >= 37 && (value.len() - 37).is_multiple_of(32),
        "malformed chunk list"
    );
    let digest: [u8; 32] = value[1..33].try_into()?;
    let len = u32::from_be_bytes(value[33..37].try_into()?) as usize;
    ensure!(
        len > 0 && len <= MAX_VALUE + 1,
        "chunk list length out of range"
    );
    let keys: Vec<[u8; 32]> = value[37..]
        .chunks(32)
        .map(|k| k.try_into().unwrap())
        .collect();
    ensure!(keys.len() == len.div_ceil(CHUNK), "chunk count mismatch");
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
/// Keys are those the node assigns to creates with salt 0, starting at `next_nonce`. A contract
/// is written all or nothing: on an error the writer is left as it was before it. The writer
/// keeps only the content-hash-to-key maps; each contract's new entities are handed back.
pub struct Writer {
    pub profile: Profile,
    pub next_nonce: u64,
    sources: HashMap<[u8; 32], [u8; 32]>,
    codes: HashMap<[u8; 32], [u8; 32]>,
    compilations: HashMap<[u8; 32], [u8; 32]>,
    journal: Vec<(u8, [u8; 32])>,
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
            journal: vec![],
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
        let entity = UdcEntity {
            key,
            owner: self.profile.owner,
            expires_at: PERMANENT,
            attributes,
            unselected: vec![],
        };
        entity.validate()?;
        self.next_nonce += 1;
        self.out.push(entity);
        Ok(key)
    }
    /// Encoded bytes fields as attributes. While the entity is over budget, the largest
    /// chunkable value moves into chunk entities (created first) and keeps a chunk list; `pinned`
    /// values move last. `sourcerefs` never moves: it holds keys, and a chunk list's digest would
    /// bind them, so re-keying a bundle for another chain or owner could not rewrite them.
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
                // A chunk list costs 37 bytes plus a key per chunk: only larger values move.
                .filter(|(_, f)| !f.2 && f.1.len() > 37 + 32 && f.0 != "sourcerefs")
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
        let mut attrs = self.place(vec![("content", encode(content.as_bytes())?)], &[])?;
        attrs.push(indexed("hash", Val::Bytes32(hash)));
        let key = self.create("source", attrs)?;
        self.sources.insert(hash, key);
        self.journal.push((0, hash));
        Ok(key)
    }
    fn code(&mut self, bytecode: &[u8]) -> Result<[u8; 32]> {
        let hash = keccak(bytecode);
        if let Some(key) = self.codes.get(&hash) {
            return Ok(*key);
        }
        let mut attrs = self.place(vec![("code", encode(bytecode)?)], &[])?;
        attrs.push(indexed("hash", Val::Bytes32(hash)));
        let key = self.create("code", attrs)?;
        self.codes.insert(hash, key);
        self.journal.push((1, hash));
        Ok(key)
    }
    /// Re-lays out the v3 entities of one contract (`crate::transform` output, dependencies
    /// first, the verified contract last). Returns the entities created for it; shared content
    /// created for an earlier contract is referenced, not repeated. All or nothing.
    pub fn contract(&mut self, v3: &[Entity]) -> Result<Vec<UdcEntity>> {
        let nonce = self.next_nonce;
        self.out.clear();
        self.journal.clear();
        match self.contract_inner(v3) {
            Ok(()) => Ok(std::mem::take(&mut self.out)),
            Err(e) => {
                self.out.clear();
                self.next_nonce = nonce;
                for (map, hash) in std::mem::take(&mut self.journal) {
                    match map {
                        0 => self.sources.remove(&hash),
                        1 => self.codes.remove(&hash),
                        _ => self.compilations.remove(&hash),
                    };
                }
                Err(e)
            }
        }
    }
    fn contract_inner(&mut self, v3: &[Entity]) -> Result<()> {
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
                    let comp = compilation.clone().context("contract before compilation")?;
                    self.verified(e, env, comp, runs.as_ref(), &code_bytes, &source_text)?;
                }
                kind => bail!("unsupported v3 kind {kind}"),
            }
        }
        Ok(())
    }
    /// Returns the compilation's key and manifest hash, and the v3 fingerprint the contract's
    /// references name.
    fn compilation(&mut self, env: Value) -> Result<Comp> {
        ensure!(
            env["schema"] == "sourcify.compilation.v3",
            "compilation schema"
        );
        let v3hash = hx(sha(json_bytes(&env)));
        let data = env["data"].as_object().context("compilation data")?;
        let refs = env["references"].as_array().context("references")?;
        // The recompiled bytecodes get the slot of their lane; any other code a reference names
        // (a lane can be referenced away, or its hex can equal a source text) takes a free slot.
        let mut slots: Vec<(&str, String)> = vec![];
        for (slot, lane) in SLOTS.iter().zip(LANES) {
            let path = format!("/{lane}/recompiledBytecode");
            if let Some(r) = refs
                .iter()
                .find(|r| r["path"] == path.as_str() && r["kind"] == "code")
            {
                slots.push((slot, r["hash"].as_str().context("code hash")?.into()));
            }
        }
        for r in refs.iter().filter(|r| r["kind"] == "code") {
            let hash = r["hash"].as_str().context("code hash")?;
            if !slots.iter().any(|(_, h)| h == hash) {
                let free = SLOTS
                    .iter()
                    .find(|s| !slots.iter().any(|(t, _)| t == *s))
                    .context("more than two codes in one compilation")?;
                slots.push((free, hash.into()));
            }
        }
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
                    let slot = slots.iter().find(|(_, h)| h == hash).unwrap().0;
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
            return Ok(Comp {
                key: *key,
                hash,
                v3hash,
            });
        }
        let mut fields: Vec<(&str, Vec<u8>)> = vec![("manifest", encode(&manifest_bytes)?)];
        for (n, b) in &group_bytes {
            fields.push((n, encode(b)?));
        }
        if !sources.is_empty() {
            let mut keys = vec![IDENTITY];
            for s in &sources {
                let hash: [u8; 32] = fixed(s)?;
                keys.extend_from_slice(self.sources.get(&hash).context("source not created")?);
            }
            fields.push(("sourcerefs", keys));
        }
        let mut attrs = self.place(fields, &["manifest"])?;
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
        self.journal.push((2, hash));
        Ok(Comp { key, hash, v3hash })
    }
    fn verified(
        &mut self,
        e: &Entity,
        env: Value,
        comp: Comp,
        runs: Option<&Value>,
        codes: &HashMap<String, Vec<u8>>,
        sources: &HashMap<String, String>,
    ) -> Result<()> {
        ensure!(
            env["schema"] == "sourcify.verified-contract.v3",
            "contract schema"
        );
        let mut data = env["data"].clone();
        let refs = env["references"].as_array().context("references")?;
        let parent = |p: &str| p.rsplit_once('/').map(|x| x.0.to_string());
        let mut code_refs: Vec<(&str, [u8; 32])> = vec![];
        for (i, r) in refs.iter().enumerate() {
            let path = r["path"].as_str().context("reference path")?;
            match r["kind"].as_str().context("reference kind")? {
                "compilation" => {
                    ensure!(
                        r["hash"] == comp.v3hash.as_str(),
                        "contract names another compilation"
                    );
                    ensure!(r["pointer"] == path, "compilation reference moves a field");
                    let field = path.trim_start_matches('/');
                    let known = TOP_FIELDS.contains(&field)
                        || LANES
                            .iter()
                            .any(|lane| LANE_FIELDS.iter().any(|f| field == format!("{lane}/{f}")));
                    ensure!(known, "unexpected compilation field {path}");
                }
                "code" => {
                    let hash = r["hash"].as_str().context("code hash")?;
                    let bytecode = codes.get(hash).context("code missing")?;
                    let canonical = r["encoding"] == json!({"prefix":true,"uppercase":[]});
                    // A reference restores its value at the end of its parent, after every
                    // earlier reference there; the reader restores on-chain code first, so only
                    // a reference no later sibling follows can become a code attribute.
                    let last_in_parent = !refs[i + 1..].iter().any(|later| {
                        later["kind"] != "compilation"
                            && later["path"].as_str().and_then(parent) == parent(path)
                    });
                    match ONCHAIN
                        .iter()
                        .find(|(lane, _, _)| path == format!("/{lane}/onchainBytecode"))
                    {
                        Some((_, attr, _)) if canonical && last_in_parent => {
                            code_refs.push((
                                attr,
                                *self.codes.get(&fixed::<32>(hash)?).context("code key")?,
                            ));
                        }
                        // Anything else stays literal contract data, in v3's order.
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
        let b = |name: &str| a[name]["value"].as_bool().with_context(|| name.to_string());
        let mut attrs = vec![
            indexed("address", Val::Addr(fixed(&s("address")?)?)),
            indexed("chainid", Val::U64(u("chainid")?)),
            indexed("matchid", Val::U64(u("matchid")?)),
            indexed("verifiedat", Val::U64(u("verifiedat")?)),
            indexed("match", Val::Str(index_str(&s("match")?))),
            indexed("name", Val::Str(index_str(&s("name")?))),
            indexed(
                "compilerversion",
                Val::Str(index_str(&s("compilerversion")?)),
            ),
            indexed("language", Val::Str(index_str(&s("language")?))),
            indexed("evmversion", Val::Str(index_str(&s("evmversion")?))),
            indexed("optimizer", Val::Bool(b("optimizer")?)),
            indexed("viair", Val::Bool(b("viair")?)),
            indexed("deployer", Val::Addr(fixed(&s("deployer")?)?)),
            indexed("blocknumber", Val::U64(u("blocknumber")?)),
            indexed("abihash", Val::Bytes32(fixed(&s("abihash")?)?)),
            indexed("compilationhash", Val::Bytes32(comp.hash)),
            stored("compilationref", Val::Key(comp.key)),
        ];
        if let Some(runs) = runs {
            let n = runs["value"].as_i64().context("runs")?;
            attrs.push(indexed("optimizerruns", Val::I32(i32::try_from(n)?)));
        }
        for name in ["creationcodehash", "runtimecodehash"] {
            if a.contains_key(name) {
                attrs.push(indexed(name, Val::Bytes32(fixed(&s(name)?)?)));
            }
        }
        if a.contains_key("isproxy") {
            attrs.push(indexed("isproxy", Val::Bool(b("isproxy")?)));
            attrs.push(indexed("proxytype", Val::Str(index_str(&s("proxytype")?))));
        }
        for (attr, key) in code_refs {
            attrs.push(stored(attr, Val::Key(key)));
        }
        // Insertion order, not canonical order: literals restored above keep v3's position.
        attrs.extend(self.place(vec![("data", encode(&serde_json::to_vec(&data)?)?)], &[])?);
        self.create("contract", attrs)?;
        Ok(())
    }
    /// Entities created so far, by kind: (sources, codes, compilations).
    pub fn distinct(&self) -> (usize, usize, usize) {
        (
            self.sources.len(),
            self.codes.len(),
            self.compilations.len(),
        )
    }
}
#[derive(Clone)]
struct Comp {
    key: [u8; 32],
    hash: [u8; 32],
    v3hash: String,
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

/// The attributes a fetch must return: all of them, or the named ones. A node still lists the
/// others in `attributeSchema`, so a reader never mistakes an unselected attribute for an absent
/// one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Select {
    All,
    Named(Vec<String>),
}

/// Fetches one entity by key.
pub type FetchOne<'a> = dyn FnMut(&[u8; 32]) -> Result<UdcEntity> + 'a;
/// Fetches several entities by key with one selection. A transport batches the keys (at most 8
/// per query); every requested key must come back exactly once.
pub type FetchMany<'a> = dyn FnMut(&[[u8; 32]], &Select) -> Result<Vec<UdcEntity>> + 'a;
enum Source<'a> {
    One(&'a mut FetchOne<'a>),
    Many(&'a mut FetchMany<'a>),
}

/// Reconstructs contracts. Entities come from `fetch` (node queries or a local list); the reader
/// checks owner, kind and every digest before using a value. It fetches in rounds: the contract's
/// chunks, compilation and on-chain code; then what the selected fields' references need
/// (sources, recompiled code, chunks); then their chunks. A key missed by a round is fetched on
/// its own; an attribute a narrow selection left out is fetched again with every attribute.
pub struct Reader<'a> {
    pub prefix: String,
    pub owner: Option<[u8; 20]>,
    source: Source<'a>,
    cache: HashMap<[u8; 32], UdcEntity>,
    pub reads: Vec<ReadStep>,
    /// Fetch calls made and keys fetched in them.
    pub fetches: usize,
    pub fetched: usize,
    /// Called before each fetch, value decode and reference expansion (deadlines, cancellation).
    check: Option<&'a mut (dyn FnMut() -> Result<()> + 'a)>,
}
impl<'a> Reader<'a> {
    /// Calls `check` before each fetch, value decode and reference expansion; its error stops
    /// the hydration (a deadline or a cancelled request).
    pub fn with_check(mut self, check: &'a mut (dyn FnMut() -> Result<()> + 'a)) -> Self {
        self.check = Some(check);
        self
    }
    fn tick(&mut self) -> Result<()> {
        match &mut self.check {
            Some(check) => check(),
            None => Ok(()),
        }
    }
    pub fn new(prefix: &str, owner: Option<[u8; 20]>, fetch: &'a mut FetchOne<'a>) -> Self {
        Self::with(prefix, owner, Source::One(fetch))
    }
    pub fn batched(prefix: &str, owner: Option<[u8; 20]>, fetch: &'a mut FetchMany<'a>) -> Self {
        Self::with(prefix, owner, Source::Many(fetch))
    }
    fn with(prefix: &str, owner: Option<[u8; 20]>, source: Source<'a>) -> Self {
        Self {
            prefix: prefix.into(),
            owner,
            source,
            cache: HashMap::new(),
            reads: vec![],
            fetches: 0,
            fetched: 0,
            check: None,
        }
    }
    /// Fetches, in one call, the keys not cached yet.
    pub fn prefetch(&mut self, keys: &[[u8; 32]], select: &Select) -> Result<()> {
        let mut seen = HashSet::new();
        let want: Vec<[u8; 32]> = keys
            .iter()
            .filter(|k| !self.cache.contains_key(*k) && seen.insert(**k))
            .copied()
            .collect();
        if want.is_empty() {
            return Ok(());
        }
        self.fetch(&want, select)
    }
    fn fetch(&mut self, keys: &[[u8; 32]], select: &Select) -> Result<()> {
        self.tick()?;
        self.fetches += 1;
        self.fetched += keys.len();
        let got = match &mut self.source {
            Source::One(fetch) => keys.iter().map(fetch).collect::<Result<Vec<_>>>()?,
            Source::Many(fetch) => fetch(keys, select)?,
        };
        let wanted: HashSet<&[u8; 32]> = keys.iter().collect();
        let mut returned = HashSet::new();
        for e in &got {
            ensure!(
                wanted.contains(&e.key) && returned.insert(e.key),
                "fetched another key"
            );
        }
        ensure!(returned.len() == keys.len(), "fetch left out a key");
        for e in got {
            self.cache.insert(e.key, e);
        }
        Ok(())
    }
    /// The cached entity with `names` selected, fetched again with every attribute if a narrow
    /// selection left one out. Records no read.
    fn selected(&mut self, key: &[u8; 32], names: &[&str]) -> Result<UdcEntity> {
        let stale = match self.cache.get(key) {
            None => true,
            Some(e) => names.iter().any(|n| e.unselected.iter().any(|u| u == n)),
        };
        if stale {
            self.fetch(&[*key], &Select::All)?;
        }
        Ok(self.cache[key].clone())
    }
    fn entity(&mut self, key: &[u8; 32], kind: &str, attribute: &str) -> Result<UdcEntity> {
        let entity = self.selected(key, &[attribute])?;
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
        self.tick()?;
        let Some(raw) = e.bytes(name)? else {
            return Ok(None);
        };
        let raw = raw.to_vec();
        let joined = match chunk_manifest(&raw)? {
            None => raw,
            Some((digest, len, keys)) => {
                self.prefetch(&keys, &Select::All)?;
                let mut joined = Vec::with_capacity(len);
                for key in keys {
                    let chunk = self.entity(&key, "chunk", "data")?;
                    let data = chunk.bytes("data")?.context("chunk data")?;
                    ensure!(
                        data.len() <= CHUNK && joined.len() + data.len() <= len,
                        "chunk size mismatch"
                    );
                    joined.extend_from_slice(data);
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
    /// Chunk keys of the cached entities' named values (for the next fetch round).
    fn chunks_of(&self, keys: &[[u8; 32]], name: &str) -> Vec<[u8; 32]> {
        keys.iter()
            .filter_map(|k| self.cache.get(k))
            .flat_map(|e| chunk_keys(e, name))
            .collect()
    }
    /// The complete contract (`fields` None) or minimal fields plus the selected top-level fields.
    pub fn hydrate(&mut self, contract: &UdcEntity, fields: Option<&[&str]>) -> Result<Value> {
        self.check(contract, "contract")?;
        let want = |f: &str| fields.is_none_or(|fs| fs.contains(&f));
        let lanes: Vec<&str> = LANES.iter().copied().filter(|l| want(l)).collect();
        let top: Vec<&str> = TOP_FIELDS.iter().copied().filter(|f| want(f)).collect();
        let needs_compilation = !top.is_empty() || !lanes.is_empty();
        // First round: the contract's chunks, its on-chain code and its compilation. A read of
        // `abi` and `compilation` alone selects only the attributes it uses.
        // A chunk list is bounded before anything is fetched.
        let mut keys = match contract.bytes("data")? {
            Some(raw) => chunk_manifest(raw)?.map(|c| c.2).unwrap_or_default(),
            None => vec![],
        };
        let mut onchain = vec![];
        for (lane, attr, _) in ONCHAIN {
            if want(lane) {
                onchain.extend(contract.key_attr(attr)?);
            }
        }
        keys.extend(&onchain);
        let compilation = match needs_compilation {
            true => Some(
                contract
                    .key_attr("compilationref")?
                    .context("compilationref")?,
            ),
            false => None,
        };
        let hot = lanes.is_empty() && top.iter().all(|f| HOT.contains(f));
        match compilation {
            Some(key) if hot => {
                self.prefetch(&keys, &Select::All)?;
                let mut names: Vec<String> = ["arkiv.schema", "hash", "manifest"]
                    .iter()
                    .chain(&top)
                    .map(|n| n.to_string())
                    .collect();
                names.dedup();
                self.prefetch(&[key], &Select::Named(names))?;
            }
            Some(key) => {
                keys.push(key);
                self.prefetch(&keys, &Select::All)?;
            }
            None => self.prefetch(&keys, &Select::All)?,
        }
        let more = self.chunks_of(&onchain, "code");
        self.prefetch(&more, &Select::All)?;
        let raw = self.value(contract, "data")?.context("contract data")?;
        let mut data: Value = serde_json::from_slice(&raw)?;
        ensure!(data.is_object(), "contract data must be an object");
        project(contract, &data)?;
        for (lane, attr, hash_attr) in ONCHAIN {
            let hash = contract.bytes32_attr(hash_attr)?;
            let key = contract.key_attr(attr)?;
            if key.is_none() {
                // Without a reference the code hash describes literal data, never nothing.
                if let Some(hash) = hash {
                    let literal = data
                        .pointer(&format!("/{lane}/onchainBytecode"))
                        .and_then(Value::as_str)
                        .context("code hash without code reference or literal")?;
                    let bytes = hex::decode(literal.trim_start_matches("0x"))?;
                    ensure!(keccak(&bytes) == hash, "literal code hash mismatch");
                }
                continue;
            }
            if !want(lane) {
                continue;
            }
            let key = key.unwrap();
            let hash = hash.context("code reference without code hash")?;
            let code = self.code(&key, &hash)?;
            insert(
                &mut data,
                &format!("/{lane}/onchainBytecode"),
                json!(format!("0x{}", hex::encode(code))),
            )?;
        }
        if let Some(key) = compilation {
            let hash = contract
                .bytes32_attr("compilationhash")?
                .context("compilationhash")?;
            let mut comp = Compilation::load(self, &key, &hash)?;
            let mut pointers: Vec<String> = top.iter().map(|f| format!("/{}", escape(f))).collect();
            for lane in &lanes {
                pointers.extend(LANE_FIELDS.iter().map(|f| format!("/{lane}/{f}")));
            }
            comp.prefetch(self, &pointers)?;
            let abihash = contract.bytes32_attr("abihash")?;
            for field in &top {
                let pointer = format!("/{}", escape(field));
                if comp.exists(self, &pointer)? {
                    let value = comp
                        .expand(self, &pointer, 0)?
                        .context("missing compilation field")?;
                    if *field == "abi" {
                        let abihash = abihash.context("abihash")?;
                        ensure!(
                            sha(json_bytes(&value)) == abihash,
                            "abihash does not match the compilation"
                        );
                    }
                    if *field == "compilation" {
                        project_compilation(contract, &value)?;
                    }
                    insert(&mut data, &pointer, value)?;
                }
            }
            for lane in &lanes {
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
/// The chunk keys of a selected bytes value that is a chunk list (none otherwise).
fn chunk_keys(e: &UdcEntity, name: &str) -> Vec<[u8; 32]> {
    match e.get(name) {
        Some(Val::Bytes(b)) => chunk_manifest(b)
            .ok()
            .flatten()
            .map(|(_, _, keys)| keys)
            .unwrap_or_default(),
        _ => vec![],
    }
}
/// The contract's query attributes are projections of its record: those the stored data
/// determines must agree with it.
fn project(contract: &UdcEntity, data: &Value) -> Result<()> {
    let address: [u8; 20] = fixed(data["address"].as_str().context("address")?)?;
    ensure!(
        contract.attr("address")? == Some(&Val::Addr(address)),
        "address attribute mismatch"
    );
    let number = |v: &Value| match v {
        Value::String(s) => s.parse::<u64>().ok(),
        other => other.as_u64(),
    };
    for (field, attr) in [("matchId", "matchid"), ("chainId", "chainid")] {
        let n = number(&data[field]).with_context(|| format!("{field} is not a number"))?;
        ensure!(
            contract.attr(attr)? == Some(&Val::U64(n)),
            "{attr} attribute mismatch"
        );
    }
    let at =
        chrono::DateTime::parse_from_rfc3339(data["verifiedAt"].as_str().context("verifiedAt")?)?
            .timestamp();
    ensure!(
        contract.attr("verifiedat")? == Some(&Val::U64(u64::try_from(at)?)),
        "verifiedat attribute mismatch"
    );
    let matched = index_str(data["match"].as_str().unwrap_or("null"));
    ensure!(
        contract.attr("match")? == Some(&Val::Str(matched)),
        "match attribute mismatch"
    );
    // The deployment projections, as the v3 transform derives them.
    let deployment = data
        .get("deployment")
        .filter(|d| d.is_object())
        .cloned()
        .unwrap_or_else(|| json!({}));
    let block = match &deployment["blockNumber"] {
        Value::Null => 0,
        other => crate::decimal(other)?.parse()?,
    };
    ensure!(
        contract.attr("blocknumber")? == Some(&Val::U64(block)),
        "blocknumber attribute mismatch"
    );
    let deployer: [u8; 20] = fixed(crate::string(
        &deployment["deployer"],
        "0x0000000000000000000000000000000000000000",
    ))?;
    ensure!(
        contract.attr("deployer")? == Some(&Val::Addr(deployer)),
        "deployer attribute mismatch"
    );
    Ok(())
}
/// The compiler projections (`name`, `compilerversion`, `language`, `evmversion`, `optimizer`,
/// `optimizerruns`, `viair`) must agree with the hydrated `compilation` field, as the v3
/// transform derives them.
fn project_compilation(contract: &UdcEntity, compilation: &Value) -> Result<()> {
    let derived = crate::compiler_attrs(&json!({ "compilation": compilation }), true);
    for (name, a) in &derived {
        if name == "compiler" {
            continue;
        }
        let want = match a["type"].as_str() {
            Some("str") => Val::Str(index_str(a["value"].as_str().unwrap_or_default())),
            Some("bool") => Val::Bool(a["value"].as_bool().unwrap_or_default()),
            Some("i32") => Val::I32(i32::try_from(a["value"].as_i64().unwrap_or_default())?),
            other => bail!("unexpected compiler attribute type {other:?}"),
        };
        ensure!(
            contract.attr(name)? == Some(&want),
            "{name} attribute does not match the compilation"
        );
    }
    Ok(())
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
fn unescape(s: &str) -> String {
    s.replace("~1", "/").replace("~0", "~")
}

/// A compilation being expanded: its manifest, and its attribute groups loaded on first use.
struct Compilation {
    key: [u8; 32],
    refs: Rc<Vec<Value>>,
    digests: Map<String, Value>,
    sources: Vec<[u8; 32]>,
    source_keys: Vec<[u8; 32]>,
    codes: Map<String, Value>,
    groups: HashMap<String, Option<Value>>,
    texts: HashMap<usize, String>,
    hexes: HashMap<String, String>,
}
impl Compilation {
    fn load(reader: &mut Reader, key: &[u8; 32], expected: &[u8; 32]) -> Result<Self> {
        let entity = reader.entity(key, "compilation", "manifest")?;
        let hash = entity.bytes32_attr("hash")?.context("compilation hash")?;
        ensure!(&hash == expected, "the contract names another compilation");
        let manifest_bytes = reader.value(&entity, "manifest")?.context("manifest")?;
        ensure!(sha(&manifest_bytes) == hash, "compilation hash mismatch");
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
            key: entity.key,
            refs: Rc::new(refs),
            digests: manifest["digests"].as_object().context("digests")?.clone(),
            sources,
            source_keys: vec![],
            codes: manifest["codes"].as_object().context("codes")?.clone(),
            groups: HashMap::new(),
            texts: HashMap::new(),
            hexes: HashMap::new(),
        })
    }
    /// The group attribute holding a top-level field, and the field name.
    fn group_of(pointer: &str) -> Result<(String, String)> {
        let first = pointer
            .strip_prefix('/')
            .context("pointer below the compilation root")?
            .split('/')
            .next()
            .unwrap();
        let field = unescape(first);
        let group = if HOT.contains(&field.as_str()) {
            field.clone()
        } else {
            "artifacts".into()
        };
        Ok((group, field))
    }
    /// Fetches, in rounds, what expanding `pointers` will read: the chunks of the groups (and of
    /// `sourcerefs`) it decodes, then the sources and recompiled code its references name (local
    /// references followed), then their chunks. The expansion itself still checks everything.
    fn prefetch(&mut self, reader: &mut Reader, pointers: &[String]) -> Result<()> {
        let (mut groups, mut sources, mut slots) =
            (BTreeSet::new(), BTreeSet::new(), BTreeSet::new());
        let mut todo: Vec<String> = pointers.to_vec();
        let mut seen = HashSet::new();
        while let Some(p) = todo.pop() {
            if seen.len() > 4096 || !seen.insert(p.clone()) {
                continue;
            }
            groups.extend(Self::group_of(&p).ok().map(|g| g.0));
            for r in self.refs.iter() {
                let path = r["path"].as_str().unwrap_or_default();
                if !(within(path, &p) || within(&p, path)) {
                    continue;
                }
                groups.extend(Self::group_of(path).ok().map(|g| g.0));
                match r["kind"].as_str() {
                    Some("source") => sources.extend(r["source"].as_u64().map(|i| i as usize)),
                    Some("code") => slots.extend(r["code"].as_str().map(String::from)),
                    Some("local") => todo.extend(r["pointer"].as_str().map(String::from)),
                    _ => {}
                }
            }
        }
        let mut names: Vec<String> = groups.iter().cloned().collect();
        if !sources.is_empty() {
            names.push("sourcerefs".into());
        }
        names.extend(slots.iter().map(|s| format!("{s}coderef")));
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let e = reader.selected(&self.key, &refs)?;
        let chunks: Vec<[u8; 32]> = names.iter().flat_map(|n| chunk_keys(&e, n)).collect();
        reader.prefetch(&chunks, &Select::All)?;
        let mut keys = vec![];
        if !sources.is_empty() {
            let source_keys = self.source_keys(reader)?;
            keys.extend(sources.iter().filter_map(|i| source_keys.get(*i)).copied());
        }
        for slot in &slots {
            keys.extend(e.key_attr(&format!("{slot}coderef"))?);
        }
        reader.prefetch(&keys, &Select::All)?;
        let mut more = reader.chunks_of(&keys, "content");
        more.extend(reader.chunks_of(&keys, "code"));
        reader.prefetch(&more, &Select::All)
    }
    /// The source keys (`sourcerefs`), in manifest order.
    fn source_keys(&mut self, reader: &mut Reader) -> Result<Vec<[u8; 32]>> {
        if self.source_keys.is_empty() {
            let key = self.key;
            let e = reader.entity(&key, "compilation", "sourcerefs")?;
            ensure!(
                e.bytes("sourcerefs")?.and_then(|b| b.first()) == Some(&IDENTITY),
                "sourcerefs must be stored inline (codec 0)"
            );
            let raw = reader.value(&e, "sourcerefs")?.context("sourcerefs")?;
            ensure!(
                raw.len() == 32 * self.sources.len(),
                "sourcerefs do not match the manifest"
            );
            self.source_keys = raw.chunks(32).map(|k| k.try_into().unwrap()).collect();
        }
        Ok(self.source_keys.clone())
    }
    /// Loads and verifies the group holding `pointer`.
    fn load_group(&mut self, reader: &mut Reader, pointer: &str) -> Result<()> {
        let (group, _) = Self::group_of(pointer)?;
        if self.groups.contains_key(&group) {
            return Ok(());
        }
        let key = self.key;
        let entity = reader.entity(&key, "compilation", &group)?;
        let value = match reader.value(&entity, &group)? {
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
        self.groups.insert(group, value);
        Ok(())
    }
    /// The loaded group value holding `pointer` and the pointer inside it.
    fn locate(&self, pointer: &str) -> Result<Option<(&Value, String)>> {
        let (group, field) = Self::group_of(pointer)?;
        let Some(Some(value)) = self.groups.get(&group) else {
            return Ok(None);
        };
        Ok(Some(if HOT.contains(&field.as_str()) {
            // A hot group holds the field's value itself.
            (value, pointer[1 + escape(&field).len()..].to_string())
        } else {
            (value, pointer.to_string())
        }))
    }
    /// The stored (stripped) value at `pointer`, loading its group.
    fn stored(&mut self, reader: &mut Reader, pointer: &str) -> Result<Option<Value>> {
        self.load_group(reader, pointer)?;
        Ok(self
            .locate(pointer)?
            .and_then(|(value, inner)| value.pointer(&inner).cloned()))
    }
    fn exists(&mut self, reader: &mut Reader, pointer: &str) -> Result<bool> {
        if self.refs.iter().any(|r| r["path"] == pointer) {
            return Ok(true);
        }
        self.load_group(reader, pointer)?;
        Ok(self
            .locate(pointer)?
            .is_some_and(|(value, inner)| value.pointer(&inner).is_some()))
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
        let refs = Rc::clone(&self.refs);
        for r in refs.iter() {
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
                reader.tick()?;
                let value = self.reference(reader, r, depth)?;
                insert(&mut data, &path[pointer.len()..], value)?;
            }
        }
        Ok(Some(data))
    }
    fn reference(&mut self, reader: &mut Reader, r: &Value, depth: usize) -> Result<Value> {
        let path = r["path"].as_str().unwrap();
        // The destination must be free in the stored value (checked without copying it).
        self.load_group(reader, path)?;
        let (parent, _) = path.rsplit_once('/').context("reference path")?;
        if parent.is_empty() {
            let taken = self
                .locate(path)?
                .is_some_and(|(value, inner)| value.pointer(&inner).is_some());
            ensure!(!taken, "reference overwrites literal");
        } else {
            let (value, inner) = self.locate(path)?.context("reference parent missing")?;
            if inner.is_empty() {
                bail!("reference overwrites literal");
            }
            validate_destination(value, &inner)?;
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
                let source_key = self.source_keys(reader)?[i];
                let e = reader.entity(&source_key, "source", "content")?;
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
                        let attr = format!("{slot}coderef");
                        let compilation = self.key;
                        let key = reader
                            .entity(&compilation, "compilation", &attr)?
                            .key_attr(&attr)?
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

/// Keys per `arkiv_query` when reading by key: eight `OR` groups is our node's planned path (more
/// fall back to a bounded scan), and both nodes accept it.
pub const KEY_BATCH: usize = 8;
/// A dataset's `arkiv.schema` prefix: letters, digits, `.`, `_` and `-`, short enough for every
/// `<prefix>.<kind>` tag to be a valid `str`.
pub fn check_prefix(prefix: &str) -> Result<()> {
    ensure!(
        !prefix.is_empty()
            && prefix.len() <= 116
            && prefix
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)),
        "invalid schema prefix {prefix:?}"
    );
    Ok(())
}
/// The query for the dataset's contract at `address` (read with limit 2: two rows are ambiguous).
pub fn contract_query(prefix: &str, owner: &[u8; 20], address: &[u8; 20]) -> String {
    format!(
        "$owner = addr({}) AND arkiv.schema = str('{prefix}.contract') AND chainid = u64(1) AND address = addr({})",
        hx(owner),
        hx(address)
    )
}
/// The query for entities by key (`$key = key(…) OR …`), at most [`KEY_BATCH`] keys.
pub fn key_query(keys: &[[u8; 32]]) -> String {
    keys.iter()
        .map(|k| format!("$key = key({})", hx(k)))
        .collect::<Vec<_>>()
        .join(" OR ")
}
/// The `select` of an entity read: the entity fields a reader checks plus `attributeSchema` (so
/// unselected attributes are known) and every or the named attributes.
pub fn select_json(select: &Select) -> Value {
    let attributes = match select {
        Select::All => json!(true),
        Select::Named(names) => {
            Value::Object(names.iter().map(|n| (n.clone(), json!(true))).collect())
        }
    };
    json!({"key": true, "owner": true, "creator": true, "expiresAt": true, "attributeSchema": true, "attributes": attributes})
}
/// The entities of an `arkiv_query` result (`data`), refusing a continuation cursor.
pub fn query_entities(result: &Value) -> Result<Vec<UdcEntity>> {
    ensure!(
        result.get("cursor").is_none_or(Value::is_null),
        "query result continues past its limit"
    );
    result["data"]
        .as_array()
        .context("query result has no data array")?
        .iter()
        .map(UdcEntity::from_json)
        .collect()
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
            *b = encode(b"{\"tampered\":true}").unwrap();
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

    /// An `arkiv_query` result with a named attribute selection: unselected attributes are
    /// listed in `attributeSchema` only, and must not read as absent.
    #[test]
    fn partial_selections_are_not_absences() {
        let record = fixture();
        let mut w = Writer::new(profile(), 0);
        let entities = w.contract(&transform(&record).unwrap()).unwrap();
        let rpc = |e: &UdcEntity, keep: &[&str]| {
            let mut v = e.to_json();
            let schema: Vec<Value> = v["attributes"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| json!({"name": a["name"], "type": a["type"], "indexed": a["indexed"]}))
                .collect();
            let attributes: Vec<Value> = v["attributes"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|a| keep.is_empty() || keep.contains(&a["name"].as_str().unwrap()))
                .map(|a| json!({"name": a["name"], "type": a["type"], "value": a["value"]}))
                .collect();
            v["attributeSchema"] = json!(schema);
            v["attributes"] = json!(attributes);
            UdcEntity::from_json(&v).unwrap()
        };
        let hot = ["arkiv.schema", "hash", "manifest", "abi", "compilation"];
        let partial: Vec<UdcEntity> = entities
            .iter()
            .map(|e| {
                if e.schema() == Some("sourcify.v4.compilation") {
                    rpc(e, &hot)
                } else {
                    rpc(e, &[])
                }
            })
            .collect();
        let contract = partial.last().unwrap().clone();
        let mut fetch = |k: &[u8; 32]| {
            partial
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
        let mut fetch = |k: &[u8; 32]| {
            partial
                .iter()
                .find(|e| &e.key == k)
                .cloned()
                .context("missing")
        };
        let error = Reader::new("sourcify.v4", None, &mut fetch)
            .hydrate(&contract, None)
            .unwrap_err();
        assert!(error.to_string().contains("was not selected"), "{error}");
    }

    fn base() -> Value {
        json!({"chainId":"1","address":"0x1111111111111111111111111111111111111111","matchId":"1","verifiedAt":"2020-01-01T00:00:00Z","abi":[],"compilation":{"language":"Solidity","compiler":"solc","compilerVersion":"1","compilerSettings":{},"fullyQualifiedName":"A.sol:A"},"sources":{"A.sol":{"content":"contract A {}"}},"creationBytecode":{"onchainBytecode":null,"recompiledBytecode":null},"runtimeBytecode":{"onchainBytecode":"0x6000","recompiledBytecode":"0x6000"}})
    }
    fn hydrate_all(entities: &[UdcEntity], contract: &UdcEntity) -> Result<Value> {
        let mut fetch = |k: &[u8; 32]| {
            entities
                .iter()
                .find(|e| &e.key == k)
                .cloned()
                .context("missing")
        };
        Reader::new("sourcify.v4", Some([0x11; 20]), &mut fetch).hydrate(contract, None)
    }

    /// Literal contract data restored from v3 references keeps v3's key order.
    #[test]
    fn contract_literals_keep_v3_order() {
        let mut r = base();
        r["unknown"] = json!({"a": "contract A {}", "b": 1});
        roundtrip(&r);
        let mut r = base();
        r["sources"]["B.sol"] = json!({"content": "1"});
        roundtrip(&r);
        let mut r = base();
        r["runtimeBytecode"]["onchainBytecode"] = json!("0x60AA");
        r["runtimeBytecode"]["transformations"] = json!([]);
        roundtrip(&r);
        let mut r = base();
        r["runtimeBytecode"]["zzz"] = json!("0x6000");
        roundtrip(&r);
    }

    /// The abihash covers the hydrated ABI, whatever the ABI shares with sources or code.
    #[test]
    fn abi_strings_equal_to_sources_or_code() {
        let mut r = base();
        r["abi"] = json!([{"type": "function", "name": "contract A {}"}, {"name": "6000"}]);
        roundtrip(&r);
    }

    /// A recompiled bytecode spelled like a source text still finds its code slot.
    #[test]
    fn recompiled_bytecode_spelled_like_a_source() {
        let mut r = base();
        r["sources"]["B.sol"] = json!({"content": "0x6000"});
        r["metadata"] = json!("6000");
        roundtrip(&r);
    }

    #[test]
    fn index_strings_are_valid_str_values() {
        let mut r = base();
        r["compilation"]["name"] = json!("A\nB");
        r["match"] = json!("m".repeat(200));
        let (_, entities) = roundtrip(&r);
        let contract = entities.last().unwrap();
        assert_eq!(contract.get("name"), Some(&Val::Str("A\u{fffd}B".into())));
        assert_eq!(
            contract.get("match"),
            Some(&Val::Str(format!("{}…", "m".repeat(125))))
        );
    }

    /// A contract names its compilation by key and by hash: a swapped key is refused.
    #[test]
    fn contract_is_bound_to_its_compilation() {
        let a = base();
        let mut b = base();
        b["address"] = json!("0x2222222222222222222222222222222222222222");
        b["matchId"] = json!("2");
        b["sources"]["A.sol"]["content"] = json!("contract A { /* other */ }");
        let mut w = Writer::new(profile(), 0);
        let first = w.contract(&transform(&a).unwrap()).unwrap();
        let second = w.contract(&transform(&b).unwrap()).unwrap();
        let all: Vec<UdcEntity> = first.iter().chain(&second).cloned().collect();
        let mut contract_b = all.last().unwrap().clone();
        let comp_a = first
            .last()
            .unwrap()
            .get("compilationref")
            .cloned()
            .unwrap();
        for at in contract_b.attributes.iter_mut() {
            if at.name == "compilationref" {
                at.value = comp_a.clone();
            }
        }
        let error = hydrate_all(&all, &contract_b).unwrap_err();
        assert!(error.to_string().contains("another compilation"), "{error}");
        hydrate_all(&all, all.last().unwrap()).unwrap();
    }

    /// An unselected code reference is an error, not missing code.
    #[test]
    fn unselected_code_reference_is_an_error() {
        let mut w = Writer::new(profile(), 0);
        let entities = w.contract(&transform(&base()).unwrap()).unwrap();
        let mut contract = entities.last().unwrap().clone();
        contract.attributes.retain(|a| a.name != "runtimecoderef");
        contract.unselected.push("runtimecoderef".into());
        let error = hydrate_all(&entities, &contract).unwrap_err();
        assert!(error.to_string().contains("not selected"), "{error}");
        // Without the attribute at all, the code hash still demands the code.
        contract.unselected.clear();
        assert!(hydrate_all(&entities, &contract).is_err());
    }

    /// A chunk list's claimed length is bounded before any chunk is fetched.
    #[test]
    fn chunk_lists_are_bounded_before_fetching() {
        let keys = (131_072 - 37) / 32;
        let mut list = vec![CHUNKED];
        list.extend_from_slice(&[0u8; 32]);
        list.extend_from_slice(&((keys * CHUNK) as u32).to_be_bytes());
        list.extend(std::iter::repeat_n(7u8, keys * 32));
        let contract = UdcEntity {
            key: [9; 32],
            owner: [0x11; 20],
            expires_at: PERMANENT,
            attributes: vec![
                indexed("arkiv.schema", Val::Str("sourcify.v4.contract".into())),
                stored("data", Val::Bytes(list)),
            ],
            unselected: vec![],
        };
        let mut fetched = 0;
        let mut fetch = |_: &[u8; 32]| -> Result<UdcEntity> {
            fetched += 1;
            bail!("no chunk")
        };
        let error = Reader::new("sourcify.v4", None, &mut fetch)
            .hydrate(&contract, None)
            .unwrap_err();
        assert!(error.to_string().contains("out of range"), "{error}");
        assert_eq!(fetched, 0);
    }

    /// The writer refuses values the reader would refuse, and a failed contract leaves no trace.
    #[test]
    fn writer_bounds_values_and_rolls_back() {
        let mut r = base();
        r["sources"]["B.sol"] = json!({"content": "a".repeat(MAX_VALUE + 1)});
        let mut w = Writer::new(profile(), 10);
        let error = w.contract(&transform(&r).unwrap()).unwrap_err();
        assert!(error.to_string().contains("exceeds"), "{error}");
        assert_eq!(w.next_nonce, 10);
        assert_eq!(w.distinct(), (0, 0, 0));
        // A.sol was created before the failure; the next contract must create it again.
        let created = w.contract(&transform(&base()).unwrap()).unwrap();
        assert!(created
            .iter()
            .any(|e| e.schema() == Some("sourcify.v4.source")));
        assert_eq!(created[0].key, entity_key(7706815, &[0x11; 20], 10, 0));
        hydrate_all(&created, created.last().unwrap()).unwrap();
    }

    /// A node's answer to a key batch: the selected attributes, every name in `attributeSchema`.
    fn node_rows(
        store: &[UdcEntity],
        keys: &[[u8; 32]],
        select: &Select,
    ) -> Result<Vec<UdcEntity>> {
        keys.iter()
            .map(|k| {
                let mut e = store
                    .iter()
                    .find(|e| &e.key == k)
                    .cloned()
                    .context("missing")?;
                if let Select::Named(names) = select {
                    let (kept, dropped): (Vec<Attr>, Vec<Attr>) = e
                        .attributes
                        .into_iter()
                        .partition(|a| names.iter().any(|n| n == &a.name));
                    e.attributes = kept;
                    e.unselected = dropped.into_iter().map(|a| a.name).collect();
                }
                Ok(e)
            })
            .collect()
    }

    /// The batched reader: rounds instead of one fetch per entity, the hot read's narrow
    /// selection, and a refetch with every attribute when a narrow selection misses one.
    #[test]
    fn batched_reads_take_rounds_and_select_attributes() {
        let record = fixture();
        let expected = canonical(&record);
        let mut w = Writer::new(profile(), 0);
        let entities = w.contract(&transform(&record).unwrap()).unwrap();
        let contract = entities.last().unwrap().clone();
        let calls: std::cell::RefCell<Vec<(usize, Select)>> = Default::default();
        let mut fetch = |keys: &[[u8; 32]], select: &Select| {
            calls.borrow_mut().push((keys.len(), select.clone()));
            node_rows(&entities, keys, select)
        };
        let full = Reader::batched("sourcify.v4", Some([0x11; 20]), &mut fetch)
            .hydrate(&contract, None)
            .unwrap();
        assert_eq!(full, expected);
        assert!(calls.borrow().len() <= 3, "{:?}", calls.borrow());
        assert!(calls.borrow().iter().all(|(_, s)| *s == Select::All));
        calls.borrow_mut().clear();
        let hot = Reader::batched("sourcify.v4", Some([0x11; 20]), &mut fetch)
            .hydrate(&contract, Some(&["abi", "compilation"]))
            .unwrap();
        assert_eq!(hot["abi"], expected["abi"]);
        let calls = calls.into_inner();
        assert_eq!(calls.len(), 1);
        let Select::Named(names) = &calls[0].1 else {
            panic!("the hot read must select names")
        };
        assert_eq!(
            names,
            &["arkiv.schema", "hash", "manifest", "compilation", "abi"]
        );
        // ABI strings equal to a source text are references into `abi`: the narrow selection
        // misses `sourcerefs`, so the compilation is fetched again with every attribute.
        let mut r = base();
        r["abi"] = json!([{"type": "function", "name": "contract A {}"}]);
        let entities = Writer::new(profile(), 0)
            .contract(&transform(&r).unwrap())
            .unwrap();
        let mut calls = 0;
        let mut fetch = |keys: &[[u8; 32]], select: &Select| {
            calls += 1;
            node_rows(&entities, keys, select)
        };
        let hot = Reader::batched("sourcify.v4", Some([0x11; 20]), &mut fetch)
            .hydrate(entities.last().unwrap(), Some(&["abi"]))
            .unwrap();
        assert_eq!(hot["abi"], canonical(&r)["abi"]);
        assert!(calls >= 2);
    }

    /// The check hook stops a hydration between steps.
    #[test]
    fn the_check_hook_stops_hydration() {
        let mut w = Writer::new(profile(), 0);
        let entities = w.contract(&transform(&fixture()).unwrap()).unwrap();
        let mut fetch = |k: &[u8; 32]| {
            entities
                .iter()
                .find(|e| &e.key == k)
                .cloned()
                .context("missing")
        };
        let mut calls = 0;
        let mut check = || -> Result<()> {
            calls += 1;
            ensure!(calls < 4, "deadline");
            Ok(())
        };
        let error = Reader::new("sourcify.v4", None, &mut fetch)
            .with_check(&mut check)
            .hydrate(entities.last().unwrap(), None)
            .unwrap_err();
        assert!(error.to_string().contains("deadline"), "{error}");
    }

    /// sourcerefs holds keys: the writer never chunks it, and readers refuse a chunked one.
    #[test]
    fn sourcerefs_are_never_chunked() {
        // 3,800 sources: their keys alone exceed the 120,000-byte budget.
        let mut r = base();
        let mut sources = Map::new();
        for i in 0..3800 {
            sources.insert(
                format!("S{i}.sol"),
                json!({ "content": format!("contract S{i} {{}}") }),
            );
        }
        r["sources"] = Value::Object(sources);
        let mut w = Writer::new(profile(), 0);
        let error = w.contract(&transform(&r).unwrap()).unwrap_err();
        assert!(
            error.to_string().contains("cannot fit its budget"),
            "{error}"
        );
        // A chunked sourcerefs from elsewhere is refused by references() and by the reader.
        let mut entities = w.contract(&transform(&base()).unwrap()).unwrap();
        let comp = entities
            .iter_mut()
            .find(|e| e.schema() == Some("sourcify.v4.compilation"))
            .unwrap();
        for a in comp
            .attributes
            .iter_mut()
            .filter(|a| a.name == "sourcerefs")
        {
            let mut list = vec![CHUNKED];
            list.extend_from_slice(&[0; 36]);
            list.extend_from_slice(&[7; 32]);
            a.value = Val::Bytes(list);
        }
        assert!(comp
            .references()
            .unwrap_err()
            .to_string()
            .contains("malformed sourcerefs"));
        assert!(hydrate_all(&entities, entities.last().unwrap()).is_err());
    }

    /// Each reference costs the same however large its parent is (no copy per reference).
    #[test]
    fn many_references_hydrate_linearly() {
        let mut r = base();
        r["metadata"] = json!(vec!["contract A {}"; 16_000]);
        let v3 = transform(&r).unwrap();
        let started = std::time::Instant::now();
        hydrate::hydrate(v3.last().unwrap(), &v3).unwrap();
        let v3_time = started.elapsed();
        let mut w = Writer::new(profile(), 0);
        let entities = w.contract(&v3).unwrap();
        let started = std::time::Instant::now();
        hydrate_all(&entities, entities.last().unwrap()).unwrap();
        let v4_time = started.elapsed();
        assert!(
            v4_time < v3_time * 20 + std::time::Duration::from_secs(1),
            "v3 {v3_time:?}, v4 {v4_time:?}"
        );
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
            &xs[(self.next() % xs.len() as u64) as usize]
        }
        fn chance(&mut self, n: u64) -> bool {
            self.next().is_multiple_of(n)
        }
    }
    const LEAVES: [&str; 14] = [
        "",
        "1",
        "x",
        "contract A {}",
        "0x6000",
        "6000",
        "0x60AA",
        "60aa",
        "0xaBcD",
        "abcd",
        "ABCD",
        "0x",
        "exact_match",
        "a longer string that is nothing special at all",
    ];
    const KEYS: [&str; 12] = [
        "a", "b", "content", "~", "/", "~1", "10", "2", "😀", "\u{ffff}", "abi", "zzz",
    ];
    fn leaf(g: &mut Rng) -> Value {
        match g.next() % 6 {
            0 => Value::Null,
            1 => json!(g.next() % 100),
            2 => json!(true),
            _ => json!(*g.pick(&LEAVES)),
        }
    }
    fn tree(g: &mut Rng, depth: u32) -> Value {
        if depth == 0 || g.chance(3) {
            return leaf(g);
        }
        if g.chance(2) {
            let n = g.next() % 4;
            Value::Array((0..n).map(|_| tree(g, depth - 1)).collect())
        } else {
            let mut m = Map::new();
            for _ in 0..(g.next() % 4) {
                m.insert((*g.pick(&KEYS)).into(), tree(g, depth - 1));
            }
            Value::Object(m)
        }
    }
    /// Records biased towards collisions between sources, code, ABI and other fields.
    fn random_record(g: &mut Rng) -> Value {
        let mut r = base();
        let mut sources = Map::new();
        for i in 0..(1 + g.next() % 3) {
            let content = *g.pick(&["", "1", "x", "contract A {}", "0x6000", "abcd"]);
            sources.insert(format!("{}{i}", g.pick(&KEYS)), json!({"content": content}));
        }
        r["sources"] = Value::Object(sources);
        let code = ["0x6000", "0x60aa", "0x60AA", "0xabcd", "0xABcd"];
        for lane in LANES {
            for field in ["recompiledBytecode", "onchainBytecode"] {
                r[lane][field] = if g.chance(4) {
                    Value::Null
                } else {
                    json!(*g.pick(&code))
                };
            }
            for k in [
                "transformations",
                "transformationValues",
                "zzz",
                "aaa",
                "sourceMap",
                "cborAuxdata",
            ] {
                if g.chance(3) {
                    r[lane][k] = tree(g, 2);
                }
            }
        }
        r["abi"] = Value::Array((0..g.next() % 3).map(|_| tree(g, 3)).collect());
        for f in [
            "metadata",
            "userdoc",
            "devdoc",
            "signatures",
            "sourceIds",
            "storageLayout",
        ] {
            if g.chance(2) {
                r[f] = tree(g, 4);
            }
        }
        if g.chance(2) {
            r["compilation"]["extra"] = tree(g, 3);
        }
        for f in ["stdJsonInput", "stdJsonOutput"] {
            match g.next() % 4 {
                0 => {}
                1 => r[f] = tree(g, 4),
                2 => {
                    let src = *g.pick(&["abi", "metadata", "userdoc", "compilation", "sources"]);
                    let v = r.get(src).cloned().unwrap_or(Value::Null);
                    r[f] = json!({"copy": v, "abi": r["abi"].clone(), "x": [Value::Null, r["sources"].clone()]});
                }
                _ => {
                    r[f] = r
                        .get("metadata")
                        .cloned()
                        .unwrap_or(json!({"m": tree(g, 3)}))
                }
            }
        }
        for f in ["unknown", "deployment", "zfield", "Afield"] {
            if g.chance(3) {
                r[f] = tree(g, 3);
            }
        }
        r
    }

    /// Differential test against v3 hydration: every record v3 accepts rebuilds byte for byte,
    /// through one shared writer (so later contracts reuse earlier sources, code and compilations).
    #[test]
    fn differential_against_v3() {
        let n: u64 = std::env::var("UDC_FUZZ_N")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(2000);
        let mut g = Rng(0x9e37_79b9_7f4a_7c15);
        let mut w = Writer::new(profile(), 0);
        let mut store: HashMap<[u8; 32], UdcEntity> = HashMap::new();
        let mut checked = 0;
        for i in 0..n {
            let mut r = random_record(&mut g);
            r["address"] = json!(format!("0x{:040x}", i + 1));
            r["matchId"] = json!((i + 1).to_string());
            let Ok(v3) = transform(&r) else { continue };
            let Ok(expected) = hydrate::hydrate(v3.last().unwrap(), &v3) else {
                continue;
            };
            let created = w
                .contract(&v3)
                .unwrap_or_else(|e| panic!("writer: {e:#}\n{r}"));
            store.extend(created.iter().map(|e| (e.key, e.clone())));
            let mut fetch = |k: &[u8; 32]| store.get(k).cloned().context("missing");
            let got = Reader::new("sourcify.v4", Some([0x11; 20]), &mut fetch)
                .hydrate(created.last().unwrap(), None)
                .unwrap_or_else(|e| panic!("reader: {e:#}\n{r}"));
            assert_eq!(
                serde_json::to_string(&got).unwrap(),
                serde_json::to_string(&expected).unwrap(),
                "{r}"
            );
            checked += 1;
        }
        assert!(checked > n / 2, "only {checked} records checked");
    }
}
