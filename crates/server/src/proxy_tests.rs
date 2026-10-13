use super::*;
use serde_json::{json, Value};
use std::{collections::VecDeque, future::Future};

const ADDRESS: &str = "0x0000000000000000000000000000000000001234";
const IMPLEMENTATION: &str = "0x1234567890123456789012345678901234567890";
const BEACON_ADDRESS: &str = "0x0000000000000000000000000000000000009876";

fn fixture(name: &str) -> String {
    let v: Value = serde_json::from_str(include_str!(
        "../tests/fixtures/proxy/upstream-bytecodes.json"
    ))
    .unwrap();
    v[name].as_str().unwrap().into()
}
fn push(slot: &str) -> String {
    format!("7f{}", &slot[2..])
}
fn code(candidate: Candidate) -> String {
    format!(
        "0x{}",
        push(SLOTS.iter().find(|(_, c)| *c == candidate).unwrap().0)
    )
}
#[derive(Debug)]
enum Answer {
    Word(String),
    Revert,
    Failed,
}
struct Step {
    call: bool,
    address: String,
    key: String,
    answer: Answer,
}
#[derive(Default)]
struct Script {
    steps: VecDeque<Step>,
    count: usize,
}
impl Script {
    fn storage(&mut self, address: &str, key: &str, value: &str) {
        self.steps.push_back(Step {
            call: false,
            address: address.into(),
            key: key.into(),
            answer: Answer::Word(value.into()),
        });
    }
    fn call(&mut self, address: &str, key: &str, answer: Answer) {
        self.steps.push_back(Step {
            call: true,
            address: address.into(),
            key: key.into(),
            answer,
        });
    }
    fn next(&mut self, call: bool, address: &str, key: &str) -> Result<String> {
        self.count += 1;
        let step = self.steps.pop_front().expect("unexpected Ethereum request");
        assert_eq!(
            (call, address, key),
            (step.call, step.address.as_str(), step.key.as_str())
        );
        match step.answer {
            Answer::Word(s) => Ok(s),
            Answer::Revert => Err(Reverted.into()),
            Answer::Failed => Err(anyhow::anyhow!("network unavailable")),
        }
    }
    fn done(&self) {
        assert!(
            self.steps.is_empty(),
            "unconsumed Ethereum requests: {}",
            self.steps.len()
        );
    }
}
impl Provider for Script {
    fn get_storage_at<'a>(&'a mut self, address: &'a str, slot: &'a str) -> ReadFuture<'a, String> {
        Box::pin(async move { self.next(false, address, slot) })
    }
    fn call<'a>(&'a mut self, address: &'a str, data: &'a str) -> ReadFuture<'a, String> {
        Box::pin(async move { self.next(true, address, data) })
    }
}
fn array(addresses: &[&str]) -> String {
    format!(
        "0x{:064x}{:064x}{}",
        32,
        addresses.len(),
        addresses
            .iter()
            .map(|a| format!("{:0>64}", &a[2..]))
            .collect::<String>()
    )
}

#[tokio::test]
async fn detector_matches_pinned_whatsabi_vectors() {
    let fixture: Value =
        serde_json::from_str(include_str!("../tests/fixtures/proxy/whatsabi-0.29.0.json")).unwrap();
    let upstream: Value = serde_json::from_str(include_str!(
        "../tests/fixtures/proxy/upstream-bytecodes.json"
    ))
    .unwrap();
    assert_eq!(fixture["version"], "0.29.0");
    for row in fixture["rows"].as_array().unwrap() {
        let code = if let Some(name) = row["upstream"].as_str() {
            upstream[name].as_str().unwrap()
        } else {
            row["code"].as_str().unwrap()
        };
        let p = disasm(code).await.unwrap();
        let proxies: Vec<Value> = p
            .proxies
            .iter()
            .map(|p| match p {
                Candidate::Fixed(address) => json!({"name":p.name(),"address":address}),
                _ => json!({"name":p.name()}),
            })
            .collect();
        assert_eq!(json!(p.factory), row["factory"], "{} factory", row["name"]);
        assert_eq!(json!(proxies), row["proxies"], "{} proxies", row["name"]);
    }
}

#[tokio::test]
async fn clone_prefix_immutable_arguments_and_factory_suppression() {
    for name in ["EIP1167Proxy", "EIP1167ProxyWithImmutableArgs"] {
        let input = fixture(name);
        let mut provider = Script::default();
        let detected = resolve(&mut provider, ADDRESS, &input).await.unwrap();
        assert_eq!(detected.proxy_type, Some("EIP1167Proxy"));
        assert_eq!(detected.implementations[0], format!("0x{}", &input[22..62]));
        assert_eq!(provider.count, 0);
        let creation = format!("0x3d602d80600a3d3981f3{}", &input[2..]);
        assert_eq!(
            resolve(&mut provider, ADDRESS, &creation).await.unwrap(),
            Detection::none()
        );
    }
    let mut provider = Script::default();
    assert_eq!(
        resolve(&mut provider, ADDRESS, &fixture("FactoryDeployingProxies"))
            .await
            .unwrap(),
        Detection::none()
    );
    assert_eq!(provider.count, 0);
}

#[tokio::test]
async fn resolves_each_storage_family_and_creation_only_slots() {
    for (bytecode, slot, name) in [
        (code(Candidate::Safe), "0x0", "GnosisSafeProxy"),
        (code(Candidate::Zeppelin), ZEPPELIN, "ZeppelinOSProxy"),
        (code(Candidate::Proxiable), PROXIABLE, "PROXIABLEProxy"),
        ("0x30545af4".into(), "0x1234", "SequenceWalletProxy"),
        (fixture("EIP1967Proxy"), IMPL, "EIP1967Proxy"),
        (
            fixture("EIP1967ProxyWithSlotOnlyInCreationCode"),
            IMPL,
            "EIP1967Proxy",
        ),
        (fixture("MaticProxy"), MATIC, "MaticProxy"),
        (
            fixture("MaticProxyWithSlotOnlyInCreationCode"),
            MATIC,
            "MaticProxy",
        ),
    ] {
        let mut provider = Script::default();
        provider.storage(ADDRESS, slot, &IMPLEMENTATION[2..]);
        assert_eq!(
            resolve(&mut provider, ADDRESS, &bytecode).await.unwrap(),
            Detection::found(name, vec![IMPLEMENTATION.into()])
        );
        provider.done();
    }
}

#[tokio::test]
async fn beacon_fallback_selector_order_zero_revert_and_network_failure() {
    let mut provider = Script::default();
    provider.storage(ADDRESS, IMPL, "0x0");
    provider.storage(ADDRESS, BEACON, BEACON_ADDRESS);
    provider.call(BEACON_ADDRESS, "0x5c60da1b", Answer::Revert);
    provider.call(BEACON_ADDRESS, "0xda525716", Answer::Word("0x".into()));
    provider.call(BEACON_ADDRESS, "0xa619486e", Answer::Revert);
    provider.call(
        BEACON_ADDRESS,
        "0xbb82aa5e",
        Answer::Word(IMPLEMENTATION.into()),
    );
    assert_eq!(
        resolve(&mut provider, ADDRESS, &code(Candidate::Eip1967))
            .await
            .unwrap(),
        Detection::found("EIP1967Proxy", vec![IMPLEMENTATION.into()])
    );
    provider.done();
    let mut provider = Script::default();
    provider.storage(ADDRESS, IMPL, "0x0");
    provider.storage(ADDRESS, BEACON, BEACON_ADDRESS);
    provider.call(BEACON_ADDRESS, "0x5c60da1b", Answer::Failed);
    assert!(resolve(&mut provider, ADDRESS, &code(Candidate::Eip1967))
        .await
        .unwrap_err()
        .to_string()
        .contains("network"));
    provider.done();
}

#[tokio::test]
async fn zero_implementation_continues_to_next_unique_type_in_detected_order() {
    let input = format!(
        "0x{}{}{}{}",
        push(PROXIABLE),
        push(IMPL),
        push(IMPL),
        push(ZEPPELIN)
    );
    let mut provider = Script::default();
    provider.storage(ADDRESS, PROXIABLE, "0x0");
    provider.storage(ADDRESS, IMPL, "0x0");
    provider.storage(ADDRESS, BEACON, "0x0");
    provider.storage(ADDRESS, ZEPPELIN, IMPLEMENTATION);
    assert_eq!(
        resolve(&mut provider, ADDRESS, &input).await.unwrap(),
        Detection::found("ZeppelinOSProxy", vec![IMPLEMENTATION.into()])
    );
    provider.done();
}

#[tokio::test]
async fn livepeer_registry_preserves_full_unprefixed_key_and_zero_controller_short_circuits() {
    let mut provider = Script::default();
    let key = "ff12345678901234567890123456789012345678901234567890123456789012";
    provider.storage(ADDRESS, "0x0", BEACON_ADDRESS);
    provider.storage(ADDRESS, "0x1", key);
    provider.call(
        BEACON_ADDRESS,
        &format!("0xe16c7d98{key}"),
        Answer::Word(IMPLEMENTATION.into()),
    );
    assert_eq!(
        resolve(&mut provider, ADDRESS, &fixture("LivepeerManagerProxy"))
            .await
            .unwrap(),
        Detection::found("LivepeerManagerProxy", vec![IMPLEMENTATION.into()])
    );
    provider.done();
    provider.storage(ADDRESS, "0x0", "0x");
    assert_eq!(
        resolve(&mut provider, ADDRESS, &fixture("LivepeerManagerProxy"))
            .await
            .unwrap(),
        Detection::none()
    );
    provider.done();
}

#[tokio::test]
async fn diamond_preserves_empty_arrays_duplicates_order_and_zero_addresses() {
    for addresses in [
        vec![],
        vec![IMPLEMENTATION, ZERO, IMPLEMENTATION, BEACON_ADDRESS],
    ] {
        let mut provider = Script::default();
        provider.call(ADDRESS, "0x52ef6b2c", Answer::Word(array(&addresses)));
        assert_eq!(
            resolve(&mut provider, ADDRESS, &fixture("DiamondProxy"))
                .await
                .unwrap(),
            Detection::found(
                "DiamondProxy",
                addresses.iter().map(|s| s.to_string()).collect()
            )
        );
        provider.done();
    }
}

#[tokio::test]
async fn diamond_errors_are_ignored_but_do_not_hide_other_resolver_failures() {
    let bytecode = format!("0x{}{}", push(IMPL), push(SLOTS[5].0));
    for answer in [
        Answer::Revert,
        Answer::Failed,
        Answer::Word("0x01".into()),
        Answer::Word(format!("0x{:064x}{:064x}{}", 32, 1, "ff".repeat(32))),
    ] {
        let mut provider = Script::default();
        provider.call(ADDRESS, "0x52ef6b2c", answer);
        provider.storage(ADDRESS, IMPL, IMPLEMENTATION);
        assert_eq!(
            resolve(&mut provider, ADDRESS, &bytecode).await.unwrap(),
            Detection::found("EIP1967Proxy", vec![IMPLEMENTATION.into()])
        );
        provider.done();
    }
    let mut provider = Script::default();
    provider.call(ADDRESS, "0x52ef6b2c", Answer::Failed);
    assert_eq!(
        resolve(&mut provider, ADDRESS, &code(Candidate::Diamond))
            .await
            .unwrap(),
        Detection::none()
    );
}

#[tokio::test]
async fn fixed_clone_has_priority_over_diamond() {
    // PUSH32 after the clone return is visited before the boundary check upstream.
    let code = format!("{}{}", fixture("EIP1167Proxy"), push(SLOTS[5].0));
    let mut provider = Script::default();
    assert_eq!(
        resolve(&mut provider, ADDRESS, &code)
            .await
            .unwrap()
            .proxy_type,
        Some("EIP1167Proxy")
    );
    assert_eq!(provider.count, 0);
}

#[tokio::test(flavor = "current_thread")]
async fn large_detector_scan_is_cooperatively_cancellable() {
    let input = format!("0x{}", "01".repeat(2_000_000));
    let mut future = Box::pin(disasm(&input));
    assert!(futures_util::poll!(&mut future).is_pending());
    let result = tokio::time::timeout(std::time::Duration::from_millis(1), future).await;
    assert!(
        result.is_err(),
        "large CPU scan must yield for its deadline"
    );
}

#[tokio::test]
async fn all_zero_or_reverted_beacon_results_return_non_proxy() {
    let mut provider = Script::default();
    provider.storage(ADDRESS, IMPL, "0x0");
    provider.storage(ADDRESS, BEACON, BEACON_ADDRESS);
    for selector in ["0x5c60da1b", "0xda525716", "0xa619486e", "0xbb82aa5e"] {
        provider.call(BEACON_ADDRESS, selector, Answer::Revert);
    }
    assert_eq!(
        resolve(&mut provider, ADDRESS, &code(Candidate::Eip1967))
            .await
            .unwrap(),
        Detection::none()
    );
    provider.done();
}

#[tokio::test]
async fn many_fixed_addresses_keep_order_deduplication_and_cooperative_scan() {
    let mut input = String::from("0x");
    for address in 1..=10_000 {
        input.push_str(&format!("73{address:040x}5af4"));
    }
    input.push_str("7300000000000000000000000000000000000000015af4");
    let mut future = Box::pin(disasm(&input));
    let mut pending = 0;
    let output = std::future::poll_fn(|cx| match future.as_mut().poll(cx) {
        std::task::Poll::Pending => {
            pending += 1;
            cx.waker().wake_by_ref();
            std::task::Poll::Pending
        }
        ready => ready,
    })
    .await
    .unwrap();
    assert_eq!(output.proxies.len(), 10_000);
    assert_eq!(output.proxies[0], Candidate::Fixed(format!("0x{:040x}", 1)));
    assert_eq!(
        output.proxies[9_999],
        Candidate::Fixed(format!("0x{:040x}", 10_000))
    );
    // More yields than decoding alone: the disassembler itself must cooperate.
    assert!(pending > (input.len() - 2).div_ceil(8192));
}
