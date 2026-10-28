//! `udc-example` and `udc-hydrate`: the UDC prototype's command line (docs/udc-proposal.md).
use anyhow::{bail, ensure, Context, Result};
use arkiv_entity_processor::udc::{decoded_view, Profile, Reader, UdcEntity, Val, Writer, BUDGET};
use arkiv_entity_processor::{hx, transform};
use clap::Args;
use serde_json::{json, Value};
use std::{fs, path::PathBuf};

pub const EXAMPLE_ABOUT: &str = "Write one contract record as UDC entities (prototype).\n\n\
Reads a Sourcify record (a JSON object, or the first line of an NDJSON file), runs the v3 split and \
writes the UDC entities in creation order with the keys a node assigns to salt-0 creates of OWNER \
from FIRST_NONCE on. Writes entities.json (node RPC encoding, every attribute with type and indexed \
flag), entities.decoded.json (bytes values decoded), hydrated-contract.json (the record rebuilt from \
those entities) and summary.json (sizes and storage units).";

#[derive(Args, Debug)]
pub struct ExampleArgs {
    /// Record file: JSON object or NDJSON (first line)
    #[arg(long)]
    pub input: PathBuf,
    /// Output directory (created; existing files are replaced)
    #[arg(long)]
    pub out: PathBuf,
    /// Entity owner and creator
    #[arg(long, default_value = "0x1111111111111111111111111111111111111111")]
    pub owner: String,
    /// Arkiv chain ID (part of the key derivation)
    #[arg(long, default_value_t = 7706815)]
    pub chain_id: u64,
    /// The owner's entity nonce before the first create
    #[arg(long, default_value_t = 0)]
    pub first_nonce: u64,
    /// arkiv.schema prefix: `<prefix>.<kind>`
    #[arg(long, default_value = "sourcify.v4")]
    pub prefix: String,
}

pub const HYDRATE_ABOUT: &str = "Rebuild a contract from UDC entities (prototype).\n\n\
ENTITIES is a JSON array of entities as `udc-example` writes them or as `arkiv_query` returns them \
(attributeSchema and attributes selected). The contract is found by address. Prints the record as \
pretty JSON; --reads prints the entities and attributes the read used to stderr.";

#[derive(Args, Debug)]
pub struct HydrateArgs {
    /// Entities file (JSON array)
    #[arg(long)]
    pub entities: PathBuf,
    /// Contract address
    #[arg(long)]
    pub address: String,
    /// arkiv.schema prefix
    #[arg(long, default_value = "sourcify.v4")]
    pub prefix: String,
    /// Owner every entity must have (the dataset owner)
    #[arg(long)]
    pub owner: String,
    /// Comma-separated top-level fields (minimal fields are always included); default all
    #[arg(long)]
    pub fields: Option<String>,
    /// Print the read plan to stderr
    #[arg(long)]
    pub reads: bool,
    /// Compact output instead of pretty
    #[arg(long)]
    pub compact: bool,
}

fn address(s: &str) -> Result<[u8; 20]> {
    let raw = hex::decode(s.strip_prefix("0x").context("address needs 0x")?)?;
    raw.try_into()
        .map_err(|_| anyhow::anyhow!("address must be 20 bytes"))
}
fn read_record(path: &PathBuf) -> Result<Value> {
    let text = fs::read_to_string(path)?;
    let value = match serde_json::from_str::<Value>(&text) {
        Ok(v) => v,
        Err(_) => serde_json::from_str(text.lines().next().context("empty input")?)?,
    };
    Ok(value)
}
fn pretty(v: &Value) -> Result<String> {
    Ok(serde_json::to_string_pretty(v)? + "\n")
}
fn kind(e: &UdcEntity) -> &str {
    e.schema().and_then(|s| s.rsplit('.').next()).unwrap_or("?")
}
/// Storage units of an entity without user attributes (512 overhead plus the seven system
/// cells): arkiv-state-db-experiment's `EMPTY_ENTITY_UNITS`, confirmed by a receipt's rentFee.
pub const SYSTEM_UNITS: u64 = 1460;

pub fn example(a: &ExampleArgs) -> Result<()> {
    let record = read_record(&a.input)?;
    let v3 = transform(&record)?;
    let profile = Profile {
        prefix: a.prefix.clone(),
        chain_id: a.chain_id,
        owner: address(&a.owner)?,
    };
    let mut writer = Writer::new(profile, a.first_nonce);
    let entities = writer.contract(&v3)?;
    let contract = entities.last().context("no entities")?.clone();
    let mut fetch = |k: &[u8; 32]| {
        entities
            .iter()
            .find(|e| &e.key == k)
            .cloned()
            .context("missing entity")
    };
    let mut reader = Reader::new(&a.prefix, Some(address(&a.owner)?), &mut fetch);
    let hydrated = reader.hydrate(&contract, None)?;
    fs::create_dir_all(&a.out)?;
    let all: Vec<Value> = entities.iter().map(UdcEntity::to_json).collect();
    fs::write(a.out.join("entities.json"), pretty(&json!(all))?)?;
    let decoded: Vec<Value> = entities.iter().map(decoded_view).collect();
    fs::write(
        a.out.join("entities.decoded.json"),
        pretty(&json!(decoded))?,
    )?;
    fs::write(a.out.join("hydrated-contract.json"), pretty(&hydrated)?)?;
    let rows: Vec<Value> = entities
        .iter()
        .enumerate()
        .map(|(i, e)| {
            json!({
                "nonce": a.first_nonce + i as u64,
                "kind": kind(e),
                "key": hx(e.key),
                "attributes": e.attributes.len(),
                "indexed": e.attributes.iter().filter(|x| x.indexed).count(),
                "bytesValues": e.bytes_len(),
                "userUnits": e.user_units(),
                "storageUnits": e.user_units() + SYSTEM_UNITS,
            })
        })
        .collect();
    let units: u64 = entities.iter().map(|e| e.user_units() + SYSTEM_UNITS).sum();
    let summary = json!({
        "input": a.input.file_name().map(|n| n.to_string_lossy().to_string()),
        "owner": a.owner.to_lowercase(),
        "chainId": a.chain_id,
        "firstNonce": a.first_nonce,
        "prefix": a.prefix,
        "budgetBytes": BUDGET,
        "entities": rows,
        "totalEntities": entities.len(),
        "totalBytesValues": entities.iter().map(UdcEntity::bytes_len).sum::<usize>(),
        "totalStorageUnits": units,
        "storageUnitsNote": "user cells (name + value + 2, +128 when indexed) plus 1,460 for the entity and its system cells, as arkiv-state-db-experiment charges rent",
    });
    fs::write(a.out.join("summary.json"), pretty(&summary)?)?;
    eprintln!(
        "{} entities, {} storage units -> {}",
        entities.len(),
        units,
        a.out.display()
    );
    Ok(())
}

pub fn hydrate(a: &HydrateArgs) -> Result<()> {
    let list: Value = serde_json::from_slice(&fs::read(&a.entities)?)?;
    let entities: Vec<UdcEntity> = list
        .as_array()
        .context("entities must be a JSON array")?
        .iter()
        .map(UdcEntity::from_json)
        .collect::<Result<_>>()?;
    let want = address(&a.address)?;
    let tag = format!("{}.contract", a.prefix);
    let contracts: Vec<&UdcEntity> = entities
        .iter()
        .filter(|e| e.schema() == Some(tag.as_str()) && e.get("address") == Some(&Val::Addr(want)))
        .collect();
    match contracts.len() {
        0 => bail!("no contract for {}", a.address),
        1 => {}
        n => bail!("{n} contracts for {}", a.address),
    }
    let contract = contracts[0].clone();
    let owner = Some(address(&a.owner)?);
    let mut fetch = |k: &[u8; 32]| {
        entities
            .iter()
            .find(|e| &e.key == k)
            .cloned()
            .with_context(|| format!("entity {} not in the input", hx(k)))
    };
    let mut reader = Reader::new(&a.prefix, owner, &mut fetch);
    let fields: Option<Vec<&str>> = a
        .fields
        .as_deref()
        .map(|f| f.split(',').filter(|s| !s.is_empty()).collect());
    let value = reader.hydrate(&contract, fields.as_deref())?;
    if a.reads {
        for r in &reader.reads {
            eprintln!("{} {} {}", r.kind, hx(r.key), r.attributes.join(","));
        }
    }
    let text = if a.compact {
        serde_json::to_string(&value)? + "\n"
    } else {
        pretty(&value)?
    };
    ensure!(!text.is_empty());
    print!("{text}");
    Ok(())
}

pub const CHECK_ABOUT: &str = "Check the UDC layout on a v3 bundle (prototype).\n\n\
For every EVERY-th contract of BUNDLE_DIR: hydrate it with the v3 reader, transform the record again, \
write it as UDC entities through one shared writer (so shared content is deduplicated across the \
sample), rebuild it with the UDC reader and compare with v3 hydration of the same entities, as \
serialized bytes. Prints a JSON summary; exits nonzero on any difference.";

#[derive(Args, Debug)]
pub struct CheckArgs {
    /// v3 bundle directory (seed.json, report.json, SHA256SUMS, entity parts)
    #[arg(value_name = "BUNDLE_DIR")]
    pub dir: PathBuf,
    /// Check every N-th contract
    #[arg(long, default_value_t = 1000)]
    pub every: usize,
    /// Stop after this many checked contracts
    #[arg(long)]
    pub limit: Option<usize>,
}

pub fn check(a: &CheckArgs) -> Result<()> {
    use arkiv_entity_processor::{bundle::Bundle, hydrate};
    use std::collections::HashMap;
    let started = std::time::Instant::now();
    let bundle = Bundle::open(&a.dir)?;
    eprintln!("bundle indexed in {:.0} s", started.elapsed().as_secs_f64());
    let profile = Profile {
        prefix: "sourcify.v4".into(),
        chain_id: 7706815,
        owner: [0x11; 20],
    };
    let mut writer = Writer::new(profile, 0);
    let mut store: HashMap<[u8; 32], UdcEntity> = HashMap::new();
    let (mut checked, mut failed, mut entities) = (0usize, 0usize, 0usize);
    let mut failures = vec![];
    for (i, entity) in bundle.contracts().enumerate() {
        if i % a.every != 0 {
            continue;
        }
        if a.limit.is_some_and(|n| checked >= n) {
            break;
        }
        let entity = entity?;
        let record =
            hydrate::hydrate_with(&entity, |kind, hash, part| bundle.resolve(kind, hash, part))?;
        let address = record["address"].as_str().unwrap_or("?").to_string();
        let outcome = (|| -> Result<()> {
            let v3 = transform(&record)?;
            let expected = serde_json::to_string(&hydrate::hydrate(v3.last().unwrap(), &v3)?)?;
            let created = writer.contract(&v3)?;
            entities += created.len();
            for e in &created {
                e.validate()?;
                store.insert(e.key, e.clone());
            }
            let contract = created.last().context("no contract entity")?.clone();
            let mut fetch = |k: &[u8; 32]| store.get(k).cloned().context("missing entity");
            let mut reader = Reader::new("sourcify.v4", Some([0x11; 20]), &mut fetch);
            let got = serde_json::to_string(&reader.hydrate(&contract, None)?)?;
            ensure!(got == expected, "hydration differs");
            ensure!(
                serde_json::from_str::<Value>(&got)? == record,
                "value differs from the bundle record"
            );
            Ok(())
        })();
        checked += 1;
        if let Err(e) = outcome {
            failed += 1;
            if failures.len() < 20 {
                failures.push(json!({"address": address, "error": format!("{e:#}")}));
            }
        }
        if checked % 500 == 0 {
            eprintln!(
                "{checked} checked, {failed} failed, {:.0} s",
                started.elapsed().as_secs_f64()
            );
        }
    }
    let summary = json!({"checked": checked, "failed": failed, "udcEntities": entities, "seconds": started.elapsed().as_secs(), "failures": failures});
    println!("{}", serde_json::to_string_pretty(&summary)?);
    ensure!(failed == 0, "{failed} contracts differ");
    Ok(())
}

pub const PAGE_ABOUT: &str = "List a v4 dataset's contracts in matchid order (prototype of the paging rule).\n\n\
Pages by matchid windows (docs/udc-proposal.md, \"Paging\"): a window query with fewer than --limit rows, or no \
cursor, is complete, whatever order the node returns; a full one is narrowed to the window its N-th row bounds. \
Never relies on node order or cursors across queries, so it gives the same pages on our node (pinned to one block) \
and on Jalebi (head), also after contracts were patched in place. Prints one JSON object: the rows (matchid, \
address, key), the next cursor, the window width for the next page and the queries made.";

#[derive(Args, Debug)]
pub struct PageArgs {
    #[arg(long, default_value = "http://127.0.0.1:8745")]
    pub rpc_url: String,
    #[arg(long)]
    pub owner: String,
    #[arg(long, default_value_t = 7706815)]
    pub arkiv_chain_id: u64,
    #[arg(long, default_value = "sourcify.v4")]
    pub schema_prefix: String,
    /// Extra positive clauses, e.g. "language = str('Vyper')"
    #[arg(long)]
    pub filter: Option<String>,
    /// Newest first (descending matchid)
    #[arg(long)]
    pub descending: bool,
    /// The previous page's last matchid
    #[arg(long)]
    pub cursor: Option<u64>,
    /// Contracts per page (below --limit)
    #[arg(long, default_value_t = 20)]
    pub size: usize,
    /// Rows per query (at most 100 on Jalebi)
    #[arg(long, default_value_t = 100)]
    pub limit: usize,
    /// First window width in matchid units (pass the previous page's `width`)
    #[arg(long, default_value_t = arkiv_entity_processor::udc_page::WIDTH)]
    pub width: u64,
    /// Read the head only (Jalebi); default pins the head block on a node that keeps history
    #[arg(long)]
    pub head: bool,
    /// Walk every page and print them all
    #[arg(long)]
    pub all: bool,
}

pub fn page(a: &PageArgs) -> Result<()> {
    use arkiv_entity_processor::udc_rpc::{page_contracts, PageRequest, UdcReadConfig, View};
    let config = || -> Result<UdcReadConfig> {
        Ok(UdcReadConfig {
            rpc_url: a.rpc_url.clone(),
            api_key: std::env::var("ARKIV_RPC_API_KEY")
                .ok()
                .filter(|k| !k.is_empty()),
            owner: address(&a.owner)?,
            arkiv_chain_id: a.arkiv_chain_id,
            prefix: a.schema_prefix.clone(),
            view: if a.head { View::Head } else { View::Auto },
            fields: None,
            timeout: std::time::Duration::from_secs(30),
            deadline: std::time::Duration::from_secs(120),
            max_rpc_requests: 10_000,
            max_rpc_response_bytes: 8 << 20,
        })
    };
    let (mut cursor, mut width, mut pages) = (a.cursor, a.width, vec![]);
    loop {
        let page = page_contracts(
            config()?,
            PageRequest {
                filter: a.filter.clone(),
                descending: a.descending,
                cursor,
                size: a.size,
                limit: a.limit,
                width,
            },
        )?;
        let full = page.rows.len() == a.size;
        pages.push(json!({
            "rows": page.rows.iter().map(|(m, addr, k)| json!({"matchid": m, "address": addr, "key": hx(k)})).collect::<Vec<_>>(),
            "next": page.next, "width": page.width, "queries": page.queries, "view": page.view,
            "blocks": [page.blocks.0, page.blocks.1],
        }));
        if !a.all || !full {
            break;
        }
        (cursor, width) = (page.next, page.width);
    }
    println!("{}", pretty(&json!(pages))?.trim_end());
    Ok(())
}
