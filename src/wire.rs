//! Shared decoding of processor wire entities, independent of their transport.
use crate::{hx, sha, Entity, BUDGET};
use anyhow::{bail, ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use flate2::read::GzDecoder;
use serde_json::{json, Map, Value};
use std::io::Read;

pub(crate) fn string<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v[key]
        .as_str()
        .with_context(|| format!("missing string {key}"))
}
pub(crate) fn typed<'a>(v: &'a Value, key: &str, prefix: &str) -> Result<&'a str> {
    string(v, key)?
        .strip_prefix(prefix)
        .with_context(|| format!("invalid {key}: expected {prefix}"))
}

pub(crate) fn decode<F>(wire: &Value, mut resolve_key: F) -> Result<Entity>
where
    F: FnMut(&str) -> Result<[u8; 32]>,
{
    let encoded = STANDARD.decode(string(wire, "$payload")?)?;
    let payload = match typed(wire, "contentencoding", "str:")? {
        "identity" => encoded,
        "gzip" => {
            let mut out = vec![];
            GzDecoder::new(encoded.as_slice())
                .take((BUDGET + 1) as u64)
                .read_to_end(&mut out)?;
            out
        }
        encoding => bail!("unsupported content encoding {encoding}"),
    };
    ensure!(
        payload.len() <= BUDGET,
        "decoded wire payload exceeds limit"
    );
    ensure!(
        typed(wire, "payloadsha256", "bytes32:")? == hx(sha(&payload)),
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
            "key" => {
                json!(hex::encode(resolve_key(raw)?))
            }
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
        typed(wire, "kind", "str:")?,
        payload,
        typed(wire, "originalcontenttype", "str:")?,
        attributes,
    );
    ensure!(
        hx(entity.id) == typed(wire, "stagingid", "bytes32:")?,
        "entity staging identity mismatch"
    );
    Ok(entity)
}
