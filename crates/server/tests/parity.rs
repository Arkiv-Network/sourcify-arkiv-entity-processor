//! Offline regression of bounded, pinned production captures. No test performs
//! a live Sourcify request. Snapshot input and reference provenance are separate.
use arkiv_sourcify_server::{api, fields};
use axum::{body::to_bytes, response::IntoResponse};
use serde_json::{json, Value};

fn fixture(name: &str) -> Value {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/sourcify-v2")
        .join(format!("{name}.json"));
    serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap()
}
fn query(case: &Value) -> Option<&str> {
    case["path"]
        .as_str()
        .unwrap()
        .split_once('?')
        .map(|(_, q)| q)
}
fn stored() -> Value {
    fixture("stored-16511691")
}

#[test]
fn captured_minimal_nested_and_omit_responses_match() {
    for name in [
        "minimal",
        "nested",
        "omit_top",
        "omit_nested",
        "omit_required",
        "signatures",
    ] {
        let case = fixture(name);
        let selection = fields::parse(query(&case)).unwrap();
        let actual = selection.project(stored()).unwrap();
        assert_eq!(actual, case["body"], "capture: {name}");
    }
    // This is an intentional production quirk, not merely schema behavior.
    assert_eq!(fixture("omit_top")["body"], fixture("omit_nested")["body"]);
}

#[tokio::test]
async fn captured_query_errors_have_sourcify_shape_messages_and_fresh_ids() {
    for name in [
        "unknown_field",
        "implicit_field",
        "deep_field",
        "both",
        "all_mixed",
        "repeated_fields",
        "unknown_query",
        "empty_fields",
        "empty_omit",
    ] {
        let case = fixture(name);
        let error = fields::parse(query(&case)).unwrap_err();
        let response = error.into_response();
        assert_eq!(
            response.status().as_u16(),
            case["status"].as_u64().unwrap() as u16,
            "{name}"
        );
        assert_eq!(
            response.headers()["content-type"],
            "application/json; charset=utf-8"
        );
        let body: Value =
            serde_json::from_slice(&to_bytes(response.into_body(), 8192).await.unwrap()).unwrap();
        assert_eq!(body["customCode"], case["body"]["customCode"], "{name}");
        assert_eq!(body["message"], case["body"]["message"], "{name}");
        assert_eq!(body.as_object().unwrap().len(), 3);
        let id = uuid::Uuid::parse_str(body["errorId"].as_str().unwrap()).unwrap();
        assert_eq!(id.get_version_num(), 4);
        assert_ne!(body["errorId"], case["body"]["errorId"]);
    }
}

#[tokio::test]
async fn captured_address_errors_and_missing_response_match() {
    for name in ["bad_checksum", "bad_address", "bad_chain"] {
        let case = fixture(name);
        let parts: Vec<_> = case["path"].as_str().unwrap().split('/').collect();
        let error = api::validate_path(parts[3], parts[4]).unwrap_err();
        assert_eq!(
            error.status.as_u16(),
            case["status"].as_u64().unwrap() as u16
        );
        assert_eq!(error.message, case["body"]["message"].as_str().unwrap());
        assert_eq!(error.code, case["body"]["customCode"].as_str().unwrap());
    }
    let response = api::not_found("0x0000000000000000000000000000000000000000").into_response();
    assert_eq!(response.status(), 404);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 8192).await.unwrap()).unwrap();
    assert_eq!(body, fixture("missing")["body"]);
    assert!(body.get("matchId").is_none());
    assert!(body.get("verifiedAt").is_none());
}

#[test]
fn selectors_have_narrow_dependency_closure_and_explicit_proxy_exclusion() {
    let selection = fields::parse(Some(
        "fields=deployment.transactionHash,compilation.language,abi,abi",
    ))
    .unwrap();
    let deps = selection.dependencies();
    assert_eq!(deps.len(), fields::MINIMAL.len() + 3);
    assert!(deps.contains(&"deployment") && deps.contains(&"compilation") && deps.contains(&"abi"));
    assert!(!deps.contains(&"sources"));
    let sig = fields::parse(Some("fields=signatures.function")).unwrap();
    assert!(sig.dependencies().contains(&"abi"));
    assert!(!sig.dependencies().contains(&"signatures"));
    assert_eq!(
        sig.project(stored()).unwrap()["signatures"]
            .as_object()
            .unwrap()
            .len(),
        1
    );
    for bad in [
        "fields=proxyResolution.implementations",
        "omit=all",
        "fields=abi,",
        "fields=abi.name",
        "fields=compilation.bad",
        "fields=%GG",
        "fields=%FF",
        "fields[]=abi",
        "omit=abi&omit=sources",
        "fields=chainId",
        "fields=match",
    ] {
        assert!(fields::parse(Some(bad)).is_err(), "{bad}");
    }
    let projected = fields::parse(Some("omit=proxyResolution"))
        .unwrap()
        .project(stored())
        .unwrap();
    assert_eq!(projected.as_object().unwrap().len(), 23);
    assert!(projected.get("proxyResolution").is_none());
    assert!(fields::parse(Some("omit=proxyResolution")).is_ok());
}

#[test]
fn null_absence_arrays_and_nested_selection_are_preserved() {
    let mut record = stored();
    record["storageLayout"] = Value::Null;
    record
        .as_object_mut()
        .unwrap()
        .remove("transientStorageLayout");
    record["compilation"]["compilerSettings"] =
        json!({"libraries":{},"optimizer":{"enabled":true}});
    let actual = fields::parse(Some(
        "fields=storageLayout,transientStorageLayout,compilation.compilerSettings,abi",
    ))
    .unwrap()
    .project(record.clone())
    .unwrap();
    assert!(actual["storageLayout"].is_null());
    assert!(actual.get("transientStorageLayout").is_none());
    assert_eq!(
        actual["compilation"],
        json!({"compilerSettings":record["compilation"]["compilerSettings"]})
    );
    assert_eq!(actual["abi"], record["abi"]);
    for selectors in [
        "fields=deployment.transactionHash,deployment",
        "fields=deployment,deployment.transactionHash",
    ] {
        assert_eq!(
            fields::parse(Some(selectors))
                .unwrap()
                .project(record.clone())
                .unwrap()["deployment"],
            record["deployment"]
        );
    }
}

#[test]
fn percent_decoding_and_address_normalization_are_compatible() {
    let checksum = "0xdAE07103f1e0B23eb58d50758b9ADe175d33e71B";
    let lower = checksum.to_ascii_lowercase();
    let upper = format!("0x{}", checksum[2..].to_ascii_uppercase());
    for address in [&lower, &upper, &checksum.to_owned()] {
        assert_eq!(api::validate_path("1", address).unwrap(), lower);
        let mut record = stored();
        record["address"] = address.as_str().into();
        assert_eq!(
            fields::parse(None).unwrap().project(record).unwrap()["address"],
            checksum
        );
    }
    assert_eq!(
        fields::parse(Some("%66ields=abi%2Ccompilation.language"))
            .unwrap()
            .project(stored())
            .unwrap(),
        fields::parse(Some("fields=abi,compilation.language"))
            .unwrap()
            .project(stored())
            .unwrap()
    );
}

#[test]
fn abi_derivation_matches_pinned_ethers_vectors_and_does_not_use_snapshot_signatures() {
    for case in fixture("signature-vectors")["cases"].as_array().unwrap() {
        let mut record = stored();
        record["abi"] = case["abi"].clone();
        record["signatures"] = json!({"function":[{"signature":"incorrect()"}]});
        let actual = fields::parse(Some("fields=signatures"))
            .unwrap()
            .project(record)
            .unwrap();
        assert_eq!(actual["signatures"], case["expected"], "{}", case["name"]);
        assert!(actual.get("abi").is_none());
    }
    let mut record = stored();
    record["abi"] = Value::Null;
    assert_eq!(
        fields::parse(Some("fields=signatures"))
            .unwrap()
            .project(record)
            .unwrap()["signatures"],
        json!({"function":[],"event":[],"error":[]})
    );
}

#[test]
fn non_solidity_metadata_is_null_and_unavailable_abi_is_an_error() {
    let mut record = stored();
    record["compilation"]["language"] = json!("Vyper");
    let selection = fields::parse(Some("fields=metadata")).unwrap();
    assert!(selection.dependencies().contains(&"compilation"));
    assert!(selection.project(record.clone()).unwrap()["metadata"].is_null());
    record.as_object_mut().unwrap().remove("abi");
    assert!(fields::parse(Some("fields=signatures"))
        .unwrap()
        .project(record)
        .is_err());
}

#[test]
fn output_metadata_compaction_preserves_literals_and_only_touches_metadata_leaves() {
    let raw = " \r\n{ \"z\": 9007199254740993, \"a\": 1e+03, \"n\": -0, \"d\": 1.00, \"huge\": 1e+999, \"s\": \" \\t\\n \\\" \\\\ \\u0061 café \" } \t";
    let compact = r#"{"z":9007199254740993,"a":1e+03,"n":-0,"d":1.00,"huge":1e+999,"s":" \t\n \" \\ \u0061 café "}"#;
    let mut record = stored();
    record["stdJsonOutput"] = json!({
        "contracts": {
            "path/~.sol": {
                "A": {"metadata":raw,"other":raw,"userdoc":{"metadata":raw}},
                "B": {"metadata":" { \"x\" : 2 } "},
                "Null": {"metadata":null}, "Absent":{}, "Object":{"metadata":{"raw":raw}}
            }
        },
        "sources":{"metadata":raw}, "metadata":raw
    });
    record["sources"] = json!({"A.sol":{"content":raw}});
    let before = record.clone();
    let output = fields::parse(Some("fields=stdJsonOutput,sources,metadata"))
        .unwrap()
        .project(record)
        .unwrap();
    assert_eq!(
        output["stdJsonOutput"]["contracts"]["path/~.sol"]["A"]["metadata"],
        compact
    );
    assert_eq!(
        output["stdJsonOutput"]["contracts"]["path/~.sol"]["B"]["metadata"],
        r#"{"x":2}"#
    );
    let mut expected = before["stdJsonOutput"].clone();
    expected["contracts"]["path/~.sol"]["A"]["metadata"] = compact.into();
    expected["contracts"]["path/~.sol"]["B"]["metadata"] = r#"{"x":2}"#.into();
    assert_eq!(output["stdJsonOutput"], expected);
    assert_eq!(output["sources"], before["sources"]);
    assert_eq!(output["metadata"], before["metadata"]);
}

#[test]
fn invalid_output_metadata_fails_only_when_selected() {
    let mut record = stored();
    record["stdJsonOutput"] = json!({"contracts":{"A.sol":{"A":{"metadata":"{ invalid json }"}}}});
    assert!(fields::parse(None).unwrap().project(record.clone()).is_ok());
    let selection = fields::parse(Some("fields=stdJsonOutput")).unwrap();
    assert_eq!(selection.dependencies().len(), fields::MINIMAL.len() + 1);
    let error = selection.project(record).unwrap_err();
    assert_eq!(error.status.as_u16(), 502);
    assert_eq!(error.code, "corrupt_data");
}
