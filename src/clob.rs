//! CLOB client: place/cancel orders. Dry-run implementation logs only; live requires EIP-712 signing.

use crate::types::SellOrderTimeInForce;
use anyhow::{Context, Result};
use rust_decimal::Decimal;
use std::time::Duration;
use tracing::info;

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

/// Abstraction for CLOB order placement (dry-run or live).
#[async_trait::async_trait]
pub trait ClobClient: Send + Sync {
    async fn place_limit_order(
        &self,
        params: LimitOrderParams,
        order_type: OrderType,
    ) -> Result<PlaceOrderResult>;

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
        })
    }
}

/// Live CLOB client (HMAC auth + EIP-712 signing). Placeholder: returns error for now.
/// To enable live orders: implement EIP-712 order signing (see Polymarket docs) and POST /order.
pub struct LiveClob {
    _clob_host: String,
    _private_key: String,
    _api_key: String,
    _api_secret: String,
    _api_passphrase: String,
    chain_id: u64,
    _client: reqwest::Client,
}

impl LiveClob {
    pub fn from_env() -> Result<Self> {
        let clob_host = std::env::var("POLYMARKET_CLOB_HOST")
            .unwrap_or_else(|_| "https://clob.polymarket.com".to_string());
        let private_key = std::env::var("PRIVATE_KEY").context("PRIVATE_KEY required for live CLOB")?;
        let api_key = std::env::var("API_KEY").context("API_KEY required")?;
        let api_secret = std::env::var("SECRET").or_else(|_| std::env::var("API_SECRET")).context("SECRET or API_SECRET required")?;
        let api_passphrase = std::env::var("PASSPHRASE").or_else(|_| std::env::var("API_PASSPHRASE")).context("PASSPHRASE required")?;
        let chain_id: u64 = std::env::var("POLYMARKET_CHAIN_ID")
            .unwrap_or_else(|_| "137".to_string())
            .parse()
            .unwrap_or(137);
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .build()?;
        Ok(Self {
            _clob_host: clob_host.trim_end_matches('/').to_string(),
            _private_key: private_key,
            _api_key: api_key,
            _api_secret: api_secret,
            _api_passphrase: api_passphrase,
            chain_id,
            _client: client,
        })
    }
}

#[async_trait::async_trait]
impl ClobClient for LiveClob {
    async fn place_limit_order(
        &self,
        params: LimitOrderParams,
        _order_type: OrderType,
    ) -> Result<PlaceOrderResult> {
        let _ = (params, self.chain_id);
        // EIP-712 signing and POST /order would go here. Polymarket expects:
        // - Build order payload (maker, signer, taker, tokenId, price, size, side, expiration, nonce, feeRateBps, ...)
        // - Sign with EIP-712 typed data (domain + order struct)
        // - POST to {clob_host}/order with HMAC auth headers (API_KEY, SIGNATURE, TIMESTAMP, PASSPHRASE)
        // For now return a clear error so users use dry_run or the TypeScript bot for live orders.
        Ok(PlaceOrderResult {
            order_id: None,
            success: false,
            error_msg: Some(
                "Live order placement not implemented in Rust (EIP-712 signing required). \
                 Use MM_DRY_RUN=true to run in simulation, or use the TypeScript Interval Sniper for live orders."
                    .to_string(),
            ),
        })
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
