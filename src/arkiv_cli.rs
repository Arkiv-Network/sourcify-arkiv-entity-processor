use anyhow::{ensure, Context, Result};
use arkiv_entity_processor::{
    arkiv::{read_contract, ReadConfig},
    udc_rpc::{self, UdcReadConfig, View},
};
use clap::Args;
use serde_json::json;
use std::{
    io::{BufWriter, Write},
    time::Duration,
};

pub const LONG_ABOUT: &str = "Read one Ethereum Mainnet contract from an Arkiv node and hydrate it.\n\n--format v3 (default): v3 entities of a prefill node. All queries and entity reads use one\nexplicit historical block (default genesis 0). Requires the imported dataset owner and validates\nnamespace, staging identities, payload checksums, compressed entities and spill chunks using the\nshared hydrator. This read path is for processor v3 prefill keys, not arbitrary production\nuploads or live proxy state.\n\n--format udc: v4 UDC entities (docs/udc-proposal.md): the contract by owner, arkiv.schema,\nchainid and address, then its dependencies by key, at most 8 keys per query, with only the\nattributes --fields needs. --read-view pinned reads every query at one block (--block, default\nthe head when the read starts) on a node that keeps history (our node); head reads the head\n(Jalebi, which keeps no history); auto (default) asks the node for the block before its head and\npins when it answers. ARKIV_RPC_API_KEY, when set, is sent as the X-API-KEY header.\n\nNo local bundle or live Sourcify dependency. Writes the stored contract JSON only after\nverification succeeds; diagnostic read statistics go to stderr.";

/// Entity format read by `read-arkiv`.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadFormat {
    V3,
    Udc,
}
/// How a `udc` read sees the chain.
#[derive(clap::ValueEnum, Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadView {
    Auto,
    Pinned,
    Head,
}

#[derive(Args, Debug)]
pub struct ReadArkivArgs {
    /// Ethereum Mainnet contract address
    #[arg(value_name = "ADDRESS")]
    address: String,
    /// Arkiv JSON-RPC endpoint
    #[arg(long, default_value = "http://127.0.0.1:8745")]
    rpc_url: String,
    /// Expected imported entity owner and creator (public address, not credentials)
    #[arg(long)]
    owner: String,
    /// Expected destination Arkiv chain ID; the source chain is always Mainnet 1
    #[arg(long, default_value_t = 7706815)]
    arkiv_chain_id: u64,
    /// Entity format: v3 payload entities or udc (v4) entities
    #[arg(long, value_enum, default_value = "v3")]
    format: ReadFormat,
    /// Fixed historical Arkiv block number, in decimal [v3 default: 0; udc: pins this block]
    #[arg(long)]
    block: Option<u64>,
    /// udc: arkiv.schema prefix of the dataset
    #[arg(long, default_value = "sourcify.v4")]
    schema_prefix: String,
    /// udc: pinned (one block), head (Jalebi) or auto
    #[arg(long, value_enum, default_value = "auto")]
    read_view: ReadView,
    /// udc: comma-separated top-level fields to hydrate (minimal fields always) [default: all]
    #[arg(long)]
    fields: Option<String>,
    /// Maximum duration of one RPC call, including response body
    #[arg(long, default_value_t = 30)]
    timeout_seconds: u64,
    /// Overall deadline checked at RPC boundaries; individual hydration steps are synchronous
    #[arg(long, default_value_t = 120)]
    deadline_seconds: u64,
    /// Total RPC call budget, including queries and block checks
    #[arg(long, default_value_t = 100000)]
    max_rpc_requests: u64,
    /// Per-RPC JSON envelope bound; does not limit the assembled contract size
    #[arg(long, default_value_t = 1048576)]
    max_rpc_response_bytes: u64,
}

pub fn read(args: &ReadArkivArgs) -> Result<()> {
    let stdout = std::io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    if args.format == ReadFormat::Udc {
        ensure!(
            args.block.is_none() || args.read_view != ReadView::Head,
            "--block pins a block; it cannot be read with --read-view head"
        );
        let owner = args.owner.to_ascii_lowercase();
        ensure!(
            owner.starts_with("0x") && owner.len() == 42,
            "invalid owner"
        );
        let result = udc_rpc::read_contract(
            UdcReadConfig {
                rpc_url: args.rpc_url.clone(),
                api_key: std::env::var("ARKIV_RPC_API_KEY")
                    .ok()
                    .filter(|k| !k.is_empty()),
                owner: hex::decode(&owner[2..])?
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("invalid owner"))
                    .context("owner")?,
                arkiv_chain_id: args.arkiv_chain_id,
                prefix: args.schema_prefix.clone(),
                view: match (args.read_view, args.block) {
                    (_, Some(block)) => View::Pinned(Some(block)),
                    (ReadView::Pinned, None) => View::Pinned(None),
                    (ReadView::Head, None) => View::Head,
                    (ReadView::Auto, None) => View::Auto,
                },
                fields: args.fields.as_ref().map(|f| {
                    f.split(',')
                        .filter(|s| !s.is_empty())
                        .map(String::from)
                        .collect()
                }),
                timeout: Duration::from_secs(args.timeout_seconds),
                deadline: Duration::from_secs(args.deadline_seconds),
                max_rpc_requests: args.max_rpc_requests,
                max_rpc_response_bytes: args.max_rpc_response_bytes,
            },
            &args.address,
        )?;
        serde_json::to_writer(&mut out, &result.contract)?;
        out.write_all(b"\n")?;
        out.flush()?;
        eprintln!(
            "{}",
            json!({"readArkiv":{"format":"udc","view":result.view,"blocks":[result.blocks.0,result.blocks.1],
            "rpcRequests":result.rpc_requests,"queries":result.queries,"entities":result.entities,
            "rpcResponseBytes":result.response_bytes,"elapsedSeconds":result.elapsed.as_secs_f64()}})
        );
        return Ok(());
    }
    let result = read_contract(
        ReadConfig {
            rpc_url: args.rpc_url.clone(),
            owner: args.owner.clone(),
            arkiv_chain_id: args.arkiv_chain_id,
            block: args.block.unwrap_or(0),
            timeout: Duration::from_secs(args.timeout_seconds),
            deadline: Duration::from_secs(args.deadline_seconds),
            max_rpc_requests: args.max_rpc_requests,
            max_rpc_response_bytes: args.max_rpc_response_bytes,
        },
        &args.address,
    )?;
    serde_json::to_writer(&mut out, &result.contract)?;
    out.write_all(b"\n")?;
    out.flush()?;
    eprintln!(
        "{}",
        json!({"readArkiv":{"block":result.block,"blockHash":result.block_hash,
        "rpcRequests":result.rpc_requests,"entityReads":result.entity_reads,"queries":result.queries,
        "rpcResponseBytes":result.response_bytes,"elapsedSeconds":result.elapsed.as_secs_f64()}})
    );
    Ok(())
}
