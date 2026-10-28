//! Stateless contract transformation for bounded parallel workers.
//! A single ordered writer owns global deduplication and output.
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use sha3::Keccak256;
pub mod arkiv;
pub mod bundle;
pub mod hydrate;
mod split;
pub mod udc;
pub mod udc_bundle;
pub mod udc_page;
pub mod udc_rpc;
mod wire;
pub const BUDGET: usize = 122880;
pub const CHUNK: usize = 100000;
pub fn sha(data: impl AsRef<[u8]>) -> [u8; 32] {
    Sha256::digest(data.as_ref()).into()
}
pub fn hx(data: impl AsRef<[u8]>) -> String {
    format!("0x{}", hex::encode(data))
}
pub fn canonical(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<_> = m.keys().collect();
            keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            Value::Object(
                keys.into_iter()
                    .map(|k| (k.clone(), canonical(&m[k])))
                    .collect(),
            )
        }
        Value::Array(a) => Value::Array(a.iter().map(canonical).collect()),
        _ => v.clone(),
    }
}
/// In-place `canonical` for an owned record: reorders keys, moves values instead of cloning them.
fn canonicalize(v: &mut Value) {
    let utf16 = |a: &String, b: &String| a.encode_utf16().cmp(b.encode_utf16());
    match v {
        Value::Object(m) => {
            if !m.keys().is_sorted_by(|a, b| utf16(a, b).is_lt()) {
                let mut entries: Vec<_> = std::mem::take(m).into_iter().collect();
                entries.sort_by(|(a, _), (b, _)| utf16(a, b));
                *m = entries.into_iter().collect();
            }
            m.values_mut().for_each(canonicalize);
        }
        Value::Array(a) => a.iter_mut().for_each(canonicalize),
        _ => {}
    }
}
/// One NDJSON input line: a contract record or the source completion trailer.
pub enum Line {
    Record(Vec<Entity>),
    /// `skipped` is the trailer's raw `skipped` value when present (validated by the writer).
    Complete {
        records: Option<u64>,
        skipped: Option<Value>,
    },
}
/// Parses and transforms one owned line; same result as `transform` on the parsed value.
pub fn transform_line(line: Vec<u8>) -> Result<Line> {
    let line = String::from_utf8(line).context("input is not valid UTF-8")?;
    let mut v: Value = serde_json::from_str(&line).context("invalid input JSON")?;
    if v["type"] == "source-complete" {
        return Ok(Line::Complete {
            records: v["records"].as_u64(),
            skipped: v.get_mut("skipped").map(Value::take),
        });
    }
    check_envelope(&v)?;
    let wrapped = match v.as_object_mut() {
        Some(m) => m.remove("record").or_else(|| m.remove("contract")),
        None => None,
    };
    let mut record = wrapped.unwrap_or(v);
    canonicalize(&mut record);
    Ok(Line::Record(convert(&record)?))
}
/// An input line is a bare contract record or `{"record": ...}` / `{"contract": ...}` with nothing else. Entities
/// carry Sourcify API data only: a source database's internal row IDs (the former `sourcifyIds` envelope) or any
/// other data beside the record are refused rather than dropped or stored.
fn check_envelope(input: &Value) -> Result<()> {
    let Some(m) = input.as_object() else {
        return Ok(());
    };
    if !(m.contains_key("record") || m.contains_key("contract")) {
        return Ok(());
    }
    ensure!(
        m.len() == 1,
        "an input envelope holds only \"record\" or \"contract\"; found {:?} (source database IDs such as sourcifyIds are not accepted)",
        m.keys().collect::<Vec<_>>()
    );
    Ok(())
}
// JavaScript JSON.stringify enumerates integer-index keys before string keys.
pub fn bytes(v: &Value) -> Vec<u8> {
    serialize(v, false).into_bytes()
}
fn serialize(v: &Value, canonical_order: bool) -> String {
    match v {
        Value::Object(m) => {
            let mut keys: Vec<_> = m.keys().collect();
            if canonical_order {
                keys.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
            } else {
                keys.sort_by_key(|k| {
                    let n = k
                        .parse::<u32>()
                        .ok()
                        .filter(|n| *n < u32::MAX && n.to_string() == ***k);
                    (n.is_none(), n.unwrap_or(0))
                });
            }
            format!(
                "{{{}}}",
                keys.into_iter()
                    .map(|k| format!(
                        "{}:{}",
                        serde_json::to_string(k).unwrap(),
                        serialize(&m[k], canonical_order)
                    ))
                    .collect::<Vec<_>>()
                    .join(",")
            )
        }
        Value::Array(a) => format!(
            "[{}]",
            a.iter()
                .map(|x| serialize(x, canonical_order))
                .collect::<Vec<_>>()
                .join(",")
        ),
        Value::Number(n) => n.to_string(),
        _ => serde_json::to_string(v).unwrap(),
    }
}
fn checksum_address(address: &str) -> Result<String> {
    ensure!(
        address.starts_with("0x") && address.len() == 42 && hex::decode(&address[2..]).is_ok(),
        "invalid address"
    );
    let lower = address[2..].to_ascii_lowercase();
    let hash = hex::encode(Keccak256::digest(lower.as_bytes()));
    Ok(format!(
        "0x{}",
        lower
            .chars()
            .zip(hash.chars())
            .map(|(c, h)| if h.to_digit(16).unwrap() >= 8 {
                c.to_ascii_uppercase()
            } else {
                c
            })
            .collect::<String>()
    ))
}
fn attr(t: &str, v: impl Into<Value>) -> Value {
    json!({"type":t,"value":v.into()})
}
fn trunc(s: &str) -> String {
    if s.len() <= 128 {
        s.into()
    } else {
        let mut n = 125;
        while !s.is_char_boundary(n) {
            n -= 1;
        }
        format!("{}…", &s[..n])
    }
}
fn string<'a>(v: &'a Value, default: &'a str) -> &'a str {
    v.as_str().unwrap_or(default)
}
fn object(v: &Value) -> Value {
    if v.is_object() {
        v.clone()
    } else {
        json!({})
    }
}
fn truth(v: &Value) -> bool {
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().unwrap_or(0.) != 0.,
        Value::String(s) => !s.is_empty(),
        _ => true,
    }
}
fn decimal(v: &Value) -> Result<String> {
    match v {
        Value::String(s) => {
            s.parse::<u64>()?;
            Ok(s.clone())
        }
        Value::Number(n) => Ok(n.as_u64().context("expected unsigned integer")?.to_string()),
        _ => bail!("expected unsigned integer"),
    }
}
#[derive(Debug, Clone)]
pub struct Entity {
    pub kind: String,
    pub payload: Vec<u8>,
    pub content_type: String,
    pub attributes: Map<String, Value>,
    pub id: [u8; 32],
}
impl Entity {
    pub fn new(
        kind: &str,
        payload: Vec<u8>,
        ctype: &str,
        mut attributes: Map<String, Value>,
    ) -> Self {
        attributes.insert("projectId".into(), attr("str", "sourcify"));
        attributes.insert("kind".into(), attr("str", kind));
        let mut e = Self {
            kind: kind.into(),
            payload,
            content_type: ctype.into(),
            attributes,
            id: [0; 32],
        };
        e.identify();
        e
    }
    pub fn content(&self) -> Value {
        json!({"schema":"sourcify-arkiv.staging-entity.v1","contentType":self.content_type,"payloadEncoding":"hex","payload":hx(&self.payload),"attributes":self.attributes,"keyReferences":"staging-entity-id"})
    }
    fn identify(&mut self) {
        self.id = sha(serialize(
            &json!({"kind":self.kind,"content":self.content()}),
            true,
        ));
    }
}
fn attrs(v: Value) -> Map<String, Value> {
    v.as_object().unwrap().clone()
}
fn fit(
    kind: &str,
    payload: Vec<u8>,
    ctype: &str,
    a: Map<String, Value>,
    out: &mut Vec<Entity>,
) -> Entity {
    if payload.len() <= BUDGET {
        return Entity::new(kind, payload, ctype, a);
    }
    let digest = hx(sha(&payload));
    let parts = payload.len().div_ceil(CHUNK);
    for (part, slice) in payload.chunks(CHUNK).enumerate() {
        out.push(Entity::new("blob",slice.to_vec(),"application/octet-stream",attrs(json!({"hash":attr("bytes32",digest.clone()),"part":attr("i32",part),"parts":attr("i32",parts),"size":attr("i32",slice.len())}))));
    }
    Entity::new(
        kind,
        bytes(&json!({"$spill":{"hash":digest,"parts":parts,"bytes":payload.len()}})),
        "application/json",
        a,
    )
}
fn compiler_attrs(r: &Value, runs: bool) -> Map<String, Value> {
    let c = &r["compilation"];
    let s = &c["compilerSettings"];
    let mut a = Map::new();
    for (k, v) in [
        ("compiler", string(&c["compiler"], "unknown")),
        ("compilerversion", string(&c["compilerVersion"], "unknown")),
        ("language", string(&c["language"], "unknown")),
        ("name", string(&c["name"], "")),
        ("evmversion", string(&s["evmVersion"], "default")),
    ] {
        a.insert(k.into(), attr("str", trunc(v)));
    }
    a.insert(
        "optimizer".into(),
        attr("bool", truth(&s["optimizer"]["enabled"])),
    );
    if runs {
        let n = s["optimizer"]["runs"]
            .as_f64()
            .or_else(|| s["optimizer"]["runs"].as_str().and_then(|v| v.parse().ok()))
            .unwrap_or(0.);
        a.insert(
            "optimizerruns".into(),
            attr(
                "i32",
                n.trunc().clamp(i32::MIN as f64, i32::MAX as f64) as i32,
            ),
        );
    }
    a.insert("viair".into(), attr("bool", truth(&s["viaIR"])));
    a
}
pub fn transform(input: &Value) -> Result<Vec<Entity>> {
    check_envelope(input)?;
    match input.get("record").or_else(|| input.get("contract")) {
        Some(record) => convert(&canonical(record)),
        None => convert(&canonical(input)),
    }
}
/// Transforms a canonical (key-sorted) record.
fn convert(r: &Value) -> Result<Vec<Entity>> {
    ensure!(
        r["chainId"] == "1" || r["chainId"] == 1,
        "Ethereum mainnet only"
    );
    ensure!(
        r.get("abi")
            .is_some_and(|abi| abi.is_array() || abi.is_null()),
        "ABI must be an array or null"
    );
    for name in ["compilation", "creationBytecode", "runtimeBytecode"] {
        ensure!(r[name].is_object(), "missing object {name}");
    }
    ensure!(
        r.get("sources")
            .is_some_and(|sources| sources.is_object() || sources.is_null()),
        "sources must be an object or null"
    );
    let address = r["address"].as_str().context("address missing")?;
    ensure!(
        address.starts_with("0x") && address.len() == 42 && hex::decode(&address[2..]).is_ok(),
        "invalid address"
    );
    let (mut out, contract_payload, compilation_id, refs) = split::split(r)?;
    let proxy = object(&r["proxyResolution"]);
    let deployment = object(&r["deployment"]);
    let abi = &r["abi"];
    let mut va = compiler_attrs(r, false);
    va.insert("chainid".into(), attr("u64", "1"));
    va.insert("address".into(), attr("addr", checksum_address(address)?));
    va.insert("match".into(), attr("str", string(&r["match"], "null")));
    let ts = chrono::DateTime::parse_from_rfc3339(
        r["verifiedAt"].as_str().context("missing verifiedAt")?,
    )?
    .timestamp();
    ensure!(ts >= 0, "negative timestamp");
    va.insert("verifiedat".into(), attr("u64", ts.to_string()));
    va.insert("matchid".into(), attr("u64", decimal(&r["matchId"])?));
    if proxy["isProxy"].is_boolean() {
        va.insert("isproxy".into(), attr("bool", truth(&proxy["isProxy"])));
        va.insert(
            "proxytype".into(),
            attr("str", trunc(string(&proxy["proxyType"], "none"))),
        );
    }
    va.insert(
        "abihash".into(),
        attr("bytes32", hx(sha(split::json_bytes(abi)))),
    );
    va.insert(
        "blocknumber".into(),
        attr(
            "u64",
            if deployment["blockNumber"].is_null() {
                "0".into()
            } else {
                decimal(&deployment["blockNumber"])?
            },
        ),
    );
    va.insert(
        "deployer".into(),
        attr(
            "addr",
            checksum_address(string(
                &deployment["deployer"],
                "0x0000000000000000000000000000000000000000",
            ))?,
        ),
    );
    va.insert("compilationref".into(), attr("key", compilation_id));
    for (name, rname) in [
        ("creationcodehash", "creationOnchain"),
        ("runtimecodehash", "runtimeOnchain"),
    ] {
        if !refs[rname].is_null() {
            va.insert(name.into(), attr("bytes32", refs[rname].clone()));
        }
    }
    let e = fit(
        "verified_contract",
        contract_payload,
        "application/json",
        va,
        &mut out,
    );
    out.push(e);
    Ok(out)
}
pub fn entity_key(id: &[u8; 32]) -> String {
    hx(sha(format!(
        "arkiv-prefill.imported-staging.v1:{}",
        hex::encode(id)
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> Value {
        json!({"chainId":"1","address":"0x1111111111111111111111111111111111111111","matchId":"1","verifiedAt":"2020-01-01T00:00:00Z","abi":[],"compilation":{"language":"Solidity","compiler":"solc","compilerVersion":"1","compilerSettings":{},"fullyQualifiedName":"A.sol:A"},"sources":{"A.sol":{"content":"contract A {}"}},"creationBytecode":{"onchainBytecode":null,"recompiledBytecode":null},"runtimeBytecode":{"onchainBytecode":"0x6000","recompiledBytecode":"0x6000"}})
    }
    #[test]
    fn deterministic_and_local_dedup() {
        let a = transform(&fixture()).unwrap();
        let b = transform(&fixture()).unwrap();
        assert_eq!(a.len(), 4);
        assert_eq!(
            a.iter().map(|e| e.id).collect::<Vec<_>>(),
            b.iter().map(|e| e.id).collect::<Vec<_>>()
        );
        assert!(!a.last().unwrap().attributes.contains_key("isproxy"));
    }
    #[test]
    fn metadata_is_identity() {
        let a = Entity::new("code", vec![1], "application/octet-stream", Map::new());
        let b = Entity::new("blob", vec![1], "application/octet-stream", Map::new());
        assert_ne!(a.id, b.id);
        assert_eq!(sha(&a.payload), sha(&b.payload));
    }
    #[test]
    fn spilled_utf8_roundtrip() {
        let mut r = fixture();
        r["sources"]["A.sol"]["content"] = json!("🌍".repeat(50000));
        let es = transform(&r).unwrap();
        let source = es.iter().find(|e| e.kind == "sourcefile").unwrap();
        let stub: Value = serde_json::from_slice(&source.payload).unwrap();
        let blobs: Vec<_> = es
            .iter()
            .filter(|e| e.kind == "blob" && e.attributes["hash"]["value"] == stub["$spill"]["hash"])
            .collect();
        let payload: Vec<u8> = blobs.iter().flat_map(|e| e.payload.clone()).collect();
        assert_eq!(hx(sha(&payload)), stub["$spill"]["hash"]);
        let restored: Value = serde_json::from_slice(&payload).unwrap();
        assert_eq!(restored["content"], r["sources"]["A.sol"]["content"]);
    }
    #[test]
    fn validation() {
        let mut r = fixture();
        r["chainId"] = json!(2);
        assert!(transform(&r).is_err());
        r["chainId"] = json!(1);
        r["abi"] = json!({});
        assert!(transform(&r).is_err());
        r.as_object_mut().unwrap().remove("abi");
        assert!(transform(&r).is_err());
    }
    #[test]
    fn nullable_abi_and_missing_metadata_are_preserved() {
        let mut r = fixture();
        r["abi"] = Value::Null;
        r["compilation"]["language"] = json!("Yul");
        let es = transform(&r).unwrap();
        let verified = es.iter().find(|e| e.kind == "verified_contract").unwrap();
        let payload: Value = serde_json::from_slice(&verified.payload).unwrap();
        assert!(payload["abi"].is_null());
        assert_eq!(verified.attributes["abihash"]["value"], hx(sha(b"null")));
        let compilation = es.iter().find(|e| e.kind == "compilation").unwrap();
        let payload: Value = serde_json::from_slice(&compilation.payload).unwrap();
        assert!(payload["metadata"].is_null());
        r["abi"] = json!([]);
        let with_array = transform(&r).unwrap();
        assert_ne!(verified.id, with_array.last().unwrap().id);
    }
    #[test]
    fn nullable_sources_preserved_without_source_entities() {
        let mut record = fixture();
        record["sources"] = Value::Null;
        let entities = transform(&record).unwrap();
        assert_eq!(restored_contract(&entities), canonical(&record));
        assert!(!entities.iter().any(|entity| entity.kind == "sourcefile"));
        record.as_object_mut().unwrap().remove("sources");
        assert!(transform(&record).is_err());
        record["sources"] = json!([]);
        assert!(transform(&record).is_err());
    }
    fn restored_contract(entities: &[Entity]) -> Value {
        hydrate::hydrate(entities.last().unwrap(), entities).unwrap()
    }

    #[test]
    fn envelope_with_anything_beside_the_record_is_rejected() {
        for wrapper in ["record", "contract"] {
            for extra in [
                json!({"sourcifyIds": {"compilationId":"compilation-uuid", "deploymentId":"deployment-uuid",
                    "verificationId":"9007199254740993", "contractId":"contract-uuid"}}),
                json!({"sourcifyIds": {}}),
                json!({"sourcifyIds": null}),
                json!({"verificationId": "1"}),
                json!({"note": "x"}),
            ] {
                let mut input = extra.clone();
                input[wrapper] = fixture();
                assert!(transform(&input).is_err(), "accepted {extra}");
                assert!(
                    transform_line(serde_json::to_vec(&input).unwrap()).is_err(),
                    "accepted {extra}"
                );
            }
        }
    }

    #[test]
    fn wrappers_preserve_identity_and_add_no_source_ids() {
        let record = fixture();
        let baseline = transform(&record).unwrap();
        for input in [json!({"record":record}), json!({"contract":record})] {
            for entities in [
                transform(&input).unwrap(),
                match transform_line(serde_json::to_vec(&input).unwrap()).unwrap() {
                    Line::Record(entities) => entities,
                    _ => panic!("record expected"),
                },
            ] {
                assert_eq!(restored_contract(&entities), canonical(&record));
                assert_eq!(entities.len(), baseline.len());
                for (before, after) in baseline.iter().zip(&entities) {
                    assert_eq!(before.id, after.id);
                    assert_eq!(before.attributes, after.attributes);
                    assert!(!after
                        .attributes
                        .keys()
                        .any(|key| key.starts_with("sourcify")));
                }
            }
        }
    }

    #[test]
    fn complete_contract_payload_preserves_source_record() {
        let mut r = fixture();
        // Numeric chain ID, nulls, unknown fields, arrays and large integers must
        // survive without adopting a new schema or normalizing their values.
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
        let entities = transform(&r).unwrap();
        let restored = restored_contract(&entities);
        assert_eq!(restored, canonical(&r));
        for absent in [
            "schema",
            "codeRefs",
            "creationTransformations",
            "sourcifyIds",
        ] {
            assert!(!restored.as_object().unwrap().contains_key(absent));
        }
        let contract = entities.last().unwrap();
        assert_eq!(contract.attributes["chainid"]["value"], "1");
        assert_eq!(contract.attributes["matchid"]["value"], "9007199254740993");
        assert!(contract.attributes.contains_key("compilationref"));
        // The owned streaming parser must have the same lossless behavior.
        let Line::Record(streamed) = transform_line(serde_json::to_vec(&r).unwrap()).unwrap()
        else {
            panic!("record expected");
        };
        assert_eq!(restored_contract(&streamed), restored);
    }

    #[test]
    fn complete_contract_payload_survives_spill_and_key_order() {
        let mut r = fixture();
        r["futureField"] = json!({"content":"🌍".repeat(BUDGET), "empty":{}, "null":null});
        let entities = transform(&r).unwrap();
        let contract = entities.last().unwrap();
        let stored: Value = serde_json::from_slice(&contract.payload).unwrap();
        assert!(stored.get("$spill").is_some());
        assert_eq!(restored_contract(&entities), canonical(&r));
        let mut reordered = r.as_object().unwrap().iter().collect::<Vec<_>>();
        reordered.reverse();
        let reordered = Value::Object(
            reordered
                .into_iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        );
        let repeated = transform(&reordered).unwrap();
        assert_eq!(contract.id, repeated.last().unwrap().id);
        assert_eq!(restored_contract(&repeated), canonical(&r));
    }

    #[test]
    fn sdk_address_checksum() {
        assert_eq!(
            checksum_address("0xa0b86991c6218b36c1d19d4a2e9eb0ce3606eb48").unwrap(),
            "0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48"
        );
    }
    #[test]
    fn line_path_matches_value_path() {
        // U+1F600 sorts before U+FFFF in UTF-16 (surrogates) but after it by code point.
        let tricky = json!({"\u{ffff}":1,"😀":[{"b":1,"a":2}],"10":"x","2":{"z":[],"y":null},"B":0.5,"a":true});
        let mut r = fixture();
        r["compilation"]["compilerSettings"] = tricky.clone();
        r["metadata"] = json!([tricky.clone(), {"k": tricky}]);
        let mut sorted = r.clone();
        canonicalize(&mut sorted);
        assert_eq!(sorted.to_string(), canonical(&r).to_string());
        for input in [r.clone(), json!({"record": r}), json!({"contract": r})] {
            let line = transform_line(input.to_string().into_bytes()).unwrap();
            let Line::Record(line) = line else {
                panic!("record expected");
            };
            let value = transform(&input).unwrap();
            assert_eq!(line.len(), value.len());
            for (a, b) in line.iter().zip(&value) {
                assert_eq!((a.id, &a.payload), (b.id, &b.payload));
            }
        }
        let trailer = br#"{"type":"source-complete","records":3}"#.to_vec();
        assert!(matches!(
            transform_line(trailer),
            Ok(Line::Complete {
                records: Some(3),
                skipped: None
            })
        ));
        assert!(transform_line(b"{\"chainId\":\"1\"".to_vec()).is_err());
        assert!(transform_line(b"{\"chainId\":\"\xff\"}".to_vec()).is_err());
    }
    #[test]
    fn shared_compilation_excludes_deployment_and_hashes_complete_payload() {
        let mut a = fixture();
        a["metadata"] = json!("  { \"a\": 1 }\n");
        a["compilation"]["future"] = json!("0xaabb");
        a["runtimeBytecode"]["onchainBytecode"] = json!("0xaabb");
        let first = transform(&a).unwrap();
        let compilation = |es: &[Entity]| es.iter().find(|e| e.kind == "compilation").unwrap().id;
        let mut b = a.clone();
        b["runtimeBytecode"]["onchainBytecode"] = json!("0xccdd");
        b["address"] = json!("0x2222222222222222222222222222222222222222");
        b["matchId"] = json!("2");
        let second = transform(&b).unwrap();
        assert_eq!(compilation(&first), compilation(&second));
        assert_eq!(restored_contract(&second), canonical(&b));
        b["metadata"] = json!("{\"a\":1}");
        assert_ne!(compilation(&first), compilation(&transform(&b).unwrap()));
        b = a.clone();
        b["compilation"]["large"] = json!(9007199254740993u64);
        let third = transform(&b).unwrap();
        b["compilation"]["large"] = json!(9007199254740992u64);
        assert_ne!(compilation(&third), compilation(&transform(&b).unwrap()));
    }
    #[test]
    fn references_preserve_literals_hex_null_empty_and_standard_json() {
        let mut r = fixture();
        r["sources"]["~odd/path.sol"] = json!({"content":"contract A {}","$ref":{"literal":true}});
        r["runtimeBytecode"]["recompiledBytecode"] = json!("0xaBcD");
        r["runtimeBytecode"]["onchainBytecode"] = json!("0xABcd");
        r["creationBytecode"]["onchainBytecode"] = json!("0x");
        r["metadata"] = json!("  { \"exact\": true }\n");
        r["abi"] = json!([{"type":"function","name":"hello_this_function_name_is_deliberately_long_to_exceed_reference_manifest_size","inputs":[],"outputs":[]}]);
        r["stdJsonInput"] = json!({"sources":{"A.sol":{"content":"contract A {}"}}});
        r["stdJsonOutput"] = json!({"contracts":{"A.sol":{"A":{"abi":r["abi"],"evm":{"bytecode":{"object":"aBcD"}}}}}});
        r["literal"] = json!({"$ref":"literal","$spill":{"parts":1},"schema":"sourcify.compilation.v3","references":[]});
        let es = transform(&r).unwrap();
        assert_eq!(restored_contract(&es), canonical(&r));
        let cp: Value =
            serde_json::from_slice(&es.iter().find(|e| e.kind == "compilation").unwrap().payload)
                .unwrap();
        assert!(cp["references"]
            .as_array()
            .unwrap()
            .iter()
            .any(|r| r["kind"] == "local"));
        let stripped: Value = serde_json::from_slice(&es.last().unwrap().payload).unwrap();
        assert!(stripped["data"].get("abi").is_none());
        assert!(stripped["data"]["runtimeBytecode"]
            .get("onchainBytecode")
            .is_none());
    }
    #[test]
    fn hydration_rejects_missing_tampered_and_invalid_references() {
        let es = transform(&fixture()).unwrap();
        let contract = es.last().unwrap();
        assert!(hydrate::hydrate(contract, &es[1..]).is_err());
        let mut tampered = es.clone();
        tampered[0].payload.push(b' ');
        assert!(hydrate::hydrate(contract, &tampered).is_err());
        let mut tampered = es.clone();
        let source = &mut tampered[0];
        source.payload =
            serde_json::to_vec(&json!({"schema":"sourcify.sourcefile.v1","content":"changed"}))
                .unwrap();
        source.identify();
        assert!(hydrate::hydrate(contract, &tampered).is_err());
        for invalid in [
            json!({"path":"/address","kind":"sourcefile","hash":es[0].attributes["hash"]["value"]}),
            json!({"path":"/missing/child","kind":"sourcefile","hash":es[0].attributes["hash"]["value"]}),
            json!({"path":"/stdJsonInput","kind":"local","pointer":"/stdJsonOutput"}),
        ] {
            let mut bad = contract.clone();
            let mut v: Value = serde_json::from_slice(&bad.payload).unwrap();
            v["references"].as_array_mut().unwrap().push(invalid);
            bad.payload = serde_json::to_vec(&v).unwrap();
            bad.identify();
            assert!(hydrate::hydrate(&bad, &es).is_err());
        }
        let mut bad = contract.clone();
        let mut v: Value = serde_json::from_slice(&bad.payload).unwrap();
        let duplicate = v["references"][0].clone();
        v["references"].as_array_mut().unwrap().push(duplicate);
        bad.payload = serde_json::to_vec(&v).unwrap();
        bad.identify();
        assert!(hydrate::hydrate(&bad, &es).is_err());
    }
    #[test]
    fn shared_spilled_code_and_compilation_roundtrip() {
        let mut r = fixture();
        r["runtimeBytecode"]["recompiledBytecode"] = json!(format!("0x{}", "aB".repeat(BUDGET)));
        r["metadata"] = json!("🌍".repeat(BUDGET));
        let es = transform(&r).unwrap();
        assert_eq!(restored_contract(&es), canonical(&r));
        let mut missing = es.clone();
        let i = missing.iter().position(|e| e.kind == "blob").unwrap();
        missing.remove(i);
        assert!(hydrate::hydrate(es.last().unwrap(), &missing).is_err());
    }
    #[test]
    fn arbitrary_precision_numbers_survive_without_rounding_or_panics() {
        let mut r = fixture();
        let numbers:Value=serde_json::from_str("[184467440737095516160001,0.12345678901234567890123456789,1e400,-184467440737095516160001]").unwrap();
        r["compilation"]["future"] = numbers.clone();
        r["abi"] = json!([{"future":numbers.clone()}]);
        r["unknown"] = numbers.clone();
        let input = serde_json::to_vec(&r).unwrap();
        let Line::Record(es) = transform_line(input).unwrap() else {
            panic!("expected record")
        };
        let hydrated = restored_contract(&es);
        assert_eq!(hydrated, canonical(&r));
        assert_eq!(hydrated["unknown"].to_string(), numbers.to_string());
        let before = es.iter().find(|e| e.kind == "compilation").unwrap().id;
        r["compilation"]["future"][0] = serde_json::from_str("184467440737095516160002").unwrap();
        let after = transform(&r).unwrap();
        assert_ne!(
            before,
            after.iter().find(|e| e.kind == "compilation").unwrap().id
        );
    }
    #[test]
    fn js_key_order() {
        let v = json!({"10":"a","2":"b","x":"c"});
        assert_eq!(
            String::from_utf8(bytes(&v)).unwrap(),
            r#"{"2":"b","10":"a","x":"c"}"#
        );
        assert_eq!(serialize(&v, true), r#"{"10":"a","2":"b","x":"c"}"#);
    }
}
