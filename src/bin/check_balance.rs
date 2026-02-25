//! Consulta balance USDC y allowance (solo lectura, no gasta gas ni envía transacciones).
//!
//! Si conectaste Polymarket con una wallet de navegador (Phantom, MetaMask, etc.),
//! Polymarket usa una "wallet derivada" (Gnosis Safe). Este binario muestra tanto
//! tu EOA como la dirección Safe (la que ve Polymarket) y el saldo/allowance de cada una.
//!
//! Uso:
//!   cargo run --bin check_balance
//!
//! Requiere POLYMARKET_PRIVATE_KEY en .env o en el entorno.
//! Opcional: POLYGON_RPC_URL (por defecto https://polygon-rpc.com)

use anyhow::{Context, Result};
use polymarket_client_sdk::auth::Signer as SignerTrait;
use polymarket_client_sdk::{contract_config, derive_safe_wallet, POLYGON, PRIVATE_KEY_VAR};
use serde::Deserialize;
use std::str::FromStr;

const USDC_POLYGON: &str = "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174";
const DEFAULT_RPC: &str = "https://polygon-rpc.com";
const USDC_DECIMALS: u32 = 6;

/// balanceOf(address) selector
const SELECTOR_BALANCE: &str = "70a08231";
/// allowance(address,address) selector
const SELECTOR_ALLOWANCE: &str = "dd62ed3e";

#[derive(Debug, Deserialize)]
struct RpcResponse {
    result: Option<String>,
    error: Option<RpcError>,
}

#[derive(Debug, Deserialize)]
struct RpcError {
    message: String,
}

fn address_to_hex_64(addr: &impl std::fmt::Display) -> String {
    let s = format!("{}", addr);
    let s = s.trim_start_matches("0x");
    let s = if s.len() >= 40 { &s[s.len() - 40..] } else { s };
    format!("{:0>64}", s.to_lowercase())
}

async fn main_impl() -> Result<()> {
    let _ = dotenvy::dotenv();

    let private_key =
        std::env::var(PRIVATE_KEY_VAR).context("POLYMARKET_PRIVATE_KEY no definida (usa .env o export)")?;
    let signer = polymarket_client_sdk::auth::LocalSigner::from_str(&private_key)?
        .with_chain_id(Some(POLYGON));

    let eoa = signer.address();
    let safe_address = derive_safe_wallet(eoa, POLYGON);
    let eoa_hex = address_to_hex_64(&eoa);

    let config = contract_config(POLYGON, false)
        .context("contract_config(POLYGON) no disponible en este build del SDK")?;
    let exchange_hex = address_to_hex_64(&config.exchange);

    let rpc_url = std::env::var("POLYGON_RPC_URL").unwrap_or_else(|_| DEFAULT_RPC.to_string());
    let client = reqwest::Client::new();

    println!("EOA (tu clave Phantom/MetaMask):     {:?}\n", eoa);
    println!("Polymarket trading wallet (Gnosis Safe): {:?}\n", safe_address);

    // --- Saldo y allowance de la EOA ---
    let data_balance_eoa = format!("0x{}{}", SELECTOR_BALANCE, eoa_hex);
    let balance_raw_eoa = eth_call(&client, &rpc_url, USDC_POLYGON, &data_balance_eoa).await?;
    let balance_eoa = parse_hex_u256(&balance_raw_eoa)? as f64 / 10f64.powi(USDC_DECIMALS as i32);
    let data_allow_eoa = format!(
        "0x{}{}{}",
        SELECTOR_ALLOWANCE,
        eoa_hex,
        exchange_hex
    );
    let allow_raw_eoa = eth_call(&client, &rpc_url, USDC_POLYGON, &data_allow_eoa).await?;
    let allowance_eoa = parse_hex_u256(&allow_raw_eoa)? as f64 / 10f64.powi(USDC_DECIMALS as i32);
    println!("  EOA  — USDC balance: {} USDC, allowance (CTF): {} USDC", balance_eoa, allowance_eoa);

    // --- Saldo y allowance de la Safe (Polymarket) ---
    let safe_addr = safe_address
        .as_ref()
        .context("derived Safe wallet address (Polymarket) no disponible")?;
    let safe_hex = address_to_hex_64(safe_addr);
    let data_balance_safe = format!("0x{}{}", SELECTOR_BALANCE, safe_hex);
    let balance_raw_safe = eth_call(&client, &rpc_url, USDC_POLYGON, &data_balance_safe).await?;
    let balance_safe = parse_hex_u256(&balance_raw_safe)? as f64 / 10f64.powi(USDC_DECIMALS as i32);
    let data_allow_safe = format!(
        "0x{}{}{}",
        SELECTOR_ALLOWANCE,
        safe_hex,
        exchange_hex
    );
    let allow_raw_safe = eth_call(&client, &rpc_url, USDC_POLYGON, &data_allow_safe).await?;
    let allowance_safe = parse_hex_u256(&allow_raw_safe)? as f64 / 10f64.powi(USDC_DECIMALS as i32);
    println!("  Safe — USDC balance: {} USDC, allowance (CTF): {} USDC\n", balance_safe, allowance_safe);

    if balance_safe > 0.0 {
        println!("✓ El bot usa la wallet Safe (Polymarket). Saldo disponible para trading: {} USDC.", balance_safe);
    } else if balance_eoa > 0.0 {
        println!("⚠️  Tu EOA tiene USDC pero la Safe (lo que ve Polymarket) tiene 0.");
        println!("   Deposita desde la web de Polymarket para que el saldo aparezca en la Safe.");
    } else {
        println!("⚠️  Ambas direcciones tienen 0 USDC en Polygon. Deposita USDC en Polymarket.");
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    main_impl().await
}

async fn eth_call(
    client: &reqwest::Client,
    rpc_url: &str,
    to: &str,
    data: &str,
) -> Result<String> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "eth_call",
        "params": [{"to": to, "data": data}, "latest"],
        "id": 1
    });
    let res = client
        .post(rpc_url)
        .json(&body)
        .send()
        .await
        .context("RPC request failed")?;
    let rpc: RpcResponse = res.json().await.context("RPC response parse")?;
    if let Some(e) = rpc.error {
        anyhow::bail!("RPC error: {}", e.message);
    }
    rpc.result
        .context("RPC result missing")
        .map(|s| s.trim_start_matches("0x").to_string())
}

fn parse_hex_u256(hex: &str) -> Result<u64> {
    let hex = hex.trim_start_matches("0x");
    if hex.len() > 16 {
        // u64 son 8 bytes = 16 hex; la respuesta son 32 bytes; tomamos los últimos 16
        let start = hex.len().saturating_sub(16);
        u64::from_str_radix(&hex[start..], 16).context("parse balance/allowance hex")
    } else {
        u64::from_str_radix(hex, 16).context("parse balance/allowance hex")
    }
}
