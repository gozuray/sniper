//! Config from environment (MM_* / INTERVAL_SNIPER_*).

use crate::types::{Config, OrderStrategy, SellOrderTimeInForce};
use anyhow::Result;
use rust_decimal::Decimal;
use std::str::FromStr;

const BTC_5MIN_INTERVAL_SEC: u64 = 300;
const DEFAULT_SECONDS_BEFORE_CLOSE: u32 = 20;
const DEFAULT_SIZE_SHARES: &str = "5";
const DEFAULT_MIN_BUY_PRICE: &str = "0.9";
const DEFAULT_MAX_BUY_PRICE: &str = "0.95";

fn env(key: &str, default: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| default.to_string())
}

fn env_decimal(key: &str, default: &str) -> Decimal {
    Decimal::from_str(env(key, default).as_str())
        .unwrap_or_else(|_| Decimal::from_str(default).unwrap())
}

fn env_u32(key: &str, default: u32) -> u32 {
    env(key, &default.to_string()).parse().unwrap_or(default)
}

fn env_u64(key: &str, default: u64) -> u64 {
    env(key, &default.to_string()).parse().unwrap_or(default)
}

fn env_bool(key: &str, default: bool) -> bool {
    let v = env(key, if default { "true" } else { "false" });
    v.to_lowercase() == "true" || v == "1"
}

/// Normalize price to 0..=1 (Polymarket probabilities). Values > 1 treated as cents (90 -> 0.9).
fn normalize_price(v: Decimal) -> Decimal {
    if v > Decimal::ONE {
        (v / Decimal::from(100))
            .min(Decimal::ONE)
            .max(Decimal::ZERO)
    } else {
        v.min(Decimal::ONE).max(Decimal::ZERO)
    }
}

/// Current 5min interval start (unix). Polymarket slug uses interval start.
pub fn current_5min_interval_start_unix() -> u64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    (now / BTC_5MIN_INTERVAL_SEC) * BTC_5MIN_INTERVAL_SEC
}

/// Current 5min interval end (close time).
pub fn current_5min_interval_end_unix() -> u64 {
    current_5min_interval_start_unix() + BTC_5MIN_INTERVAL_SEC
}

/// Slug prefix for asset.
pub fn slug_prefix(asset: crate::types::IntervalMarketAsset) -> &'static str {
    match asset {
        crate::types::IntervalMarketAsset::Btc5m => "btc-updown-5m",
        crate::types::IntervalMarketAsset::Sol5m => "sol-updown-5m",
    }
}

/// Current 5min slug for asset (interval that is open now).
pub fn current_5min_slug(asset: crate::types::IntervalMarketAsset) -> String {
    format!(
        "{}-{}",
        slug_prefix(asset),
        current_5min_interval_start_unix()
    )
}

/// Load config from environment.
pub fn load_config() -> Result<Config> {
    let interval_market = crate::types::IntervalMarketAsset::from_str(
        env("INTERVAL_SNIPER_MARKET", "btc_5m").as_str(),
    );
    let interval_market = interval_market.unwrap(); // FromStr Err is Infallible
                                                    // For BTC/SOL 5m we always use the current 5-min interval slug (e.g. btc-updown-5m-1772169300 for 5:15–5:20).
                                                    // Do not pin to a fixed MM_MARKET_SLUG so the bot subscribes to the live interval.
    let market_slug = current_5min_slug(interval_market);

    let order_strategy = match env("MM_ORDER_STRATEGY", "fak_cross_spread")
        .to_lowercase()
        .as_str()
    {
        "gtc_resting" => OrderStrategy::GtcResting,
        "fok_same_price" => OrderStrategy::FokSamePrice,
        "fak_same_price" => OrderStrategy::FakSamePrice,
        "cross_spread" => OrderStrategy::CrossSpread,
        "fok_cross_spread" => OrderStrategy::FokCrossSpread,
        "fak_cross_spread" => OrderStrategy::FakCrossSpread,
        "market_fok" => OrderStrategy::MarketFok,
        _ => OrderStrategy::FakCrossSpread,
    };

    let take_profit_tif = match env("MM_TAKE_PROFIT_TIME_IN_FORCE", "FAK")
        .to_uppercase()
        .as_str()
    {
        "FOK" => SellOrderTimeInForce::Fok,
        "FAK" => SellOrderTimeInForce::Fak,
        _ => SellOrderTimeInForce::Gtc,
    };

    let loop_ms = env_u64("MM_LOOP_MS", 100).clamp(1, 500);
    let cooldown_ms = env_u64("MM_COOLDOWN_MS", 2000).min(60000);
    // Take profit / stop loss: fixed prices (0..=1). Sell when best_bid >= take_profit_price (TP) or best_bid <= stop_loss_price (SL).
    let tp_default = env("TAKE_PROFIT", "0.97");
    let sl_default = env("STOP_LOSS", "0.90");
    let take_profit_price = normalize_price(env_decimal("MM_TAKE_PROFIT_PRICE", &tp_default));
    let stop_loss_price = normalize_price(env_decimal("MM_STOP_LOSS_PRICE", &sl_default));
    let take_profit_margin = env_decimal("MM_TAKE_PROFIT_PRICE_MARGIN", "0.01");
    let take_profit_margin = take_profit_margin
        .max(Decimal::ZERO)
        .min(Decimal::from_str("0.05").unwrap_or(take_profit_margin));

    Ok(Config {
        interval_market,
        market_slug: market_slug.clone(),
        gamma_base_url: env("POLYMARKET_REST_BASE", "https://gamma-api.polymarket.com"),
        seconds_before_close: env_u32("MM_SECONDS_BEFORE_CLOSE", DEFAULT_SECONDS_BEFORE_CLOSE),
        size_shares: env_decimal("MM_SIZE_SHARES", DEFAULT_SIZE_SHARES).round_dp(2),
        min_buy_price: normalize_price(env_decimal("MM_MIN_BUY_PRICE", DEFAULT_MIN_BUY_PRICE)),
        max_buy_price: normalize_price(env_decimal("MM_MAX_BUY_PRICE", DEFAULT_MAX_BUY_PRICE)),
        allow_buy_up: env_bool("MM_ALLOW_BUY_UP", true),
        allow_buy_down: env_bool("MM_ALLOW_BUY_DOWN", true),
        min_btc_price_diff_usd: env_decimal("MM_MIN_BTC_PRICE_DIFF_USD", "0"),
        dry_run: env_bool("MM_DRY_RUN", true),
        order_strategy,
        enable_auto_sell: env_bool("MM_ENABLE_AUTO_SELL", true),
        take_profit_price,
        auto_sell_at_max_price: env_bool("MM_AUTO_SELL_AT_MAX_PRICE", false),
        auto_sell_quantity_percent: env_u32("MM_AUTO_SELL_QUANTITY_PERCENT", 100).clamp(1, 100)
            as u8,
        take_profit_time_in_force: take_profit_tif,
        enable_stop_loss: env_bool("MM_ENABLE_STOP_LOSS", true),
        stop_loss_price,
        stop_loss_quantity_percent: env_u32("MM_STOP_LOSS_QUANTITY_PERCENT", 100).clamp(1, 100)
            as u8,
        loop_ms,
        cooldown_between_orders_ms: cooldown_ms,
        no_window_all_intervals: env_bool("MM_NO_WINDOW_ALL_INTERVALS", true),
        min_seconds_after_market_open: env_u32("MM_MIN_SECONDS_AFTER_MARKET_OPEN", 0).min(300),
        min_seconds_after_buy_before_auto_sell: env_u32(
            "MM_MIN_SECONDS_AFTER_BUY_BEFORE_AUTO_SELL",
            0,
        )
        .min(30),
        take_profit_price_margin: take_profit_margin,
    })
}
