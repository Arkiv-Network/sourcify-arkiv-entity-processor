use arkiv_sourcify_server::fields;
use serde_json::{json, Value};

fn record(address: &str, output: Value) -> Value {
    json!({
        "chainId":"1", "address":address, "match":"exact_match", "creationMatch":"exact_match",
        "runtimeMatch":"exact_match", "verifiedAt":"2020-01-01T00:00:00Z", "matchId":"1",
        "stdJsonOutput":output
    })
}

#[test]
fn exact_metadata_strings_match_five_independent_live_responses() {
    let cases: Value =
        serde_json::from_str(include_str!("fixtures/metadata-unicode/captured.json")).unwrap();
    for case in cases.as_array().unwrap() {
        let file = case["file"].as_str().unwrap();
        let contract = case["contract"].as_str().unwrap();
        let input = record(
            case["address"].as_str().unwrap(),
            json!({"contracts":{file:{contract:{"metadata":case["stored"]}}}}),
        );
        let actual = fields::parse(Some("fields=stdJsonOutput"))
            .unwrap()
            .project(input)
            .unwrap();
        assert_eq!(
            actual["stdJsonOutput"]["contracts"][file][contract]["metadata"], case["expected"],
            "match {}",
            case["matchId"]
        );
    }
}

#[test]
fn unicode_decoding_keeps_lexical_forms_literals_and_surrogates_safe() {
    let input = r#" { "n": 9007199254740993, "decimal": 1.00, "huge": 1e+999, "n": -0, "ascii": "\u0061", "controls": "\u0009\n", "literal": "\\u2019", "slash": "\/", "quote": "\"", "\u0628": "\u0627\u2019\ud83d\ude00", "secondPair": "\ud800\udfff" } "#;
    let expected = r#"{"n":9007199254740993,"decimal":1.00,"huge":1e+999,"n":-0,"ascii":"\u0061","controls":"\u0009\n","literal":"\\u2019","slash":"\/","quote":"\"","ب":"ا’😀","secondPair":"𐏿"}"#;
    let actual = fields::parse(Some("fields=stdJsonOutput"))
        .unwrap()
        .project(record(
            "0x1111111111111111111111111111111111111111",
            json!({"contracts":{"A.sol":{"A":{"metadata":input,"other":input}}}}),
        ))
        .unwrap();
    assert_eq!(
        actual["stdJsonOutput"]["contracts"]["A.sol"]["A"]["metadata"],
        expected
    );
    assert_eq!(
        actual["stdJsonOutput"]["contracts"]["A.sol"]["A"]["other"],
        input
    );
}

#[test]
fn isolated_surrogates_are_preserved_without_panicking() {
    let input = r#"{ "high":"\ud800x", "low":"\udfff", "asciiAfter":"\ud800\u0061" }"#;
    let result = fields::parse(Some("fields=stdJsonOutput"))
        .unwrap()
        .project(record(
            "0x1111111111111111111111111111111111111111",
            json!({"contracts":{"A.sol":{"A":{"metadata":input}}}}),
        ));
    // IgnoredAny accepts escaped UTF-16 code units without materializing Rust strings.
    let actual = result.unwrap();
    assert_eq!(
        actual["stdJsonOutput"]["contracts"]["A.sol"]["A"]["metadata"],
        r#"{"high":"\ud800x","low":"\udfff","asciiAfter":"\ud800\u0061"}"#
    );
}
