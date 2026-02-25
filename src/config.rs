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
    /// When true (OUTCOME=both or TRADE_BOTH_SIDES=1), scan and execute on both Up and Down when price is in range.
    pub trade_both_sides: bool,
    pub buy_min: Decimal,
    pub buy_max: Decimal,
    pub take_profit_trigger: Decimal,
    pub stop_loss_trigger: Decimal,
    pub order_size: Decimal,
    pub max_position: Decimal,
    pub dedupe_ttl: Duration,
    pub stale_threshold: Duration,
    pub clob_url: String,
    /// Seconds to wait after interval start (or after interval switch) before allowing buy. Default 3.
    pub min_delay_after_interval_start_sec: u64,
    /// Minimum time (ms) the buy order must stay on the book before we allow cancel/replace. Gives the order time to fill before we cancel it. Default 2000.
    pub buy_order_min_age_ms: u64,
    /// Max frequency (ms) for syncing position from resting buy order via get_order. Lower = faster fill detection, more REST calls. Default 200. Set lower (e.g. 100) for HFT.
    pub order_sync_interval_ms: u64,
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

        let trade_both_sides = std::env::var("OUTCOME")
            .map(|v| v.to_lowercase() == "both")
            .unwrap_or(false)
            || std::env::var("TRADE_BOTH_SIDES")
                .map(|v| {
                    let low = v.to_lowercase();
                    low == "1" || low == "true" || low == "yes"
                })
                .unwrap_or(false);

        let order_size = parse_env_decimal("ORDER_SIZE", dec!(100))?;
        let max_position = parse_env_decimal("MAX_POSITION", dec!(500))?;
        let buy_min = normalize_price_to_zero_one(parse_env_decimal("BUY_MIN", dec!(0.93))?);
        let buy_max = normalize_price_to_zero_one(parse_env_decimal("BUY_MAX", dec!(0.95))?);
        let take_profit_trigger = normalize_price_to_zero_one(parse_env_decimal("TAKE_PROFIT", dec!(0.97))?);
        let stop_loss_trigger = normalize_price_to_zero_one(parse_env_decimal("STOP_LOSS", dec!(0.90))?);

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

        let min_delay_after_interval_start_sec: u64 = std::env::var("MIN_DELAY_AFTER_INTERVAL_START_SEC")
            .unwrap_or_else(|_| "3".into())
            .parse()
            .unwrap_or(3);

        let buy_order_min_age_ms: u64 = std::env::var("BUY_ORDER_MIN_AGE_MS")
            .unwrap_or_else(|_| "2000".into())
            .parse()
            .unwrap_or(2000);

        let order_sync_interval_ms: u64 = std::env::var("ORDER_SYNC_INTERVAL_MS")
            .unwrap_or_else(|_| "200".into())
            .parse()
            .unwrap_or(200);

        Ok(Self {
            token_id,
            auto_btc5m,
            outcome_up,
            trade_both_sides,
            buy_min,
            buy_max,
            take_profit_trigger,
            stop_loss_trigger,
            order_size,
            max_position,
            dedupe_ttl: Duration::from_millis(dedupe_ttl_ms),
            stale_threshold: Duration::from_millis(stale_ms),
            clob_url,
            min_delay_after_interval_start_sec,
            buy_order_min_age_ms,
            order_sync_interval_ms,
        })
    }
}

fn parse_env_decimal(key: &str, default: Decimal) -> Result<Decimal> {
    match std::env::var(key) {
        Ok(val) => val.parse().with_context(|| format!("Invalid {key}")),
        Err(_) => Ok(default),
    }
}

/// Normalize price to [0, 1]. If value > 1, treat as cents (e.g. 95 -> 0.95). Matches polybot normalizePriceToZeroOne.
fn normalize_price_to_zero_one(v: Decimal) -> Decimal {
    if v > dec!(1) {
        (v / dec!(100)).min(dec!(1)).max(Decimal::ZERO)
    } else {
        v.min(dec!(1)).max(Decimal::ZERO)
    }
}
