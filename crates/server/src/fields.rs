//! Version 1 fixture serving contract. This is a subset, not Sourcify API parity.
use crate::api::ApiError;

pub const MINIMAL: &[&str] = &[
    "address",
    "chainId",
    "match",
    "matchId",
    "creationMatch",
    "runtimeMatch",
    "verifiedAt",
];
pub const OPTIONAL: &[&str] = &[
    "abi",
    "compilation",
    "deployment",
    "creationBytecode",
    "runtimeBytecode",
    "sources",
    "metadata",
    "storageLayout",
    "transientStorageLayout",
    "userdoc",
    "devdoc",
    "sourceIds",
    "stdJsonInput",
    "stdJsonOutput",
    "additionalInput",
    "signatures",
];

/// Reject ambiguous/unsupported query semantics before accessing any backend.
/// Percent escapes are decoded strictly; only one `fields` parameter is supported.
pub fn parse(query: Option<&str>) -> Result<Vec<&'static str>, ApiError> {
    let mut fields = MINIMAL.to_vec();
    let Some(query) = query.filter(|s| !s.is_empty()) else {
        return Ok(fields);
    };
    let params: Vec<_> = query.split('&').collect();
    if params.len() != 1 {
        return Err(ApiError::invalid("Only one fields parameter is supported"));
    }
    let (key, value) = params[0]
        .split_once('=')
        .ok_or_else(|| ApiError::invalid("Expected fields=value"))?;
    if decode(key)? != "fields" {
        return Err(ApiError::invalid(
            "Only the fields query parameter is supported; omit is unsupported",
        ));
    }
    let value = decode(value)?;
    if value == "all" {
        fields.extend_from_slice(OPTIONAL);
        return Ok(fields);
    }
    for field in value.split(',') {
        if field == "proxyResolution" || field.starts_with("proxyResolution.") {
            return Err(ApiError::invalid(
                "proxyResolution is deferred and unsupported",
            ));
        }
        let field = MINIMAL
            .iter()
            .chain(OPTIONAL)
            .find(|&&known| known == field)
            .ok_or_else(|| ApiError::invalid("Unknown, empty, or nested field selector"))?;
        if !fields.contains(field) {
            fields.push(field);
        }
    }
    Ok(fields)
}

fn decode(value: &str) -> Result<String, ApiError> {
    let mut bytes = value.bytes();
    let mut decoded = Vec::with_capacity(value.len());
    while let Some(byte) = bytes.next() {
        match byte {
            b'%' => {
                let hex = |v: Option<u8>| v.and_then(|v| (v as char).to_digit(16));
                let hi =
                    hex(bytes.next()).ok_or_else(|| ApiError::invalid("Invalid query escape"))?;
                let lo =
                    hex(bytes.next()).ok_or_else(|| ApiError::invalid("Invalid query escape"))?;
                decoded.push((hi * 16 + lo) as u8);
            }
            b'+' => decoded.push(b' '),
            _ => decoded.push(byte),
        }
    }
    String::from_utf8(decoded).map_err(|_| ApiError::invalid("Query must be UTF-8"))
}
