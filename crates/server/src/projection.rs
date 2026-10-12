//! Preserve stored values, including null versus absent. No signature derivation
//! or live proxy policy is implied by this initial fixture-serving projection.
use crate::{api::ApiError, fields::MINIMAL};
use anyhow::{ensure, Result};
use serde_json::Value;

pub fn validate(record: &Value, chain: &str, address: &str) -> Result<()> {
    for field in MINIMAL {
        ensure!(record.get(*field).is_some(), ApiError::corrupt());
    }
    let stored_chain = record["chainId"]
        .as_u64()
        .or_else(|| record["chainId"].as_str()?.parse::<u64>().ok());
    ensure!(
        stored_chain == chain.parse::<u64>().ok(),
        ApiError::corrupt()
    );
    ensure!(
        record["address"]
            .as_str()
            .is_some_and(|v| v.eq_ignore_ascii_case(address)),
        ApiError::corrupt()
    );
    Ok(())
}
