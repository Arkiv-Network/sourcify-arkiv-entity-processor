//! Stored values retain null/absence and scalar types. Serving policy selects
//! fields, normalizes addresses/metadata and derives signatures from ABI. No
//! live Ethereum or Sourcify calls occur.
use crate::{
    api::{self, ApiError},
    fields::MINIMAL,
};
use anyhow::{ensure, Result};
use serde_json::{json, Map, Value};
use sha3::{Digest, Keccak256};
use std::collections::BTreeSet;

pub fn validate(record: &Value, chain: &str, address: &str) -> Result<()> {
    for field in MINIMAL {
        ensure!(record.get(*field).is_some(), ApiError::corrupt());
    }
    let stored_chain = record["chainId"]
        .as_u64()
        .or_else(|| record["chainId"].as_str()?.parse::<u64>().ok());
    ensure!(
        stored_chain == chain.parse::<u64>().ok(),
        ApiError::corrupt()
    );
    ensure!(
        record["address"]
            .as_str()
            .is_some_and(|v| v.eq_ignore_ascii_case(address)),
        ApiError::corrupt()
    );
    Ok(())
}

pub(crate) fn project(mut record: Value, selectors: &[String]) -> Result<Value, ApiError> {
    let stored = record.as_object_mut().ok_or_else(ApiError::corrupt)?;
    if selectors
        .iter()
        .any(|s| s == "signatures" || s.starts_with("signatures."))
    {
        let signatures = derive_signatures(stored.get("abi").ok_or_else(ApiError::corrupt)?)?;
        stored.insert("signatures".into(), signatures);
    }
    if selectors.iter().any(|s| s == "metadata") && stored.contains_key("metadata") {
        if let Some(language) = stored
            .get("compilation")
            .and_then(|v| v.get("language"))
            .and_then(Value::as_str)
        {
            if !language.eq_ignore_ascii_case("solidity") {
                stored.insert("metadata".into(), Value::Null);
            }
        }
    }
    if selectors.iter().any(|s| s == "stdJsonOutput") {
        if let Some(output) = stored.get_mut("stdJsonOutput") {
            compact_output_metadata(output)?;
        }
    }
    // Ethers getAddress checksums both the request address and deployer upstream.
    {
        let address = stored
            .get("address")
            .and_then(Value::as_str)
            .ok_or_else(ApiError::corrupt)?;
        let normalized = api::checksum_address(address).ok_or_else(ApiError::corrupt)?;
        stored.insert("address".into(), Value::String(normalized));
    }
    stored.insert("chainId".into(), Value::String("1".into()));
    if let Some(deployment) = stored.get_mut("deployment").and_then(Value::as_object_mut) {
        if let Some(Value::String(address)) = deployment.get_mut("deployer") {
            *address = api::checksum_address(address).ok_or_else(ApiError::corrupt)?;
        }
    }
    let mut projected = Map::new();
    for selector in selectors {
        if let Some((top, sub)) = selector.split_once('.') {
            // Selecting an absent value must not fabricate a null. A stored null
            // parent stays null, rather than turning into an empty object.
            if let Some(parent) = stored.get(top) {
                if parent.is_null() {
                    projected.insert(top.into(), Value::Null);
                } else if let Some(value) = parent.get(sub) {
                    projected
                        .entry(top.to_owned())
                        .or_insert_with(|| Value::Object(Map::new()))
                        .as_object_mut()
                        .ok_or_else(ApiError::corrupt)?
                        .insert(sub.into(), value.clone());
                }
            }
        } else if let Some(value) = stored.get(selector) {
            projected.insert(selector.clone(), value.clone());
        }
    }
    for field in MINIMAL {
        if let Some(value) = stored.get(*field) {
            projected.insert((*field).into(), value.clone());
        }
    }
    Ok(Value::Object(projected))
}

/// Parquet metadata text can contain formatting whitespace absent from the
/// upstream API's embedded metadata string. This is an HTTP serving policy;
/// hydration continues to reconstruct the original stored text exactly.
fn compact_output_metadata(output: &mut Value) -> Result<(), ApiError> {
    let Some(files) = output.get_mut("contracts").and_then(Value::as_object_mut) else {
        return Ok(());
    };
    for contracts in files.values_mut().filter_map(Value::as_object_mut) {
        for contract in contracts.values_mut() {
            let Some(Value::String(text)) = contract.get_mut("metadata") else {
                continue;
            };
            // Validate before editing. Do not parse/reserialize: that can change
            // number spellings, escape sequences, key ordering or duplicate keys.
            serde_json::from_str::<serde::de::IgnoredAny>(text).map_err(|_| ApiError::corrupt())?;
            let mut quoted = false;
            let mut escaped = false;
            text.retain(|ch| {
                if quoted {
                    if escaped {
                        escaped = false;
                    } else if ch == '\\' {
                        escaped = true;
                    } else if ch == '"' {
                        quoted = false;
                    }
                    true
                } else if ch == '"' {
                    quoted = true;
                    true
                } else {
                    !matches!(ch, ' ' | '\t' | '\r' | '\n')
                }
            });
        }
    }
    Ok(())
}

/// Equivalent to Sourcify's extractSignaturesFromAbi for compiler JSON ABI:
/// canonical tuple/array types, Keccak-256 hashes, and invalid-fragment skipping.
/// Its database key deduplicates by kind/hash; its response sorts by signature.
fn derive_signatures(abi: &Value) -> Result<Value, ApiError> {
    let mut groups: [BTreeSet<String>; 3] = Default::default();
    let items = match abi {
        Value::Null => &[][..], // Yul has no ABI and no signatures.
        Value::Array(items) => items.as_slice(),
        _ => return Err(ApiError::corrupt()),
    };
    for item in items {
        let group = match item.get("type").and_then(Value::as_str) {
            Some("function") => 0,
            Some("event") => 1,
            Some("error") => 2,
            _ => continue,
        };
        let Some(name) = item
            .get("name")
            .and_then(Value::as_str)
            .filter(|s| identifier(s))
        else {
            continue;
        };
        let Some(inputs) = parameters(item.get("inputs"), group == 1, 0) else {
            continue;
        };
        // Ethers validates outputs while parsing a function, even though they
        // are absent from its signature. This skips old custom library types.
        if group == 0 && parameters(item.get("outputs"), false, 0).is_none() {
            continue;
        }
        groups[group].insert(format!("{name}({})", inputs.join(",")));
    }
    let mut result = Map::new();
    for (kind, signatures) in ["function", "event", "error"].into_iter().zip(groups) {
        let signatures = signatures.into_iter().map(|signature| {
            let hash = format!("0x{}", hex::encode(Keccak256::digest(signature.as_bytes())));
            json!({"signature": signature, "signatureHash4": &hash[..10], "signatureHash32": hash})
        }).collect();
        result.insert(kind.into(), Value::Array(signatures));
    }
    Ok(Value::Object(result))
}

fn parameters(value: Option<&Value>, indexed: bool, depth: usize) -> Option<Vec<String>> {
    match value {
        None | Some(Value::Null) => Some(Vec::new()),
        Some(Value::Array(values)) => values
            .iter()
            .map(|v| parameter(v, indexed, depth))
            .collect(),
        _ => None,
    }
}

fn parameter(value: &Value, indexed: bool, depth: usize) -> Option<String> {
    if depth > 128 {
        return None;
    }
    if let Some(name) = value.get("name").filter(|v| !v.is_null()) {
        let name = name.as_str()?;
        if !name.is_empty() && !identifier(name) {
            return None;
        }
    }
    if !indexed && value.get("indexed").is_some_and(|v| !v.is_null()) {
        return None;
    }
    let typ = value.get("type")?.as_str()?;
    let base_end = typ.find('[').unwrap_or(typ.len());
    let base = &typ[..base_end];
    let mut canonical = if base == "tuple" || base.starts_with("tuple(") || base.starts_with('(') {
        let components = value.get("components")?.as_array()?;
        let types: Option<Vec<_>> = components
            .iter()
            .map(|c| parameter(c, false, depth + 1))
            .collect();
        format!("({})", types?.join(","))
    } else {
        match base {
            "address" | "bool" | "string" | "bytes" => base.to_owned(),
            "uint" => "uint256".into(),
            "int" => "int256".into(),
            _ => {
                if let Some(bits) = base
                    .strip_prefix("uint")
                    .or_else(|| base.strip_prefix("int"))
                {
                    let n: u16 = bits.parse().ok()?;
                    if n == 0 || n > 256 || !n.is_multiple_of(8) || n.to_string() != bits {
                        return None;
                    }
                } else if let Some(bytes) = base.strip_prefix("bytes") {
                    let n: u8 = bytes.parse().ok()?;
                    if n == 0 || n > 32 || n.to_string() != bytes {
                        return None;
                    }
                } else {
                    return None;
                }
                base.to_owned()
            }
        }
    };
    let mut rest = &typ[base_end..];
    while !rest.is_empty() {
        rest = rest.strip_prefix('[')?;
        let end = rest.find(']')?;
        let size = &rest[..end];
        canonical.push('[');
        if !size.is_empty() {
            if !size.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let size: u64 = size.parse().ok()?;
            // Ethers represents array lengths as JavaScript safe integers.
            if size > 9_007_199_254_740_991 {
                return None;
            }
            canonical.push_str(&size.to_string());
        }
        canonical.push(']');
        rest = &rest[end + 1..];
    }
    Some(canonical)
}

fn identifier(name: &str) -> bool {
    let mut bytes = name.bytes();
    bytes
        .next()
        .is_some_and(|b| b.is_ascii_alphabetic() || b == b'_' || b == b'$')
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'$')
}
