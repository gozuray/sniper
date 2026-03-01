//! Redeem resolved Conditional Token positions on Polygon (CTF.redeemPositions).
//! Runs every 10 minutes for resolved markets (e.g. past 5m interval markets).

use anyhow::{Context, Result};
use ethers::contract::abigen;
use ethers::prelude::*;
use ethers::types::Address;
use std::str::FromStr;
use tracing::{info, warn};

/// Polygon mainnet: Conditional Tokens (CTF).
const CTF_ADDRESS: &str = "0x4D97DCd97eC945f40cF65F87097ACe5EA0476045";
/// Polygon mainnet: USDC.e (bridged).
const USDC_E_ADDRESS: &str = "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174";
/// Parent collection ID for Polymarket (no parent) = 32 zero bytes.
const PARENT_COLLECTION_ID: [u8; 32] = [0u8; 32];
/// Binary markets: redeem both outcome index sets (only winning pays out).
const INDEX_SETS: [u64; 2] = [1, 2];

abigen!(
    ConditionalTokens,
    r#"[
        function redeemPositions(address collateralToken, bytes32 parentCollectionId, bytes32 conditionId, uint256[] indexSets) external
    ]"#
);

/// Parse condition_id (hex with or without 0x) into bytes32.
fn condition_id_to_bytes32(condition_id: &str) -> Result<[u8; 32]> {
    let s = condition_id.trim().trim_start_matches("0x");
    let decoded = hex::decode(s).context("condition_id hex decode")?;
    if decoded.len() != 32 {
        anyhow::bail!("condition_id must be 32 bytes, got {}", decoded.len());
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&decoded);
    Ok(out)
}

/// Call CTF.redeemPositions for one condition. Skips (no error) if condition not resolved yet.
pub async fn redeem_positions(
    wallet: &LocalWallet,
    rpc_url: &str,
    condition_id: &str,
) -> Result<bool> {
    let condition_bytes = condition_id_to_bytes32(condition_id)?;
    let provider = Provider::<Http>::try_from(rpc_url)
        .context("Polygon RPC provider")?;
    let chain_id = provider.get_chainid().await?.as_u64();
    let wallet = wallet.clone().with_chain_id(chain_id);
    let client = SignerMiddleware::new(provider, wallet);

    let ctf_addr = Address::from_str(CTF_ADDRESS).context("CTF address")?;
    let collateral = Address::from_str(USDC_E_ADDRESS).context("USDC.e address")?;
    let contract = ConditionalTokens::new(ctf_addr, client.into());

    let parent_b32: [u8; 32] = PARENT_COLLECTION_ID;
    let condition_b32: [u8; 32] = condition_bytes;
    let index_sets: Vec<ethers::types::U256> = INDEX_SETS.iter().map(|&u| u.into()).collect();

    match contract
        .redeem_positions(collateral, parent_b32, condition_b32, index_sets)
        .send()
        .await
    {
        Ok(pending) => {
            let hash = pending.tx_hash();
            info!(
                "[Redeem] redeemPositions tx submitted condition_id={}.. tx_hash={:?}",
                &condition_id[..condition_id.len().min(18)],
                hash
            );
            let success = if let Ok(Some(receipt)) = pending.await {
                let s = receipt.status.map(|s| s.as_u64() == 1).unwrap_or(false);
                if s {
                    info!("[Redeem] tx confirmed block={:?}", receipt.block_number);
                } else {
                    warn!("[Redeem] tx reverted");
                }
                s
            } else {
                false
            };
            Ok(success)
        }
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("result for condition not received yet") || msg.contains("not received yet") {
                return Ok(false);
            }
            Err(e.into())
        }
    }
}
