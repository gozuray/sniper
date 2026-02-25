use anyhow::{Context, Result};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Config {
    /// Set by TOKEN_ID, or resolved from Gamma when auto_btc5m is true.
    pub token_id: String,
    /// When true, ignore TOKEN_ID and resolve token from Gamma each 5-min window.
    pub auto_btc5m: bool,
    /// For auto_btc5m: true = trade "Up" (first token), false = "Down" (second token).
    pub outcome_up: bool,
    pub buy_min: Decimal,
    pub buy_max: Decimal,
    pub take_profit_trigger: Decimal,
    pub stop_loss_trigger: Decimal,
    pub order_size: Decimal,
    pub max_position: Decimal,
    pub dedupe_ttl: Duration,
    pub stale_threshold: Duration,
    pub clob_url: String,
}

impl Config {
    pub fn from_env() -> Result<Self> {
        let auto_btc5m = std::env::var("AUTO_BTC5M")
            .map(|v| {
                let low = v.to_lowercase();
                low == "1" || low == "true" || low == "yes"
            })
            .unwrap_or(false);

        let token_id = if auto_btc5m {
            std::env::var("TOKEN_ID").unwrap_or_default()
        } else {
            std::env::var("TOKEN_ID").context("TOKEN_ID is required (or set AUTO_BTC5M=1)")?
        };

        let outcome_up = std::env::var("OUTCOME")
            .map(|v| {
                let low = v.to_lowercase();
                low != "down" && low != "no"
            })
            .unwrap_or(true);

        let order_size = parse_env_decimal("ORDER_SIZE", dec!(100))?;
        let max_position = parse_env_decimal("MAX_POSITION", dec!(500))?;
        let buy_min = parse_env_decimal("BUY_MIN", dec!(0.93))?;
        let buy_max = parse_env_decimal("BUY_MAX", dec!(0.95))?;
        let take_profit_trigger = parse_env_decimal("TAKE_PROFIT", dec!(0.97))?;
        let stop_loss_trigger = parse_env_decimal("STOP_LOSS", dec!(0.90))?;

        let dedupe_ttl_ms: u64 = std::env::var("DEDUPE_TTL_MS")
            .unwrap_or_else(|_| "50".into())
            .parse()
            .context("Invalid DEDUPE_TTL_MS")?;

        let stale_ms: u64 = std::env::var("STALE_THRESHOLD_MS")
            .unwrap_or_else(|_| "200".into())
            .parse()
            .context("Invalid STALE_THRESHOLD_MS")?;

        let clob_url = std::env::var("POLYMARKET_CLOB_URL")
            .unwrap_or_else(|_| "https://clob.polymarket.com".into());

        Ok(Self {
            token_id,
            auto_btc5m,
            outcome_up,
            buy_min,
            buy_max,
            take_profit_trigger,
            stop_loss_trigger,
            order_size,
            max_position,
            dedupe_ttl: Duration::from_millis(dedupe_ttl_ms),
            stale_threshold: Duration::from_millis(stale_ms),
            clob_url,
        })
    }
}

fn parse_env_decimal(key: &str, default: Decimal) -> Result<Decimal> {
    match std::env::var(key) {
        Ok(val) => val.parse().with_context(|| format!("Invalid {key}")),
        Err(_) => Ok(default),
    }
}
