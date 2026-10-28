//! Hydrate actual import wire entities through the shared bundle reader.
use anyhow::{Context, Result};
use arkiv_entity_processor::{
    bundle::Bundle,
    udc::Reader,
    udc_bundle::{seed_version, UdcBundle},
};
use clap::Args;
use std::{
    io::{BufWriter, Write},
    path::PathBuf,
};

pub const LONG_ABOUT: &str = "Reconstruct full contract JSON from a bundle's import-ready entities.\n\nWrites one hydrated contract per line to stdout, in bundle order. Requires seed.json,\nreport.json, SHA256SUMS and the entity parts; checks completion, counts and file checksums.\nRuns offline without RPC. Output may be partial on error.\n\nv3 bundles (seed.json version 1): reads gzip payloads and spill blobs; auxiliary payloads.bin\nand metadata.jsonl are not used. Validates entity identities, payload checksums and shared\ncontent hashes. Memory contains an index proportional to entity count and the current\ncontract/dependencies, never all bundle payloads.\n\nv4 bundles (seed.json version 2, UDC entities): rebuilds each contract with the UDC reader from\nthe entities its keys name, checking owner, kind, codecs, chunk lists, digests and content hashes.\nThe output equals v3 hydration of the same records, byte for byte.";
#[derive(Args, Debug)]
pub struct HydrateArgs {
    /// Bundle directory (also supports extracted prefill archives)
    #[arg(value_name = "BUNDLE_DIR")]
    pub dir: PathBuf,
}
pub fn hydrate(args: &HydrateArgs) -> Result<()> {
    let stdout = std::io::stdout();
    let mut out = BufWriter::new(stdout.lock());
    if seed_version(&args.dir).unwrap_or(1) == 2 {
        let bundle = UdcBundle::open(&args.dir)?;
        let prefix = bundle.manifest.prefix.clone();
        let owner = bundle.manifest.owner;
        for key in bundle.contracts() {
            let contract = bundle.entity(key)?;
            let mut fetch = |k: &[u8; 32]| bundle.entity(k);
            let record = Reader::new(&prefix, Some(owner), &mut fetch)
                .hydrate(&contract, None)
                .with_context(|| format!("contract {}", arkiv_entity_processor::hx(key)))?;
            serde_json::to_writer(&mut out, &record)?;
            out.write_all(b"\n")?;
        }
        out.flush()?;
        return Ok(());
    }
    let bundle = Bundle::open(&args.dir)?;
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
