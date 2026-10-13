use arkiv_sourcify_server::fields;
use serde_json::{json, Value};

#[test]
fn abi_projection_matches_captured_reference_signatures_without_reordering_arrays() {
    let captures: Value =
        serde_json::from_str(include_str!("fixtures/signature-collation.json")).unwrap();
    let selection = fields::parse(Some("fields=signatures")).unwrap();
    let mut checked = 0;
    for contract in captures["contracts"].as_array().unwrap() {
        let Some(abi) = contract.get("abi") else {
            continue;
        };
        let record = json!({
            "address": contract["address"], "chainId": "1", "match": "exact_match",
            "creationMatch": "exact_match", "runtimeMatch": "exact_match",
            "matchId": "1", "verifiedAt": "2026-10-13T00:00:00Z", "abi": abi
        });
        assert_eq!(
            selection.project(record).unwrap()["signatures"],
            contract["signatures"],
            "{}",
            contract["address"]
        );
        checked += 1;
    }
    assert!(checked >= 20);
}
