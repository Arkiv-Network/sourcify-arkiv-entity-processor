//! Re-keys a v4 bundle for another Arkiv chain or owner (docs/udc-proposal.md, "Keys").
//!
//! A key is `keccak256(chainId ‖ 0x44…44 ‖ owner ‖ nonce ‖ salt)`, so the bundle a `process` run
//! would have written for another chain or owner differs only in its keys: each line's `key`, its
//! key attributes, its `sourcerefs` and the keys in its chunk lists. Content hashes, chunk-list
//! digests, sizes and storage units do not change.
//!
//! Every key in a bundle is the salt-0 key of an earlier nonce, so the old key of every nonce is
//! computed up front (in parallel) into a sorted index, and then every part is rewritten on its
//! own thread: each line is parsed, its nonce and key checked, each key it names looked up (it
//! must be an earlier line) and replaced by the new key of the same nonce, and the line written
//! with the writer's own encoding. The result is byte-identical to `process` with the new chain
//! and owner. Every input part's size and SHA-256 are checked against report.json while it is
//! read; nothing is published unless every part passed. `verify` checks the output in full.
use crate::udc::{chunk_manifest, entity_key, UdcEntity, Val, IDENTITY};
use crate::udc_bundle::{manifest, publish, read_sums, Manifest, Part};
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{BufRead, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    time::Instant,
};

pub const LONG_ABOUT: &str = "\
Re-key a v4 (UDC) bundle for another Arkiv chain or owner.

Every key in a v4 bundle is derived from (chain, owner, nonce, salt 0), so moving a dataset to
another chain or owner rewrites only keys: each line's key, its key attributes, its sourcerefs and
the keys in its chunk lists. Content, hashes, sizes and storage units are unchanged, and the output
is byte-identical to what `process` writes for the new chain and owner.

The input is checked while it is read (every part's size and SHA-256, every line's nonce and key,
every named key an earlier line). The output directory must not exist; seed.json is published
last. Run `verify` on the output for the full structural check.";

/// Totals of a re-keying run.
#[derive(Debug)]
pub struct Rekeyed {
    pub entities: u64,
    pub references: u64,
    pub seconds: f64,
}

/// Old salt-0 keys of nonces `0..n`, sorted, for the nonce a referenced key belongs to.
struct Index(Vec<([u8; 32], u32)>);
impl Index {
    fn build(chain_id: u64, owner: &[u8; 20], n: u64, threads: usize) -> Result<Self> {
        ensure!(n <= u32::MAX as u64, "too many entities to index");
        let mut keys = vec![([0u8; 32], 0u32); n as usize];
        let per = keys.len().div_ceil(threads).max(1);
        std::thread::scope(|s| {
            for (i, chunk) in keys.chunks_mut(per).enumerate() {
                s.spawn(move || {
                    for (j, slot) in chunk.iter_mut().enumerate() {
                        let nonce = (i * per + j) as u64;
                        *slot = (entity_key(chain_id, owner, nonce, 0), nonce as u32);
                    }
                });
            }
        });
        keys.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        Ok(Self(keys))
    }
    fn nonce(&self, key: &[u8; 32]) -> Option<u64> {
        self.0
            .binary_search_by(|(k, _)| k.cmp(key))
            .ok()
            .map(|i| self.0[i].1 as u64)
    }
}

struct Job<'a> {
    from: &'a Manifest,
    index: &'a Index,
    chain_id: u64,
    owner: [u8; 20],
}
impl Job<'_> {
    /// The new key for a key line `nonce` names: it must be the old key of an earlier line.
    fn map(&self, key: &[u8; 32], nonce: u64) -> Result<[u8; 32]> {
        let target = self
            .index
            .nonce(key)
            .with_context(|| format!("names {}, which is no key of the bundle", crate::hx(key)))?;
        ensure!(
            target < nonce,
            "names {}, which is not an earlier line",
            crate::hx(key)
        );
        Ok(entity_key(self.chain_id, &self.owner, target, 0))
    }
    /// Rewrites every key of one line; returns how many keys it names.
    fn rekey(&self, e: &mut UdcEntity, nonce: u64) -> Result<u64> {
        let chunk = e.schema().is_some_and(|s| s.ends_with(".chunk"));
        let mut named = 0;
        e.key = entity_key(self.chain_id, &self.owner, nonce, 0);
        for a in &mut e.attributes {
            match &mut a.value {
                Val::Key(k) => {
                    *k = self.map(k, nonce)?;
                    named += 1;
                }
                // A chunk's data is raw bytes, never a list of keys.
                Val::Bytes(b) if !chunk => {
                    let keys = if a.name == "sourcerefs" {
                        ensure!(
                            b.first() == Some(&IDENTITY)
                                && b.len() > 1
                                && (b.len() - 1).is_multiple_of(32),
                            "malformed sourcerefs"
                        );
                        1
                    } else if chunk_manifest(b)?.is_some() {
                        37
                    } else {
                        continue;
                    };
                    for slot in b[keys..].chunks_mut(32) {
                        let old: [u8; 32] = (&*slot).try_into().unwrap();
                        let new = self.map(&old, nonce)?;
                        slot.copy_from_slice(&new);
                        named += 1;
                    }
                }
                _ => {}
            }
        }
        Ok(named)
    }
    /// Rewrites one part into `out`; returns its report.json audit and the keys it names.
    fn part(&self, input: &Path, part: &Part, out: &Path) -> Result<(Value, u64)> {
        let mut reader = BufReader::with_capacity(
            1 << 20,
            File::open(input.join(&part.path)).with_context(|| part.path.clone())?,
        );
        let mut writer = BufWriter::with_capacity(
            1 << 20,
            File::create(out.join(&part.path)).with_context(|| part.path.clone())?,
        );
        let (mut read, mut written) = (Sha256::new(), Sha256::new());
        let (mut in_bytes, mut out_bytes, mut count, mut named) = (0u64, 0u64, 0u64, 0u64);
        let mut line = Vec::new();
        loop {
            line.clear();
            let n = reader.read_until(b'\n', &mut line)?;
            if n == 0 {
                break;
            }
            read.update(&line);
            in_bytes += n as u64;
            ensure!(
                line.ends_with(b"\n"),
                "{}: last line unterminated",
                part.path
            );
            let nonce = part.first_nonce + count;
            let rewritten = (|| -> Result<Vec<u8>> {
                let value: Value = serde_json::from_slice(&line).context("invalid JSON")?;
                let (n, mut e) = UdcEntity::from_line(&value, self.from.owner)?;
                ensure!(n == nonce, "nonce {n}, expected {nonce}");
                ensure!(
                    e.key == entity_key(self.from.chain_id, &self.from.owner, nonce, 0),
                    "key is not the salt-0 key of nonce {nonce}"
                );
                named += self.rekey(&mut e, nonce)?;
                let mut text = serde_json::to_vec(&e.to_line(nonce))?;
                text.push(b'\n');
                Ok(text)
            })()
            .with_context(|| format!("{} line {}", part.path, count + 1))?;
            written.update(&rewritten);
            out_bytes += rewritten.len() as u64;
            writer.write_all(&rewritten)?;
            count += 1;
        }
        ensure!(
            in_bytes == part.bytes && hex::encode(read.finalize()) == part.sha256,
            "{}: checksum mismatch",
            part.path
        );
        ensure!(
            count == part.entities,
            "{}: entity count differs",
            part.path
        );
        writer.flush()?;
        writer.get_ref().sync_all()?;
        Ok((
            json!({
                "path": part.path,
                "bytes": out_bytes,
                "sha256": hex::encode(written.finalize()),
                "entities": count,
                "firstNonce": part.first_nonce,
            }),
            named,
        ))
    }
}

/// Re-keys the bundle at `input` for `chain_id` and `owner` into the new directory `output`.
pub fn rekey(
    input: &Path,
    output: &Path,
    chain_id: u64,
    owner: [u8; 20],
    threads: usize,
) -> Result<Rekeyed> {
    let started = Instant::now();
    ensure!(threads > 0, "threads must be positive");
    ensure!(owner != [0; 20], "owner must be nonzero");
    let sums = read_sums(input)?;
    let from = manifest(input, &sums)?;
    ensure!(
        (chain_id, owner) != (from.chain_id, from.owner),
        "the bundle already has this chain and owner"
    );
    ensure!(
        !output.exists(),
        "output already exists; use a fresh directory (bundles are never resumed)"
    );
    fs::create_dir_all(output.join("entities"))?;
    for part in &from.parts {
        if let Some(parent) = Path::new(&part.path).parent() {
            fs::create_dir_all(output.join(parent))?;
        }
    }
    let index = Index::build(from.chain_id, &from.owner, from.entities, threads)?;
    let job = Job {
        from: &from,
        index: &index,
        chain_id,
        owner,
    };
    let next = AtomicUsize::new(0);
    let failed = AtomicBool::new(false);
    let mut results: Vec<Option<Result<(Value, u64)>>> =
        (0..from.parts.len()).map(|_| None).collect();
    let done = std::sync::Mutex::new(&mut results);
    std::thread::scope(|s| {
        for _ in 0..threads.min(from.parts.len()) {
            s.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                if i >= from.parts.len() || failed.load(Ordering::Relaxed) {
                    break;
                }
                let result = job.part(input, &from.parts[i], output);
                if result.is_err() {
                    failed.store(true, Ordering::Relaxed);
                }
                done.lock().unwrap()[i] = Some(result);
            });
        }
    });
    let (mut audits, mut references) = (vec![], 0u64);
    for result in results {
        match result {
            Some(Ok((audit, named))) => {
                audits.push(audit);
                references += named;
            }
            Some(Err(e)) => return Err(e),
            None => bail!("a part was not re-keyed after another failed"),
        }
    }
    let mut report: Value = serde_json::from_slice(&fs::read(input.join("report.json"))?)?;
    let seconds = started.elapsed().as_secs_f64();
    let fields = report.as_object_mut().context("report.json")?;
    fields.insert("arkivChainId".into(), json!(chain_id));
    fields.insert("owner".into(), json!(crate::hx(owner)));
    fields.insert("parts".into(), json!(audits));
    fields.insert(
        "rekey".into(),
        json!({
            "from": {
                "arkivChainId": from.chain_id,
                "owner": crate::hx(from.owner),
                "seedSha256": sums["seed.json"],
                "reportSha256": sums["report.json"],
            },
            "references": references,
            "threads": threads,
            "seconds": seconds,
        }),
    );
    publish(output, &report, chain_id, &owner, &from.prefix)?;
    Ok(Rekeyed {
        entities: from.entities,
        references,
        seconds,
    })
}

#[derive(clap::Args, Debug)]
pub struct RekeyArgs {
    /// v4 bundle directory to re-key (left unchanged)
    #[arg(value_name = "BUNDLE_DIR")]
    pub input: PathBuf,
    /// Output bundle directory (must not exist)
    #[arg(short, long, value_name = "DIR")]
    pub output: PathBuf,
    /// New owner/creator address (default: the bundle's)
    #[arg(long, value_name = "ADDRESS")]
    pub owner: Option<String>,
    /// New destination Arkiv chain ID (default: the bundle's). Alias: --chain-id.
    #[arg(long = "destination-chain-id", alias = "chain-id", value_name = "ID")]
    pub chain_id: Option<u64>,
    /// Worker threads (default: available parallelism)
    #[arg(long, value_name = "N")]
    pub threads: Option<usize>,
}

pub fn run(a: &RekeyArgs) -> Result<()> {
    let sums = read_sums(&a.input)?;
    let from = manifest(&a.input, &sums)?;
    let owner = match &a.owner {
        None => from.owner,
        Some(text) => {
            ensure!(
                text.starts_with("0x") && text.len() == 42,
                "--owner must be a 0x-prefixed 20-byte hex address"
            );
            hex::decode(&text[2..])
                .context("--owner must be hex")?
                .try_into()
                .unwrap()
        }
    };
    let threads = a
        .threads
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, |n| n.get()));
    let r = rekey(
        &a.input,
        &a.output,
        a.chain_id.unwrap_or(from.chain_id),
        owner,
        threads,
    )?;
    println!(
        "OK {}: {} entities, {} keys named, re-keyed in {:.1} s",
        a.output.display(),
        r.entities,
        r.references,
        r.seconds
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{transform, udc::Profile, udc_bundle::BundleWriter};
    use serde_json::Map;

    fn record(i: u64) -> Value {
        // Two sources per contract, half of them shared, and one source large enough to be chunked.
        let big = if i == 2 {
            // Hex of a hash chain does not compress below the 120,000-byte chunk size.
            (0..12_000u32)
                .map(|j| format!("// {}\n", hex::encode(crate::sha(j.to_be_bytes()))))
                .collect::<String>()
        } else {
            String::new()
        };
        json!({"chainId":"1","address":format!("0x{:040x}", i),"matchId":i.to_string(),"verifiedAt":"2020-01-01T00:00:00Z",
            "abi":[],"compilation":{"language":"Solidity","compiler":"solc","compilerVersion":"1","compilerSettings":{"optimizer":{"enabled":true,"runs":200}},"fullyQualifiedName":"A.sol:A","name":"A"},
            "sources":{"A.sol":{"content":format!("contract A{} {{}}{big}", i % 2)},"B.sol":{"content":format!("library B{i} {{}}")}},
            "creationBytecode":{"onchainBytecode":"0x6001","recompiledBytecode":"0x6001"},
            "runtimeBytecode":{"onchainBytecode":"0x6000","recompiledBytecode":"0x6000"},
            "deployment":{"deployer":"0x2222222222222222222222222222222222222222","blockNumber":"7"}})
    }
    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("udc-rekey-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        d
    }
    fn write(name: &str, chain_id: u64, owner: [u8; 20]) -> PathBuf {
        let d = dir(name);
        let profile = Profile {
            prefix: "sourcify.v4".into(),
            chain_id,
            owner,
        };
        let mut w = BundleWriter::new(&d, profile, 5).unwrap();
        for i in 1..=6 {
            w.record(&transform(&record(i)).unwrap()).unwrap();
        }
        w.finish(6, Map::new()).unwrap();
        d
    }
    fn parts(d: &Path) -> Vec<Vec<u8>> {
        let seed: Value = serde_json::from_slice(&fs::read(d.join("seed.json")).unwrap()).unwrap();
        seed["entities"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| fs::read(d.join(p["path"].as_str().unwrap())).unwrap())
            .collect()
    }

    #[test]
    fn rekeying_equals_processing_for_the_new_chain_and_owner() {
        let a = write("a", 7706815, [0x11; 20]);
        let b = write("b", 60138453, [0x22; 20]);
        let out = dir("a-to-b");
        let r = rekey(&a, &out, 60138453, [0x22; 20], 3).unwrap();
        assert!(r.references > 0);
        // Every part byte for byte, and seed.json too.
        assert_eq!(parts(&out), parts(&b));
        assert_eq!(
            fs::read(out.join("seed.json")).unwrap(),
            fs::read(b.join("seed.json")).unwrap()
        );
        // The bundle has every kind of key: key attributes, sourcerefs and a chunk list.
        let text = String::from_utf8(parts(&a).concat()).unwrap();
        assert!(text.contains("sourcify.v4.chunk"), "no chunk list");
        assert!(text.contains("\"sourcerefs\""), "no sourcerefs");
        // The output is a complete bundle, and its report says where it came from.
        crate::udc_bundle::check(&out, &manifest(&out, &read_sums(&out).unwrap()).unwrap())
            .unwrap();
        let report: Value =
            serde_json::from_slice(&fs::read(out.join("report.json")).unwrap()).unwrap();
        assert_eq!(report["rekey"]["from"]["owner"], crate::hx([0x11; 20]));
        assert_eq!(report["owner"], crate::hx([0x22; 20]));
        // And back again.
        let back = dir("b-to-a");
        rekey(&out, &back, 7706815, [0x11; 20], 1).unwrap();
        assert_eq!(parts(&back), parts(&a));
        for d in [a, b, out, back] {
            fs::remove_dir_all(d).unwrap();
        }
    }

    #[test]
    fn rekeying_refuses_bad_input_and_publishes_nothing() {
        let a = write("bad", 7706815, [0x11; 20]);
        // Same chain and owner, an existing output.
        assert!(rekey(&a, &dir("same"), 7706815, [0x11; 20], 2)
            .unwrap_err()
            .to_string()
            .contains("already has"));
        assert!(rekey(&a, &a, 1, [0x22; 20], 2)
            .unwrap_err()
            .to_string()
            .contains("already exists"));
        // A part changed after report.json was written.
        let part = a.join("entities/000002.jsonl");
        let mut bytes = fs::read(&part).unwrap();
        let at = bytes.len() - 3;
        bytes[at] ^= 1;
        fs::write(&part, bytes).unwrap();
        let out = dir("bad-out");
        let error = format!("{:#}", rekey(&a, &out, 1, [0x22; 20], 2).unwrap_err());
        assert!(error.contains("000002.jsonl"), "{error}");
        assert!(!out.join("seed.json").exists());
        fs::remove_dir_all(a).unwrap();
        let _ = fs::remove_dir_all(out);
    }
}
