use anyhow::Result;
use arkiv_entity_processor::arkiv::{read_contract, ReadConfig};
use clap::Args;
use serde_json::json;
use std::{
    io::{BufWriter, Write},
    time::Duration,
};

pub const LONG_ABOUT: &str = "Read one Ethereum Mainnet contract from v3 entities in an Arkiv prefill node.\n\nAll queries and entity reads use one explicit historical block (default genesis 0).\nRequires the imported dataset owner and validates namespace, staging identities, payload\nchecksums, compressed entities and spill chunks using the shared hydrator. No local\nbundle or live Sourcify dependency. Writes the complete stored contract JSON only after\nverification succeeds; diagnostic read statistics go to stderr. This first read path\nis for processor v3 prefill keys, not arbitrary production uploads or live proxy state.";

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
    /// Fixed historical Arkiv block number, in decimal
    #[arg(long, default_value_t = 0)]
    block: u64,
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
    let result = read_contract(
        ReadConfig {
            rpc_url: args.rpc_url.clone(),
            owner: args.owner.clone(),
            arkiv_chain_id: args.arkiv_chain_id,
            block: args.block,
            timeout: Duration::from_secs(args.timeout_seconds),
            deadline: Duration::from_secs(args.deadline_seconds),
            max_rpc_requests: args.max_rpc_requests,
            max_rpc_response_bytes: args.max_rpc_response_bytes,
        },
        &args.address,
    )?;
    let stdout = std::io::stdout();
    let mut out = BufWriter::new(stdout.lock());
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
