//! Redeem resolved Conditional Token positions on Polygon (CTF.redeemPositions).
//! Auto-claim every N minutes for all closed markets (from Data API /positions?user=...).
//! When FUNDER_ADDRESS (proxy/Safe) is set, executes redeem via Gnosis Safe so the proxy's positions are claimed.

use anyhow::{Context, Result};
use ethers::contract::abigen;
use ethers::prelude::*;
use ethers::types::{Address, Bytes};
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
/// Default gas price for redeem txs (gwei). Must be >= block base fee (Polygon can be 100+ gwei when busy).
const DEFAULT_REDEEM_GAS_PRICE_GWEI: u64 = 150;
/// Operation.Call for Safe execTransaction.
const SAFE_OP_CALL: u8 = 0;

abigen!(
    ConditionalTokens,
    r#"[
        function redeemPositions(address collateralToken, bytes32 parentCollectionId, bytes32 conditionId, uint256[] indexSets) external
    ]"#
);

abigen!(
    GnosisSafe,
    r#"[
        function nonce() external view returns (uint256)
        function getTransactionHash(address to, uint256 value, bytes data, uint8 operation, uint256 safeTxGas, uint256 baseGas, uint256 gasPrice, address gasToken, address refundReceiver, uint256 _nonce) external view returns (bytes32)
        function approveHash(bytes32 hashToApprove) external
        function execTransaction(address to, uint256 value, bytes data, uint8 operation, uint256 safeTxGas, uint256 baseGas, uint256 gasPrice, address gasToken, address refundReceiver, bytes signatures) external payable returns (bool success)
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

/// Build calldata for CTF.redeemPositions(collateral, parent, conditionId, indexSets).
fn encode_redeem_calldata(
    collateral: Address,
    parent_b32: [u8; 32],
    condition_b32: [u8; 32],
    index_sets: &[ethers::types::U256],
) -> Result<Bytes> {
    use ethers::abi::{encode, Token};
    let selector = ethers::utils::id("redeemPositions(address,bytes32,bytes32,uint256[])");
    let tokens = vec![
        Token::Address(collateral),
        Token::FixedBytes(parent_b32.to_vec()),
        Token::FixedBytes(condition_b32.to_vec()),
        Token::Array(
            index_sets
                .iter()
                .map(|u| Token::Uint(*u))
                .collect::<Vec<_>>(),
        ),
    ];
    let encoded = encode(&tokens);
    let calldata: Vec<u8> = selector[..4]
        .iter()
        .copied()
        .chain(encoded.into_iter())
        .collect();
    Ok(Bytes::from(calldata))
}

/// Redeem one condition via Gnosis Safe (proxy). The Safe holds the positions; EOA signs and calls execTransaction.
async fn redeem_positions_via_safe(
    wallet: &LocalWallet,
    rpc_url: &str,
    safe_address: Address,
    condition_id: &str,
    gas_price: ethers::types::U256,
) -> Result<bool> {
    let condition_bytes = condition_id_to_bytes32(condition_id)?;
    let provider = Provider::<Http>::try_from(rpc_url).context("Polygon RPC provider")?;
    let chain_id = provider.get_chainid().await?.as_u64();
    let wallet = wallet.clone().with_chain_id(chain_id);
    let client = SignerMiddleware::new(provider.clone(), wallet.clone());

    let ctf_addr = Address::from_str(CTF_ADDRESS).context("CTF address")?;
    let collateral = Address::from_str(USDC_E_ADDRESS).context("USDC.e address")?;
    let parent_b32: [u8; 32] = PARENT_COLLECTION_ID;
    let condition_b32: [u8; 32] = condition_bytes;
    let index_sets: Vec<ethers::types::U256> = INDEX_SETS.iter().map(|&u| u.into()).collect();

    let calldata = encode_redeem_calldata(collateral, parent_b32, condition_b32, &index_sets)?;

    let safe = GnosisSafe::new(safe_address, client.into());
    let nonce = safe.nonce().call().await.context("Safe nonce")?;
    let zero = Address::zero();
    let safe_tx_gas = U256::zero();
    let base_gas = U256::zero();
    let safe_gas_price = U256::zero();
    let tx_hash = safe
        .get_transaction_hash(
            ctf_addr,
            U256::zero(),
            calldata.clone(),
            SAFE_OP_CALL,
            safe_tx_gas,
            base_gas,
            safe_gas_price,
            zero,
            zero,
            nonce,
        )
        .call()
        .await
        .context("Safe getTransactionHash")?;

    // Polymarket Safe: 1) approveHash(txHash), 2) execTransaction(CTF, 0, redeem_calldata, CALL, 0, 0, 0, 0, 0, signatures). Both txs from EOA (PRIVATE_KEY).
    let provider2 = Provider::<Http>::try_from(rpc_url).context("Polygon RPC provider 2")?;
    let client2 = SignerMiddleware::new(provider2, wallet.clone());
    let safe_signer = GnosisSafe::new(safe_address, client2.into());

    // Step 1: approveHash(txHash)
    let approve_call = safe_signer.approve_hash(tx_hash);
    let approve_with_gas = approve_call.gas_price(gas_price);
    let approve_pending = approve_with_gas
        .send()
        .await
        .context("Safe approveHash send")?;
    info!(
        "[Redeem] Safe approveHash() tx submitted condition_id={}.. tx_hash={:?}",
        &condition_id[..condition_id.len().min(18)],
        approve_pending.tx_hash()
    );
    let approve_receipt = approve_pending
        .await
        .context("Safe approveHash await")?
        .context("Safe approveHash no receipt")?;
    if !approve_receipt.status.map(|s| s.as_u64() == 1).unwrap_or(false) {
        anyhow::bail!("Safe approveHash tx reverted");
    }
    info!("[Redeem] Safe approveHash() confirmed block={:?}", approve_receipt.block_number);

    // Step 2: execTransaction(CTF, 0, calldata, CALL, 0, 0, 0, address(0), address(0), signatures)
    let owner = wallet.address();
    let mut r_b = [0u8; 32];
    r_b[12..32].copy_from_slice(owner.as_bytes());
    let s_b = [0u8; 32];
    let v = 1u8; // approved hash signature type
    let signature_bytes: Vec<u8> = r_b
        .iter()
        .chain(s_b.iter())
        .chain(std::iter::once(&v))
        .copied()
        .collect();

    let exec_call = safe_signer.exec_transaction(
        ctf_addr,
        U256::zero(),
        calldata,
        SAFE_OP_CALL,
        U256::zero(), // safeTxGas = 0
        U256::zero(), // baseGas = 0
        U256::zero(), // gasPrice = 0 (Safe internal)
        zero,
        zero,
        signature_bytes.into(),
    );
    let with_gas = exec_call.gas_price(gas_price);
    info!(
        "[Redeem] Safe execTransaction() sending condition_id={}..",
        &condition_id[..condition_id.len().min(18)]
    );
    let pending = with_gas
        .send()
        .await
        .context("Safe execTransaction send")?;
    let hash = pending.tx_hash();
    info!(
        "[Redeem] Safe execTransaction() tx submitted condition_id={}.. tx_hash={:?}",
        &condition_id[..condition_id.len().min(18)],
        hash
    );
    let success = if let Ok(Some(receipt)) = pending.await {
        let s = receipt.status.map(|s| s.as_u64() == 1).unwrap_or(false);
        if s {
            info!("[Redeem] Safe exec tx confirmed block={:?}", receipt.block_number);
        } else {
            warn!("[Redeem] Safe exec tx reverted");
        }
        s
    } else {
        false
    };
    Ok(success)
}

/// Call CTF.redeemPositions for one condition. Skips (no error) if condition not resolved yet.
/// If funder_address is Some and different from wallet.address(), uses Safe (proxy) path so the proxy's positions are redeemed.
pub async fn redeem_positions(
    wallet: &LocalWallet,
    rpc_url: &str,
    condition_id: &str,
    funder_address: Option<Address>,
) -> Result<bool> {
    let gas_price_gwei: u64 = std::env::var("REDEEM_GAS_PRICE_GWEI")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_REDEEM_GAS_PRICE_GWEI)
        .max(25);
    let gas_price_gwei = if let Some(max_gwei) = std::env::var("REDEEM_GAS_PRICE_MAX_GWEI")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
    {
        gas_price_gwei.min(max_gwei)
    } else {
        gas_price_gwei
    };
    let gas_price = ethers::types::U256::from(gas_price_gwei) * ethers::types::U256::from(1_000_000_000u64);

    // Redeem always uses the wallet from PRIVATE_KEY. If that key's address equals FUNDER_ADDRESS,
    // the proxy is an EOA (same account) → direct CTF.redeemPositions. Otherwise FUNDER_ADDRESS
    // is treated as a Safe contract → approveHash + execTransaction as owner.
    let wallet_addr = wallet.address();
    let use_safe = funder_address
        .map(|f| f != wallet_addr)
        .unwrap_or(false);

    if use_safe {
        let safe_addr = funder_address.unwrap();
        return redeem_positions_via_safe(wallet, rpc_url, safe_addr, condition_id, gas_price).await;
    }

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
