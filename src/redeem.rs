//! Redeem resolved Conditional Token positions on Polygon (CTF.redeemPositions).
//! Auto-claim every N minutes for all closed markets (from Data API /positions?user=...).

use anyhow::{Context, Result};
use ethers::contract::abigen;
use ethers::prelude::*;
use ethers::types::Address;
use reqwest::Client;
use serde::Deserialize;
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
/// Default gas price for redeem txs (gwei). Polygon often requires >= 25 gwei.
const DEFAULT_REDEEM_GAS_PRICE_GWEI: u64 = 30;

abigen!(
    ConditionalTokens,
    r#"[
        function redeemPositions(address collateralToken, bytes32 parentCollectionId, bytes32 conditionId, uint256[] indexSets) external
    ]"#
);

/// Data API: position item (GET https://data-api.polymarket.com/positions?user=...).
#[derive(Debug, Deserialize)]
struct DataApiPosition {
    #[serde(rename = "conditionId")]
    condition_id: Option<String>,
    #[serde(default)]
    redeemable: Option<bool>,
}

/// Data API base URL for positions (not CLOB).
const DATA_API_BASE: &str = "https://data-api.polymarket.com";

/// Fetch all redeemable condition IDs for a user from Polymarket Data API.
/// Uses GET {data_api}/positions?user={address}&redeemable=true so only claimable positions are returned.
pub async fn fetch_resolved_condition_ids_from_positions(
    http: &Client,
    _clob_host: &str,
    user_address: &str,
) -> Result<Vec<String>> {
    let base = std::env::var("POLYMARKET_DATA_API_URL").unwrap_or_else(|_| DATA_API_BASE.to_string());
    let base = base.trim_end_matches('/');
    let url = format!(
        "{}/positions?user={}&redeemable=true&limit=500",
        base,
        urlencoding::encode(user_address)
    );
    let positions: Vec<DataApiPosition> = http
        .get(&url)
        .send()
        .await
        .context("Data API positions request")?
        .error_for_status()
        .context("Data API positions error status")?
        .json()
        .await
        .context("Data API positions JSON")?;

    let mut condition_ids: Vec<String> = positions
        .into_iter()
        .filter(|p| p.redeemable.unwrap_or(false))
        .filter_map(|p| {
            p.condition_id
                .filter(|id| !id.trim().is_empty())
        })
        .collect();

    // Deduplicate by condition_id (user can have multiple positions per market).
    condition_ids.sort();
    condition_ids.dedup();
    Ok(condition_ids)
}

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

    let gas_price_gwei: u64 = std::env::var("REDEEM_GAS_PRICE_GWEI")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_REDEEM_GAS_PRICE_GWEI)
        .max(25);
    let gas_price = ethers::types::U256::from(gas_price_gwei) * ethers::types::U256::from(1_000_000_000u64);

    let parent_b32: [u8; 32] = PARENT_COLLECTION_ID;
    let condition_b32: [u8; 32] = condition_bytes;
    let index_sets: Vec<ethers::types::U256> = INDEX_SETS.iter().map(|&u| u.into()).collect();

    match contract
        .redeem_positions(collateral, parent_b32, condition_b32, index_sets)
        .gas_price(gas_price)
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
