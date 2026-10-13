//! Sourcify v2 selection contract, pinned in docs/sourcify-http-compatibility.md.
use crate::api::ApiError;
use serde_json::Value;

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
    "proxyResolution",
];

// These three implicit response fields are deliberately not selectable upstream:
// address, chainId, match. The other four minimal fields are selectable, but cannot
// be removed with omit. Ordering follows Sourcify's FIELDS_TO_STORED_PROPERTIES.
const SELECTABLE: &[&str] = &[
    "matchId",
    "creationMatch",
    "runtimeMatch",
    "verifiedAt",
    "creationBytecode",
    "runtimeBytecode",
    "deployment",
    "sources",
    "compilation",
    "abi",
    "metadata",
    "storageLayout",
    "transientStorageLayout",
    "userdoc",
    "devdoc",
    "sourceIds",
    "additionalInput",
    "stdJsonInput",
    "stdJsonOutput",
    "signatures",
    "proxyResolution",
];
const SUBFIELDS: &[(&str, &[&str])] = &[
    (
        "creationBytecode",
        &[
            "onchainBytecode",
            "recompiledBytecode",
            "sourceMap",
            "linkReferences",
            "cborAuxdata",
            "transformations",
            "transformationValues",
        ],
    ),
    (
        "runtimeBytecode",
        &[
            "onchainBytecode",
            "recompiledBytecode",
            "sourceMap",
            "linkReferences",
            "cborAuxdata",
            "immutableReferences",
            "transformations",
            "transformationValues",
        ],
    ),
    (
        "deployment",
        &[
            "transactionHash",
            "blockNumber",
            "transactionIndex",
            "deployer",
        ],
    ),
    (
        "compilation",
        &[
            "language",
            "compiler",
            "compilerVersion",
            "compilerSettings",
            "name",
            "fullyQualifiedName",
        ],
    ),
    ("signatures", &["function", "event", "error"]),
    (
        "proxyResolution",
        &["onchainRuntimeBytecode", "onchainCreationBytecode"],
    ),
];

#[derive(Clone, Debug)]
pub struct Selection {
    selectors: Vec<String>,
}
impl Selection {
    /// Hydration works at top-level granularity. Selection never fetches a field
    /// merely because another optional field exists in the stored contract.
    pub fn dependencies(&self) -> Vec<&'static str> {
        let mut fields = MINIMAL.to_vec();
        for selector in &self.selectors {
            let top = selector.split('.').next().expect("validated selector");
            if top == "proxyResolution" {
                for (field, sub) in [
                    ("creationBytecode", "onchainCreationBytecode"),
                    ("runtimeBytecode", "onchainRuntimeBytecode"),
                ] {
                    if (selector == top || selector.ends_with(sub)) && !fields.contains(&field) {
                        fields.push(field);
                    }
                }
                continue;
            }
            // The serving policy derives signatures from ABI, leaving snapshot
            // signature tables untouched. It must never depend on both copies.
            let top = if top == "signatures" { "abi" } else { top };
            let field = SELECTABLE
                .iter()
                .copied()
                .find(|s| *s == top)
                .expect("validated selector");
            if !fields.contains(&field) {
                fields.push(field);
            }
        }
        if fields.contains(&"metadata") && !fields.contains(&"compilation") {
            fields.push("compilation");
        }
        fields
    }

    pub fn project(&self, record: Value) -> Result<Value, ApiError> {
        crate::projection::project(record, &self.selectors)
    }
    pub fn resolves_proxy(&self) -> bool {
        self.selectors.iter().any(|s| s == "proxyResolution")
    }
}

/// Validate query syntax and selectors before accessing the backend.
pub fn parse(query: Option<&str>) -> Result<Selection, ApiError> {
    let mut fields = None;
    let mut omit = None;
    for param in query.unwrap_or("").split('&').filter(|s| !s.is_empty()) {
        let (key, value) = param.split_once('=').unwrap_or((param, ""));
        let key = decode(key)?;
        let value = decode(value)?;
        let slot = match key.as_str() {
            "fields" => &mut fields,
            "omit" => &mut omit,
            key if key.starts_with("fields[") || key.starts_with("omit[") => {
                let name = key.split('[').next().unwrap();
                return Err(ApiError::invalid(format!(
                    "request/query/{name} must be string"
                )));
            }
            key => {
                return Err(ApiError::invalid(format!(
                    "Unknown query parameter '{key}'"
                )))
            }
        };
        if slot.is_some() {
            return Err(ApiError::invalid(format!(
                "request/query/{key} must be string"
            )));
        }
        if value.is_empty() {
            return Err(ApiError::invalid(format!(
                "Empty value found for query parameter '{key}'"
            )));
        }
        *slot = Some(value);
    }
    if fields.is_some() && omit.is_some() {
        return Err(ApiError::invalid("Cannot specify both fields and omit"));
    }
    let selectors = if let Some(fields) = fields {
        let requested: Vec<_> = fields.split(',').collect();
        if requested.contains(&"all") {
            if requested.len() > 1 {
                return Err(ApiError::invalid("Cannot specify 'all' with other fields"));
            }
            SELECTABLE.iter().map(|s| (*s).to_owned()).collect()
        } else {
            let mut selected = Vec::new();
            for field in requested {
                validate_selector(field)?;
                if !selected.iter().any(|s| s == field) {
                    selected.push(field.to_owned());
                }
            }
            selected
        }
    } else if let Some(omit) = omit {
        let omitted: Vec<_> = omit.split(',').collect();
        for field in &omitted {
            validate_selector(field)?;
        }
        // Production Sourcify 4.1.2 validates dotted omissions but only removes
        // exact top-level names. Match this observed quirk; see pinned captures.
        SELECTABLE
            .iter()
            .filter(|field| !omitted.contains(field))
            .map(|s| (*s).to_owned())
            .collect()
    } else {
        Vec::new()
    };
    Ok(Selection { selectors })
}

fn validate_selector(field: &str) -> Result<(), ApiError> {
    let parts: Vec<_> = field.split('.').collect();
    if parts.len() > 2 {
        return Err(ApiError::invalid(format!(
            "Field selector cannot have more than one level subselectors: {field}"
        )));
    }
    let valid = if parts.len() == 1 {
        SELECTABLE.contains(&field)
    } else {
        SUBFIELDS
            .iter()
            .any(|(top, subs)| *top == parts[0] && subs.contains(&parts[1]))
    };
    if !valid {
        return Err(ApiError::invalid(format!(
            "Field selector {field} is not a valid field"
        )));
    }
    Ok(())
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
