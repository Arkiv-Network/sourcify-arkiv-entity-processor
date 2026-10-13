//! Sourcify 4.1.2 proxy detection, ported from @shazow/whatsabi 0.29.0.
//!
//! This deliberately preserves the upstream heuristic order and creation-code quirks.
//! Only storage and calls go to Ethereum; the bytecode comes from the Arkiv snapshot.
//! The detector yields during long scans so the enclosing request deadline can cancel it.
//!
//! Portions copyright (c) 2022 Andrey Petrov, MIT; see proxy-whatsabi-LICENSE.txt.
use crate::backend::ReadFuture;
use anyhow::{ensure, Result};
use std::collections::{BTreeMap, HashSet};

pub trait Provider: Send {
    fn get_storage_at<'a>(&'a mut self, address: &'a str, slot: &'a str) -> ReadFuture<'a, String>;
    fn call<'a>(&'a mut self, address: &'a str, data: &'a str) -> ReadFuture<'a, String>;
}

/// Transport classification used only for the upstream beacon-selector fallback.
#[derive(Debug)]
pub struct Reverted;
impl std::fmt::Display for Reverted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("execution reverted")
    }
}
impl std::error::Error for Reverted {}

#[derive(Debug, PartialEq, Eq)]
pub struct Detection {
    pub is_proxy: bool,
    pub proxy_type: Option<&'static str>,
    pub implementations: Vec<String>,
}
impl Detection {
    fn none() -> Self {
        Self {
            is_proxy: false,
            proxy_type: None,
            implementations: vec![],
        }
    }
    fn found(name: &'static str, implementations: Vec<String>) -> Self {
        Self {
            is_proxy: true,
            proxy_type: Some(name),
            implementations,
        }
    }
}

const ZERO: &str = "0x0000000000000000000000000000000000000000";
const IMPL: &str = "0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc";
const BEACON: &str = "0xa3f0ad74e5423aebfd80d3ef4346578335a9a72aeaee59ff6cb3582b35133d50";
const ZEPPELIN: &str = "0x7050c9e0f4ca769c69bd3a8ef740bc37934f8e2c036e5a723fd8ee048ed3f8c3";
const PROXIABLE: &str = "0xc5f16f0fcc639fa48a6947836d9850f504798523bf8c9a3a87d5876cf622bcf7";
const MATIC: &str = "0xbaab7dbf64751104133af04abc7d9979f0fda3b059a322a8333f533d3f32bf7f";
const MATIC_PREIMAGE: &str = "6d617469632e6e6574776f726b2e70726f78792e696d706c656d656e746174696f6e";

#[derive(Clone, Debug, PartialEq, Eq)]
enum Candidate {
    Eip1967,
    Zeppelin,
    Proxiable,
    Safe,
    Diamond,
    Matic,
    Sequence,
    Livepeer,
    Fixed(String),
}
impl Candidate {
    fn name(&self) -> &'static str {
        match self {
            Self::Eip1967 => "EIP1967Proxy",
            Self::Zeppelin => "ZeppelinOSProxy",
            Self::Proxiable => "PROXIABLEProxy",
            Self::Safe => "GnosisSafeProxy",
            Self::Diamond => "DiamondProxy",
            Self::Matic => "MaticProxy",
            Self::Sequence => "SequenceWalletProxy",
            Self::Livepeer => "LivepeerManagerProxy",
            Self::Fixed(_) => "FixedProxy",
        }
    }
}
// This insertion order matters for constants in the unstructured data segment.
const SLOTS: &[(&str, Candidate)] = &[
    (IMPL, Candidate::Eip1967),
    (BEACON, Candidate::Eip1967),
    (ZEPPELIN, Candidate::Zeppelin),
    (PROXIABLE, Candidate::Proxiable),
    (
        "0xa619486e00000000000000000000000000000000000000000000000000000000",
        Candidate::Safe,
    ),
    (
        "0xc8fcad8db84d3cc18b4c41d551ea0ee66dd599cde068d998e57d5e09332c131b",
        Candidate::Diamond,
    ),
    (
        "0xc8fcad8db84d3cc18b4c41d551ea0ee66dd599cde068d998e57d5e09332c131c",
        Candidate::Diamond,
    ),
    (MATIC, Candidate::Matic),
    (
        "0xc8fcad8db84d3cc18b4c41d551ea0ee66dd599cde068d998e57d5e09332c131d",
        Candidate::Diamond,
    ),
    (
        "0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbd",
        Candidate::Eip1967,
    ),
];

pub async fn resolve(
    provider: &mut dyn Provider,
    address: &str,
    bytecode: &str,
) -> Result<Detection> {
    let p = disasm(bytecode).await?;
    if p.factory {
        return Ok(Detection::none());
    }
    if let Some(Candidate::Fixed(implementation)) =
        p.proxies.iter().find(|p| matches!(p, Candidate::Fixed(_)))
    {
        // Sourcify tests the ORIGINAL input, even if disasm found an embedded runtime.
        let stub = format!(
            "363d3d373d3d3d363d73{}5af43d82803e903d91602b57fd5bf3",
            &implementation[2..]
        );
        if bytecode
            .strip_prefix("0x")
            .unwrap_or(bytecode)
            .get(..stub.len())
            .is_some_and(|s| s.eq_ignore_ascii_case(&stub))
        {
            return Ok(Detection::found(
                "EIP1167Proxy",
                vec![implementation.clone()],
            ));
        }
    }
    if p.proxies.contains(&Candidate::Diamond) {
        // Sourcify ignores both a failed loupe call and an invalid ABI response.
        if let Ok(data) = provider.call(address, "0x52ef6b2c").await {
            if let Ok(addresses) = decode_address_array(&data).await {
                return Ok(Detection::found("DiamondProxy", addresses));
            }
        }
    }
    let mut checked = HashSet::from(["FixedProxy", "DiamondProxy"]);
    for (index, candidate) in p.proxies.into_iter().enumerate() {
        if index.is_multiple_of(4096) {
            tokio::task::yield_now().await;
        }
        if !checked.insert(candidate.name()) {
            continue;
        }
        let implementation = resolve_candidate(provider, address, &candidate).await?;
        if implementation != ZERO {
            return Ok(Detection::found(candidate.name(), vec![implementation]));
        }
    }
    Ok(Detection::none())
}

async fn resolve_candidate(
    provider: &mut dyn Provider,
    address: &str,
    candidate: &Candidate,
) -> Result<String> {
    let slot = match candidate {
        Candidate::Eip1967 => {
            let implementation = address_from(&provider.get_storage_at(address, IMPL).await?)?;
            if implementation != ZERO {
                return Ok(implementation);
            }
            let beacon = address_from(&provider.get_storage_at(address, BEACON).await?)?;
            if beacon == ZERO {
                return Ok(beacon);
            }
            for selector in ["0x5c60da1b", "0xda525716", "0xa619486e", "0xbb82aa5e"] {
                match provider.call(&beacon, selector).await {
                    Ok(data) => {
                        let implementation = address_from(&data)?;
                        if implementation != ZERO {
                            return Ok(implementation);
                        }
                    }
                    Err(error) if error.is::<Reverted>() => continue,
                    Err(error) => return Err(error),
                }
            }
            return Ok(ZERO.into());
        }
        Candidate::Safe => "0x0",
        Candidate::Zeppelin => ZEPPELIN,
        Candidate::Proxiable => PROXIABLE,
        Candidate::Matic => MATIC,
        Candidate::Sequence => {
            let suffix = address
                .strip_prefix("0x")
                .unwrap_or(address)
                .trim_start_matches('0');
            let slot = format!("0x{}", if suffix.is_empty() { "0" } else { suffix });
            return address_from(
                &provider
                    .get_storage_at(address, &slot.to_ascii_lowercase())
                    .await?,
            );
        }
        Candidate::Livepeer => {
            let controller = address_from(&provider.get_storage_at(address, "0x0").await?)?;
            if controller == ZERO {
                return Ok(controller);
            }
            let target = word_from(&provider.get_storage_at(address, "0x1").await?)?;
            return address_from(
                &provider
                    .call(&controller, &format!("0xe16c7d98{target}"))
                    .await?,
            );
        }
        Candidate::Diamond | Candidate::Fixed(_) => unreachable!("handled before resolver loop"),
    };
    address_from(&provider.get_storage_at(address, slot).await?)
}
fn word_from(data: &str) -> Result<String> {
    let data = data.strip_prefix("0x").unwrap_or(data);
    ensure!(
        data.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid Ethereum hex word"
    );
    Ok(format!("{data:0>64}").to_ascii_lowercase())
}
fn address_from(data: &str) -> Result<String> {
    let word = word_from(data)?;
    Ok(format!("0x{}", &word[word.len() - 40..]))
}
async fn decode_address_array(data: &str) -> Result<Vec<String>> {
    let mut bytes = Vec::new();
    for chunk in data
        .strip_prefix("0x")
        .unwrap_or(data)
        .as_bytes()
        .chunks(8192)
    {
        bytes.extend(hex::decode(chunk)?);
        tokio::task::yield_now().await;
    }
    let offset = abi_offset(
        bytes
            .get(..32)
            .ok_or_else(|| anyhow::anyhow!("missing ABI offset"))?,
    )?;
    let header = offset
        .checked_add(32)
        .ok_or_else(|| anyhow::anyhow!("ABI offset overflow"))?;
    let count = abi_offset(
        bytes
            .get(offset..header)
            .ok_or_else(|| anyhow::anyhow!("missing ABI length"))?,
    )?;
    ensure!(
        count <= bytes.len().saturating_sub(header) / 32,
        "truncated ABI address array"
    );
    let mut addresses = Vec::with_capacity(count);
    for (index, word) in bytes[header..].chunks_exact(32).take(count).enumerate() {
        if index.is_multiple_of(4096) {
            tokio::task::yield_now().await;
        }
        ensure!(
            word[..12].iter().all(|b| *b == 0),
            "invalid ABI address padding"
        );
        addresses.push(format!("0x{}", hex::encode(&word[12..])));
    }
    Ok(addresses)
}
fn abi_offset(word: &[u8]) -> Result<usize> {
    let mut value = 0usize;
    for byte in word {
        value = value
            .checked_mul(256)
            .and_then(|n| n.checked_add(*byte as usize))
            .ok_or_else(|| anyhow::anyhow!("ABI integer overflow"))?;
    }
    Ok(value)
}

#[derive(Default)]
struct Program {
    proxies: Vec<Candidate>,
    fixed: HashSet<String>,
    factory: bool,
    selectors: HashSet<String>,
    functions: Vec<Tags>,
    dests: BTreeMap<i128, usize>,
    init_proxies: Vec<Candidate>,
}
#[derive(Default)]
struct Tags {
    returns: bool,
    calldata: bool,
    delegate: bool,
}
fn push_width(op: u8) -> usize {
    if (0x60..=0x7f).contains(&op) {
        (op - 0x5f) as usize
    } else {
        0
    }
}
fn halt(op: u8) -> bool {
    op == 0 || op == 0xf3 || op >= 0xfd
}
fn offset(value: &[u8]) -> usize {
    // JS parseInt returns a value larger than any real code position (or NaN for
    // an empty truncated PUSH); both suppress the boundary comparison here.
    if value.is_empty() {
        return usize::MAX;
    }
    value
        .iter()
        .try_fold(0usize, |n, b| n.checked_mul(256)?.checked_add(*b as usize))
        .unwrap_or(usize::MAX)
}
fn padded_hex(value: &[u8], width: usize) -> String {
    format!("0x{:0>width$}", hex::encode(value), width = width * 2)
}

async fn disasm(bytecode: &str) -> Result<Program> {
    let source = bytecode.strip_prefix("0x").unwrap_or(bytecode).as_bytes();
    ensure!(source.len().is_multiple_of(2), "odd bytecode hex length");
    let mut bytes = Vec::with_capacity(source.len() / 2);
    // Decode in bounded chunks instead of monopolizing an async executor thread.
    for chunk in source.chunks(8192) {
        bytes.extend(hex::decode(chunk)?);
        tokio::task::yield_now().await;
    }
    let mut p = Program::default();
    p.functions.push(Tags::default());
    p.dests.insert(0, 0);
    let mut current = 0;
    let mut history = [usize::MAX; 5];
    let mut pos = 0;
    let mut instructions = 0usize;
    let mut inverted = String::new();
    let mut last_push32 = false;
    let mut check_jump_table = true;
    let mut resume = HashSet::new();
    let mut max_jump_dest = 0;
    let mut runtime_offset = 0;
    let mut boundary = None;
    while pos < bytes.len() {
        history.rotate_right(1);
        history[0] = pos;
        let op = bytes[pos];
        let at = |back: usize| bytes.get(history[back]).copied().unwrap_or(0x0c); // undefined is not a halt
                                                                                  // Upstream isPush uses !(op < PUSH1 || op > PUSH32), which also
                                                                                  // returns true for missing history (JavaScript undefined).
        let is_push = |back: usize| history[back] == usize::MAX || push_width(at(back)) > 0;
        let value = |back: usize| -> &[u8] {
            let start = history[back].saturating_add(1).min(bytes.len());
            let end = start.saturating_add(push_width(at(back))).min(bytes.len());
            &bytes[start..end]
        };
        let here = pos;
        pos += 1 + push_width(op);
        instructions += 1;
        if instructions.is_multiple_of(4096) {
            tokio::task::yield_now().await;
        }
        if op == 0x7f {
            let slot = padded_hex(value(0), 0);
            if let Some((_, candidate)) = SLOTS.iter().find(|(s, _)| *s == slot) {
                if p.proxies
                    .first()
                    .is_none_or(|first| first.name() != candidate.name())
                {
                    p.proxies.push(candidate.clone());
                } else {
                    last_push32 = !value(0).is_empty();
                }
            } else {
                last_push32 = !value(0).is_empty();
            }
            continue;
        } else if (0xa1..=0xa4).contains(&op) && last_push32 {
            continue;
        }
        if op == 0xf4 && at(1) == 0x5a {
            if is_push(2) {
                let v = value(2);
                // Preserve Uint8Array.slice(length - 20), including its negative-index behavior.
                let start = if v.len() >= 20 {
                    v.len() - 20
                } else {
                    (2 * v.len()).saturating_sub(20)
                };
                let address = padded_hex(&v[start..], 20);
                if p.fixed.insert(address.clone()) {
                    p.proxies.push(Candidate::Fixed(address));
                }
            } else if at(2) == 0x54 && at(3) == 0x30 {
                p.proxies.push(Candidate::Sequence);
            }
        }
        if op == 0x5b {
            if halt(at(1)) || at(1) == 0x56 {
                current = p.functions.len();
                p.functions.push(Tags::default());
                if check_jump_table && !p.selectors.is_empty() {
                    check_jump_table = false;
                }
                let destination = here as i128 - runtime_offset as i128;
                if destination >= 0 && resume.remove(&(destination as usize)) {
                    check_jump_table = matches!(bytes.get(here + 1), Some(0x80 | 0x35));
                }
            }
            p.dests
                .insert(here as i128 - runtime_offset as i128, current);
            continue;
        } else if (halt(at(1)) || at(1) == 0x56) && runtime_offset < here && max_jump_dest < here {
            boundary = Some(here);
            break;
        }
        if matches!(op, 0x56 | 0x57) && is_push(1) {
            max_jump_dest = max_jump_dest.max(offset(value(1)));
        }
        let tags = &mut p.functions[current];
        match op {
            0xf3 => tags.returns = true,
            0x35 => tags.calldata = true,
            0xf4 => tags.delegate = true,
            0xf0 | 0xf5 => p.factory = true,
            _ => {}
        }
        if op == 0x39 && matches!(at(1), 0x60 | 0x5f | 0x3d) && is_push(2) {
            runtime_offset = offset(value(2));
            resume.insert(runtime_offset);
            continue;
        }
        if here == runtime_offset && tags.returns && !tags.calldata {
            p = Program {
                init_proxies: p.proxies,
                ..Program::default()
            };
            p.functions.push(Tags::default());
            p.dests.insert(0, 0);
            current = 0;
            check_jump_table = true;
        }
        if !check_jump_table {
            continue;
        }
        if op == 0x56 && is_push(1) {
            if !inverted.is_empty() {
                p.selectors.insert(std::mem::take(&mut inverted));
            } else {
                resume.insert(offset(value(1)));
            }
        }
        if op != 0x57 || !is_push(1) {
            continue;
        }
        if at(2) == 0x14 && is_push(3) {
            p.selectors.insert(padded_hex(value(3), 4));
            continue;
        }
        if at(2) == 0x14 && at(3) == 0x81 && is_push(4) {
            p.selectors.insert(padded_hex(value(4), 4));
            continue;
        }
        if at(2) == 0x03 && is_push(3) {
            inverted = padded_hex(value(3), 4);
        }
        if at(2) == 0x15 && at(3) == 0x80 && matches!(at(4), 0x36 | 0x35 | 0x5b | 0x1c) {
            p.selectors.insert("0x00000000".into());
            continue;
        }
        if at(2) != 0x14 && (0x10..=0x14).contains(&at(2)) && at(4) == 0x80 {
            resume.insert(offset(value(1)));
        }
    }
    let mut data_end = bytes.len();
    if !bytes.last().is_some_and(|b| halt(*b)) || bytes.get(bytes.len().wrapping_sub(2)) == Some(&0)
    {
        let length = offset(&bytes[bytes.len().saturating_sub(2)..]);
        if let Some(start) = bytes.len().checked_sub(length.saturating_add(2)) {
            if boundary.is_none_or(|b| start >= b)
                && bytes.get(start).is_some_and(|b| b & 0xe0 == 0xa0)
            {
                data_end = start;
            }
        }
    }
    if let Some(start) = boundary.filter(|start| *start > 0 && p.proxies.is_empty()) {
        if data_end.saturating_sub(start) >= 32 {
            // Chunked matching retains upstream's nibble-aligned substring semantics.
            for (slot, candidate) in SLOTS {
                if contains_hex(&bytes[start..data_end], &slot[2..]).await {
                    p.proxies.push(candidate.clone());
                }
            }
            if contains_hex(&bytes[start..data_end], MATIC_PREIMAGE).await {
                p.proxies.push(Candidate::Matic);
            }
        }
    }
    if p.proxies.is_empty()
        && p.selectors.len() == 3
        && ["0xf77c4791", "0x92eefe9b", "0x51720b41"]
            .iter()
            .all(|s| p.selectors.contains(*s))
        && p.dests.values().any(|i| p.functions[*i].delegate)
    {
        p.proxies.push(Candidate::Livepeer);
    }
    if p.proxies.is_empty() {
        p.proxies = std::mem::take(&mut p.init_proxies);
    }
    Ok(p)
}
async fn contains_hex(bytes: &[u8], needle: &str) -> bool {
    let overlap = needle.len().div_ceil(2);
    for start in (0..bytes.len()).step_by(4096) {
        let end = (start + 4096 + overlap).min(bytes.len());
        if hex::encode(&bytes[start..end]).contains(needle) {
            return true;
        }
        tokio::task::yield_now().await;
    }
    false
}

#[cfg(test)]
#[path = "proxy_tests.rs"]
mod tests;
