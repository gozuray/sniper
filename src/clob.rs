//! CLOB client: place/cancel orders. Dry-run implementation logs only; live uses EIP-712 signing + HMAC L2.

use crate::signing::{
    build_poly_hmac, parse_token_id, sign_order, EXCHANGE_ADDRESS_POLYGON,
    NEG_RISK_EXCHANGE_POLYGON,
};
use crate::types::SellOrderTimeInForce;
use anyhow::{Context, Result};
use ethers::signers::{LocalWallet, Signer};
use ethers::types::H160;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::str::FromStr;
use std::time::{Duration, UNIX_EPOCH};
use tracing::{info, warn};

const CONDITIONAL_BASE_DECIMALS: u32 = 6;
const CONDITIONAL_BASE_FACTOR: Decimal = dec!(1000000);

/// Order type for placement.
#[derive(Debug, Clone, Copy)]
pub enum OrderType {
    Gtc,
    Gtd,
    Fok,
    Fak,
}

/// Result of placing an order.
#[derive(Debug)]
pub struct PlaceOrderResult {
    pub order_id: Option<String>,
    pub success: bool,
    pub error_msg: Option<String>,
    /// Filled size in shares (from API takingAmount when matched). Use this for TP/SL so we sell 100% of what was actually bought.
    pub filled_size: Option<Decimal>,
    /// HTTP status from the order API (e.g. 400 when TP/SL fails with balance/allowance).
    pub http_status: Option<u16>,
}

/// Parameters for a limit order.
#[derive(Debug, Clone)]
pub struct LimitOrderParams {
    pub token_id: String,
    pub side: OrderSide,
    pub price: Decimal,
    pub size: Decimal,
    pub expiration_unix: Option<u64>,
    pub post_only: bool,
    pub fee_rate_bps: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderSide {
    Buy,
    Sell,
}

/// Result of cancelling orders (e.g. cancel-market-orders).
#[derive(Debug, Default)]
pub struct CancelOrdersResult {
    pub canceled: Vec<String>,
    pub not_canceled: std::collections::HashMap<String, String>,
}

/// Abstraction for CLOB order placement (dry-run or live).
#[async_trait::async_trait]
pub trait ClobClient: Send + Sync {
    async fn place_limit_order(
        &self,
        params: LimitOrderParams,
        order_type: OrderType,
    ) -> Result<PlaceOrderResult>;

    /// Cancel all open orders for a given outcome token. Use before placing SL/TP sell so any
    /// resting GTC order (e.g. TP) does not lock balance and cause "not enough balance" on SL.
    async fn cancel_orders_for_token(&self, _token_id: &str) -> Result<CancelOrdersResult> {
        Ok(CancelOrdersResult::default())
    }

    /// Fetch balance/allowance for conditional token (GET /balance-allowance?asset_type=CONDITIONAL&token_id=...&signature_type=...).
    /// Used when TP/SL returns 400 to debug balance/allowance.
    async fn get_balance_allowance(&self, _token_id: &str) -> Result<String> {
        Ok("(not available)".to_string())
    }

    /// Available balance for token (from balance-allowance endpoint). Returns None on error or parse failure.
    /// Used to compute sell_size = min(position_size_real, available) for TP/SL.
    async fn get_available_balance(&self, _token_id: &str) -> Result<Option<Decimal>> {
        Ok(None)
    }

    async fn place_sell_order(
        &self,
        token_id: &str,
        price: Decimal,
        size: Decimal,
        time_in_force: SellOrderTimeInForce,
    ) -> Result<PlaceOrderResult> {
        let order_type = match time_in_force {
            SellOrderTimeInForce::Gtc => OrderType::Gtc,
            SellOrderTimeInForce::Fok => OrderType::Fok,
            SellOrderTimeInForce::Fak => OrderType::Fak,
        };
        self.place_limit_order(
            LimitOrderParams {
                token_id: token_id.to_string(),
                side: OrderSide::Sell,
                price,
                size,
                expiration_unix: None,
                post_only: false,
                fee_rate_bps: None,
            },
            order_type,
        )
        .await
    }
}

/// Dry-run: log order and return success with fake order ID.
pub struct DryRunClob;

#[async_trait::async_trait]
impl ClobClient for DryRunClob {
    async fn place_limit_order(
        &self,
        params: LimitOrderParams,
        order_type: OrderType,
    ) -> Result<PlaceOrderResult> {
        let side_str = match params.side {
            OrderSide::Buy => "BUY",
            OrderSide::Sell => "SELL",
        };
        let type_str = match order_type {
            OrderType::Gtc => "GTC",
            OrderType::Gtd => "GTD",
            OrderType::Fok => "FOK",
            OrderType::Fak => "FAK",
        };
        info!(
            "[DryRun] place_limit_order {} {} @ {} size={} type={} token_id={}",
            side_str,
            type_str,
            params.price,
            params.size,
            type_str,
            &params.token_id[..params.token_id.len().min(18)]
        );
        Ok(PlaceOrderResult {
            order_id: Some("dry-run".to_string()),
            success: true,
            error_msg: None,
            filled_size: Some(params.size),
            http_status: None,
        })
    }
}

/// Live CLOB client: EIP-712 order signing + HMAC L2 auth.
pub struct LiveClob {
    clob_host: String,
    wallet: LocalWallet,
    api_key: String,
    api_secret: String,
    api_passphrase: String,
    chain_id: u64,
    funder: H160,
    signature_type: u8,
    neg_risk: bool,
    client: reqwest::Client,
}

impl LiveClob {
    pub fn from_env() -> Result<Self> {
        let clob_host = std::env::var("POLYMARKET_CLOB_HOST")
            .or_else(|_| std::env::var("POLYMARKET_CLOB_URL"))
            .unwrap_or_else(|_| "https://clob.polymarket.com".to_string());
        let pk = std::env::var("PRIVATE_KEY")
            .or_else(|_| std::env::var("POLYMARKET_PRIVATE_KEY"))
            .context("PRIVATE_KEY or POLYMARKET_PRIVATE_KEY required for live CLOB")?;
        let wallet = pk
            .trim()
            .strip_prefix("0x")
            .unwrap_or(pk.trim())
            .parse::<LocalWallet>()
            .context("Invalid PRIVATE_KEY")?;
        let api_key = std::env::var("API_KEY").context("API_KEY required")?;
        let api_secret = std::env::var("SECRET")
            .or_else(|_| std::env::var("API_SECRET"))
            .context("SECRET or API_SECRET required")?;
        let api_passphrase = std::env::var("PASSPHRASE")
            .or_else(|_| std::env::var("API_PASSPHRASE"))
            .context("PASSPHRASE required")?;
        let chain_id: u64 = std::env::var("POLYMARKET_CHAIN_ID")
            .unwrap_or_else(|_| "137".to_string())
            .parse()
            .unwrap_or(137);
        let funder_str = std::env::var("FUNDER_ADDRESS").unwrap_or_else(|_| {
            format!("{:?}", wallet.address())
                .trim_matches('"')
                .to_string()
        });
        let funder = funder_str
            .trim()
            .strip_prefix("0x")
            .unwrap_or(funder_str.trim())
            .parse::<H160>()
            .context("Invalid FUNDER_ADDRESS")?;
        let signature_type: u8 = std::env::var("SIGNATURE_TYPE")
            .unwrap_or_else(|_| "2".to_string())
            .parse()
            .unwrap_or(2);
        let neg_risk = std::env::var("MM_NEG_RISK")
            .map(|v| v.to_lowercase() == "true" || v == "1")
            .unwrap_or(false);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()?;
        Ok(Self {
            clob_host: clob_host.trim_end_matches('/').to_string(),
            wallet,
            api_key,
            api_secret,
            api_passphrase,
            chain_id,
            funder,
            signature_type,
            neg_risk,
            client,
        })
    }

    fn maker_taker_amounts_6dec(
        &self,
        side: OrderSide,
        price: &Decimal,
        size: &Decimal,
    ) -> Result<(ethers::types::U256, ethers::types::U256)> {
        let six = dec!(1000000);
        let (maker_human, taker_human) = match side {
            OrderSide::Buy => (size * price, *size),
            OrderSide::Sell => (*size, size * price),
        };
        let maker = (maker_human * six).trunc();
        let taker = (taker_human * six).trunc();
        let maker_u =
            ethers::types::U256::from_dec_str(&maker.to_string()).context("maker amount")?;
        let taker_u =
            ethers::types::U256::from_dec_str(&taker.to_string()).context("taker amount")?;
        Ok((maker_u, taker_u))
    }

    async fn post_order(
        &self,
        order_type: &str,
        order_json: &serde_json::Value,
        side: OrderSide,
        price: Option<Decimal>,
    ) -> Result<PlaceOrderResult> {
        let path = "/order";
        let body = serde_json::json!({
            "order": order_json,
            "owner": self.api_key,
            "orderType": order_type,
            "deferExec": false
        });
        let body_str = body.to_string();
        let timestamp = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let sig = build_poly_hmac(&self.api_secret, timestamp, "POST", path, Some(&body_str))?;
        let url = format!("{}{}", self.clob_host, path);
        let signer_addr = format!("{:?}", self.wallet.address())
            .trim_matches('"')
            .to_string();
        let res = self
            .client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("POLY_API_KEY", &self.api_key)
            .header("POLY_ADDRESS", &signer_addr)
            .header("POLY_SIGNATURE", &sig)
            .header("POLY_TIMESTAMP", timestamp.to_string())
            .header("POLY_PASSPHRASE", &self.api_passphrase)
            .body(body_str)
            .send()
            .await?;
        let status = res.status();
        let text = res.text().await.unwrap_or_default();
        let json: serde_json::Value =
            serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
        let success = json
            .get("success")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let order_id = json
            .get("orderID")
            .and_then(|v| v.as_str())
            .map(String::from);
        let error_msg = json
            .get("errorMsg")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(String::from);
        if !status.is_success() {
            return Ok(PlaceOrderResult {
                order_id,
                success: false,
                error_msg: Some(format!(
                    "HTTP {}: {}",
                    status,
                    text.chars().take(200).collect::<String>()
                )),
                filled_size: None,
                http_status: Some(status.as_u16()),
            });
        }
        // Parse filled size from API: takingAmount is in 6 decimals (string or number). For BUY = shares filled; for SELL = (size*price) so size = takingAmount/1e6/price.
        let taker_6dec_opt = json.get("takingAmount").and_then(|v| {
            v.as_str()
                .and_then(|s| Decimal::from_str(s).ok())
                .or_else(|| v.as_i64().map(|n| Decimal::from(n)))
                .or_else(|| v.as_u64().map(|n| Decimal::from(n)))
        });
        let filled_size = taker_6dec_opt.map(|taker_6dec| {
            let human = taker_6dec / dec!(1000000);
            match (side, price) {
                (OrderSide::Buy, _) => human,
                (OrderSide::Sell, Some(p)) if !p.is_zero() => human / p,
                _ => human,
            }
        });
        Ok(PlaceOrderResult {
            order_id,
            success,
            error_msg,
            filled_size,
            http_status: Some(status.as_u16()),
        })
    }

    /// GET /balance-allowance with HMAC auth. L2 HMAC is over path only (no query), per py-clob-client.
    async fn get_balance_allowance_inner(&self, token_id: &str) -> Result<String> {
        let path_for_sig = "/balance-allowance";
        let timestamp = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let sig = build_poly_hmac(&self.api_secret, timestamp, "GET", path_for_sig, None)?;
        let path_with_query = format!(
            "{}?asset_type=CONDITIONAL&token_id={}&signature_type={}",
            path_for_sig,
            urlencoding::encode(token_id),
            self.signature_type
        );
        let url = format!("{}{}", self.clob_host, path_with_query);
        let signer_addr = format!("{:?}", self.wallet.address())
            .trim_matches('"')
            .to_string();
        let res = self
            .client
            .get(&url)
            .header("POLY_API_KEY", &self.api_key)
            .header("POLY_ADDRESS", &signer_addr)
            .header("POLY_SIGNATURE", &sig)
            .header("POLY_TIMESTAMP", timestamp.to_string())
            .header("POLY_PASSPHRASE", &self.api_passphrase)
            .send()
            .await?;
        let status = res.status();
        let text = res.text().await.unwrap_or_default();
        if !status.is_success() {
            anyhow::bail!(
                "balance-allowance HTTP {}: {}",
                status,
                text.chars().take(200).collect::<String>()
            );
        }
        Ok(text)
    }

    /// Parse balance from balance-allowance JSON and normalize to shares.
    /// Conditional balances are returned in base units (1e6).
    fn parse_balance_from_response(text: &str) -> Option<Decimal> {
        let json: serde_json::Value = serde_json::from_str(text).ok()?;
        let balance = json.get("balance")?;
        let s = balance
            .as_str()
            .map(String::from)
            .or_else(|| balance.as_f64().map(|f| f.to_string()))
            .or_else(|| balance.as_i64().map(|i| i.to_string()))
            .or_else(|| balance.as_u64().map(|u| u.to_string()))?;
        let raw = Decimal::from_str(s.trim())
            .ok()
            .filter(|d| *d >= Decimal::ZERO)?;
        let shares = if CONDITIONAL_BASE_DECIMALS == 6 {
            raw / CONDITIONAL_BASE_FACTOR
        } else {
            let divisor = Decimal::from(10u64.pow(CONDITIONAL_BASE_DECIMALS));
            raw / divisor
        };
        Some(shares)
    }
}

#[async_trait::async_trait]
impl ClobClient for LiveClob {
    async fn place_limit_order(
        &self,
        params: LimitOrderParams,
        order_type: OrderType,
    ) -> Result<PlaceOrderResult> {
        let (maker_amount, taker_amount) =
            self.maker_taker_amounts_6dec(params.side, &params.price, &params.size)?;
        let token_id = parse_token_id(&params.token_id)?;
        let signer_addr = format!("0x{:x}", self.wallet.address());
        let taker = H160::from_str("0x0000000000000000000000000000000000000000").unwrap();
        // For non-GTD orders use expiration 0 in both signature and API (API parses as big.Int).
        let (expiration_for_sig, expiration_for_api) = match order_type {
            OrderType::Gtd => {
                let e = params.expiration_unix.unwrap_or(0);
                (e, serde_json::Value::String(e.to_string()))
            }
            _ => (0u64, serde_json::Value::String("0".to_string())),
        };
        let expiration = expiration_for_sig;
        let nonce = 0u64;
        let fee_rate_bps = params.fee_rate_bps.unwrap_or(1000);
        let side = match params.side {
            OrderSide::Buy => 0u8,
            OrderSide::Sell => 1u8,
        };
        let salt = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let exchange_addr = if self.neg_risk {
            NEG_RISK_EXCHANGE_POLYGON
        } else {
            EXCHANGE_ADDRESS_POLYGON
        };
        let verifying = H160::from_str(exchange_addr).unwrap();
        let signature = sign_order(
            &self.wallet,
            self.chain_id,
            verifying,
            salt,
            self.funder,
            self.wallet.address(),
            taker,
            token_id,
            maker_amount,
            taker_amount,
            expiration,
            nonce,
            fee_rate_bps,
            side,
            self.signature_type,
        )
        .await?;
        let order_json = serde_json::json!({
            "maker": format!("0x{:x}", self.funder),
            "signer": &signer_addr,
            "taker": "0x0000000000000000000000000000000000000000",
            "tokenId": params.token_id,
            "makerAmount": maker_amount.to_string(),
            "takerAmount": taker_amount.to_string(),
            "side": if params.side == OrderSide::Buy { "BUY" } else { "SELL" },
            "expiration": expiration_for_api,
            "nonce": nonce.to_string(),
            "feeRateBps": fee_rate_bps.to_string(),
            "signature": signature,
            "salt": salt,
            "signatureType": self.signature_type
        });
        let order_type_str = match order_type {
            OrderType::Gtc => "GTC",
            OrderType::Gtd => "GTD",
            OrderType::Fok => "FOK",
            OrderType::Fak => "FAK",
        };
        let result = self
            .post_order(order_type_str, &order_json, params.side, Some(params.price))
            .await?;
        if result.success {
            info!("[LiveClob] order placed order_id={:?}", result.order_id);
        } else if let Some(ref msg) = result.error_msg {
            info!("[LiveClob] order failed: {}", msg);
        }
        Ok(result)
    }

    async fn cancel_orders_for_token(&self, token_id: &str) -> Result<CancelOrdersResult> {
        let path = "/cancel-market-orders";
        let body = serde_json::json!({ "asset_id": token_id });
        let body_str = body.to_string();
        let timestamp = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let sig = build_poly_hmac(&self.api_secret, timestamp, "DELETE", path, Some(&body_str))?;
        let url = format!("{}{}", self.clob_host, path);
        let signer_addr = format!("{:?}", self.wallet.address())
            .trim_matches('"')
            .to_string();
        let res = self
            .client
            .request(reqwest::Method::DELETE, &url)
            .header("Content-Type", "application/json")
            .header("POLY_API_KEY", &self.api_key)
            .header("POLY_ADDRESS", &signer_addr)
            .header("POLY_SIGNATURE", &sig)
            .header("POLY_TIMESTAMP", timestamp.to_string())
            .header("POLY_PASSPHRASE", &self.api_passphrase)
            .body(body_str)
            .send()
            .await?;
        let text = res.text().await.unwrap_or_default();
        let json: serde_json::Value =
            serde_json::from_str(&text).unwrap_or(serde_json::Value::Null);
        let canceled: Vec<String> = json
            .get("canceled")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let not_canceled: std::collections::HashMap<String, String> = json
            .get("not_canceled")
            .and_then(|v| v.as_object())
            .map(|obj| {
                obj.iter()
                    .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                    .collect()
            })
            .unwrap_or_default();
        if !canceled.is_empty() {
            info!(
                "[LiveClob] canceled {} open order(s) for token to free balance",
                canceled.len()
            );
        }
        if !not_canceled.is_empty() {
            warn!(
                "[LiveClob] {} order(s) could not be canceled (balance may stay locked): {:?}",
                not_canceled.len(),
                not_canceled
            );
        }
        Ok(CancelOrdersResult {
            canceled,
            not_canceled,
        })
    }

    async fn get_balance_allowance(&self, token_id: &str) -> Result<String> {
        self.get_balance_allowance_inner(token_id).await
    }

    async fn get_available_balance(&self, token_id: &str) -> Result<Option<Decimal>> {
        let text = self.get_balance_allowance_inner(token_id).await.ok();
        Ok(text.as_deref().and_then(Self::parse_balance_from_response))
    }
}

/// Build a CLOB client from config: DryRun if dry_run, else Live (which currently fails on place).
pub fn create_clob_client(dry_run: bool) -> Result<Box<dyn ClobClient>> {
    if dry_run {
        Ok(Box::new(DryRunClob))
    } else {
        Ok(Box::new(LiveClob::from_env()?))
    }
}
