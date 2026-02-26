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
//! Opcional: POLYGON_RPC_URL. Si no está definida, se usa un RPC público de Polygon
//! (p. ej. https://polygon-rpc.com o https://rpc.ankr.com/polygon como respaldo).

use anyhow::{Context, Result};
use polymarket_client_sdk::auth::Signer as SignerTrait;
use polymarket_client_sdk::{contract_config, derive_safe_wallet, POLYGON, PRIVATE_KEY_VAR};
use serde::Deserialize;
use std::str::FromStr;

const USDC_POLYGON: &str = "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174";
/// Variable de entorno para la URL RPC de Polygon. Si no está definida, se usan los fallbacks públicos.
const POLYGON_RPC_URL_VAR: &str = "POLYGON_RPC_URL";
/// RPC público de Polygon usado cuando POLYGON_RPC_URL no está definida.
const DEFAULT_POLYGON_RPC: &str = "https://polygon-rpc.com";
/// Respaldo si el RPC por defecto falla (misma red de seguridad que en el proyecto).
const FALLBACK_POLYGON_RPC: &str = "https://rpc.ankr.com/polygon";
const USDC_DECIMALS: u32 = 6;

/// balanceOf(address) selector
const SELECTOR_BALANCE: &str = "70a08231";
/// allowance(address,address) selector
const SELECTOR_ALLOWANCE: &str = "dd62ed3e";
/// isApprovedForAll(address,address) selector (ERC-1155 Conditional Tokens)
const SELECTOR_IS_APPROVED_FOR_ALL: &str = "e985e9c5";

#[derive(Debug, Deserialize)]
struct RpcResponse {
    result: Option<String>,
    #[serde(deserialize_with = "deserialize_rpc_error")]
    error: Option<RpcErrorKind>,
}

#[derive(Debug)]
enum RpcErrorKind {
    Object { message: String },
    String(String),
}

fn deserialize_rpc_error<'de, D>(deserializer: D) -> Result<Option<RpcErrorKind>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de::Error;
    let opt: Option<serde_json::Value> = Option::deserialize(deserializer)?;
    let Some(v) = opt else { return Ok(None) };
    Ok(Some(match v {
        serde_json::Value::String(s) => RpcErrorKind::String(s),
        serde_json::Value::Object(m) => {
            let message = m
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown RPC error")
                .to_string();
            RpcErrorKind::Object { message }
        }
        _ => return Err(Error::custom("RPC error must be string or object")),
    }))
}

fn address_to_hex_64(addr: &impl std::fmt::Display) -> String {
    let s = format!("{}", addr);
    let s = s.trim_start_matches("0x");
    let s = if s.len() >= 40 { &s[s.len() - 40..] } else { s };
    format!("{:0>64}", s.to_lowercase())
}

/// Devuelve la lista de URLs RPC a usar: si POLYGON_RPC_URL está definida, solo esa;
/// si no, los dos RPCs públicos en orden (red de seguridad).
fn polygon_rpc_urls() -> Vec<String> {
    if let Ok(url) = std::env::var(POLYGON_RPC_URL_VAR) {
        let s = url.trim().to_string();
        if !s.is_empty() {
            return vec![s];
        }
    }
    vec![
        DEFAULT_POLYGON_RPC.to_string(),
        FALLBACK_POLYGON_RPC.to_string(),
    ]
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
    let ctf_contract = format!("{:?}", config.conditional_tokens);

    let rpc_urls = polygon_rpc_urls();
    let client = reqwest::Client::new();

    println!("EOA (tu clave Phantom/MetaMask):     {:?}\n", eoa);
    println!("Polymarket trading wallet (Gnosis Safe): {:?}\n", safe_address);

    // --- Saldo y allowance de la EOA ---
    let data_balance_eoa = format!("0x{}{}", SELECTOR_BALANCE, eoa_hex);
    let balance_raw_eoa = eth_call(&client, &rpc_urls, USDC_POLYGON, &data_balance_eoa).await?;
    let balance_eoa = parse_hex_u256(&balance_raw_eoa)? as f64 / 10f64.powi(USDC_DECIMALS as i32);
    let data_allow_eoa = format!(
        "0x{}{}{}",
        SELECTOR_ALLOWANCE,
        eoa_hex,
        exchange_hex
    );
    let allow_raw_eoa = eth_call(&client, &rpc_urls, USDC_POLYGON, &data_allow_eoa).await?;
    let allowance_eoa = parse_hex_u256(&allow_raw_eoa)? as f64 / 10f64.powi(USDC_DECIMALS as i32);
    println!("  EOA  — USDC balance: {} USDC, USDC allowance (to exchange): {} USDC", balance_eoa, allowance_eoa);

    // --- Saldo y allowance de la Safe (Polymarket) ---
    let safe_addr = safe_address
        .as_ref()
        .context("derived Safe wallet address (Polymarket) no disponible")?;
    let safe_hex = address_to_hex_64(safe_addr);
    let data_balance_safe = format!("0x{}{}", SELECTOR_BALANCE, safe_hex);
    let balance_raw_safe = eth_call(&client, &rpc_urls, USDC_POLYGON, &data_balance_safe).await?;
    let balance_safe = parse_hex_u256(&balance_raw_safe)? as f64 / 10f64.powi(USDC_DECIMALS as i32);
    let data_allow_safe = format!(
        "0x{}{}{}",
        SELECTOR_ALLOWANCE,
        safe_hex,
        exchange_hex
    );
    let allow_raw_safe = eth_call(&client, &rpc_urls, USDC_POLYGON, &data_allow_safe).await?;
    let allowance_safe = parse_hex_u256(&allow_raw_safe)? as f64 / 10f64.powi(USDC_DECIMALS as i32);
    // CTF (Conditional Tokens ERC-1155): isApprovedForAll(Safe, exchange) — required for SELL (SL/TP)
    let data_ctf_safe = format!(
        "0x{}{}{}",
        SELECTOR_IS_APPROVED_FOR_ALL,
        safe_hex,
        exchange_hex
    );
    let ctf_approved_safe = eth_call(&client, &rpc_urls, ctf_contract.as_str(), &data_ctf_safe).await
        .map(|r| parse_ctf_approved(&r))
        .unwrap_or(Ok(false))
        .unwrap_or(false);
    println!(
        "  Safe — USDC balance: {} USDC, USDC allowance: {} USDC, CTF approved (sell): {}",
        balance_safe, allowance_safe, ctf_approved_safe
    );
    println!();

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
    rpc_urls: &[String],
    to: &str,
    data: &str,
) -> Result<String> {
    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "eth_call",
        "params": [{"to": to, "data": data}, "latest"],
        "id": 1
    });
    let mut last_err = None;
    for url in rpc_urls {
        let res = match client.post(url).json(&body).send().await {
            Ok(r) => r,
            Err(e) => {
                last_err = Some(anyhow::anyhow!("{}", e));
                continue;
            }
        };
        let rpc: RpcResponse = match res.json().await {
            Ok(r) => r,
            Err(e) => {
                last_err = Some(e.into());
                continue;
            }
        };
        if let Some(e) = rpc.error {
            let msg = match e {
                RpcErrorKind::Object { message } => message,
                RpcErrorKind::String(s) => s,
            };
            last_err = Some(anyhow::anyhow!("RPC error: {}", msg));
            continue;
        }
        if let Some(result) = rpc.result {
            return Ok(result.trim_start_matches("0x").to_string());
        }
        last_err = Some(anyhow::anyhow!("RPC result missing"));
    }
    Err(last_err
        .unwrap_or_else(|| anyhow::anyhow!("No RPC URLs to try"))
        .context("Todos los RPCs fallaron"))
}

/// Parse ERC-1155 isApprovedForAll return: 32-byte bool (true = 0x...01, false = 0x...00).
fn parse_ctf_approved(hex: &str) -> Result<bool> {
    let hex = hex.trim_start_matches("0x");
    let last_byte = if hex.len() >= 2 {
        &hex[hex.len().saturating_sub(2)..]
    } else {
        hex
    };
    let byte = u8::from_str_radix(last_byte, 16).unwrap_or(0);
    Ok(byte != 0)
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
