//! Hydrate actual import wire entities through the shared bundle reader.
use anyhow::Result;
use arkiv_entity_processor::bundle::Bundle;
use clap::Args;
use std::{
    io::{BufWriter, Write},
    path::PathBuf,
};

pub const LONG_ABOUT: &str = "Reconstruct full contract JSON from a bundle's import-ready entities.\n\nWrites one hydrated contract per line to stdout, in bundle order. Requires seed.json,\nreport.json, SHA256SUMS and the entity parts; checks completion, counts and file checksums.\nReads gzip payloads and spill blobs; auxiliary payloads.bin and metadata.jsonl are not used.\nValidates entity identities, payload checksums and shared content hashes.\nRuns offline without RPC. Memory contains an index proportional to entity count and the\ncurrent contract/dependencies, never all bundle payloads. Output may be partial on error.";
#[derive(Args, Debug)]
pub struct HydrateArgs {
    /// Bundle directory (also supports extracted prefill archives)
    #[arg(value_name = "BUNDLE_DIR")]
    pub dir: PathBuf,
}
pub fn hydrate(args: &HydrateArgs) -> Result<()> {
    let bundle = Bundle::open(&args.dir)?;
    let stdout = std::io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    for entity in bundle.contracts() {
        let entity = entity?;
        let record = arkiv_entity_processor::hydrate::hydrate_with(&entity, |kind, hash, part| {
            bundle.resolve(kind, hash, part)
        })?;
        serde_json::to_writer(&mut out, &record)?;
        out.write_all(b"\n")?;
    }
    out.flush()?;
    Ok(())
}
