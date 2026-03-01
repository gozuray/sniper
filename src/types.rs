//! Types for Interval Sniper: config, market, order book, runner state.

use rust_decimal::Decimal;
use serde::Deserialize;

/// Market asset: BTC or SOL 5m interval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum IntervalMarketAsset {
    #[default]
    Btc5m,
    Sol5m,
}

impl std::str::FromStr for IntervalMarketAsset {
    type Err = std::convert::Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s.to_lowercase().as_str() {
            "sol_5m" => IntervalMarketAsset::Sol5m,
            _ => IntervalMarketAsset::Btc5m,
        })
    }
}

/// Order strategy: how aggressive the buy order is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderStrategy {
    GtcResting,
    FokSamePrice,
    FakSamePrice,
    CrossSpread,
    FokCrossSpread,
    FakCrossSpread,
    MarketFok,
}

/// Time-in-force for sell orders (TP/SL).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SellOrderTimeInForce {
    Gtc,
    Fok,
    Fak,
}

/// Main config: same semantics as TypeScript Interval Sniper.
#[derive(Debug, Clone)]
pub struct Config {
    pub interval_market: IntervalMarketAsset,
    pub market_slug: String,
    pub gamma_base_url: String,
    pub seconds_before_close: u32,
    pub size_shares: Decimal,
    pub min_buy_price: Decimal,
    pub max_buy_price: Decimal,
    pub allow_buy_up: bool,
    pub allow_buy_down: bool,
    pub min_btc_price_diff_usd: Decimal,
    pub dry_run: bool,
    pub order_strategy: OrderStrategy,
    pub enable_auto_sell: bool,
    /// Fixed price: sell when best_bid >= this (take profit).
    pub take_profit_price: Decimal,
    pub auto_sell_at_max_price: bool,
    pub auto_sell_quantity_percent: u8,
    pub take_profit_time_in_force: SellOrderTimeInForce,
    pub enable_stop_loss: bool,
    /// Fixed price: sell when best_bid <= this (stop loss).
    pub stop_loss_price: Decimal,
    pub stop_loss_quantity_percent: u8,
    pub loop_ms: u64,
    pub cooldown_between_orders_ms: u64,
    pub no_window_all_intervals: bool,
    pub min_seconds_after_market_open: u32,
    pub min_seconds_after_buy_before_auto_sell: u32,
    pub take_profit_price_margin: Decimal,
    /// If true, append session events to a JSONL file in session_log_dir (close, interval_summary, session_summary).
    pub session_log_enabled: bool,
    /// Directory for session log files (e.g. "logs"). Created if missing.
    pub session_log_dir: String,
}

/// Resolved market from Gamma API.
#[derive(Debug, Clone)]
pub struct ResolvedMarket {
    pub slug: String,
    pub condition_id: String,
    pub close_time_unix: u64,
    pub interval_start_unix: u64,
    pub token_id_up: String,
    pub token_id_down: String,
}

/// One side of the book (Up or Down token).
#[derive(Debug, Clone, Default)]
pub struct TopOfBookSide {
    pub best_bid: Option<Decimal>,
    pub best_bid_size: Option<Decimal>,
    pub best_ask: Option<Decimal>,
    pub best_ask_size: Option<Decimal>,
}

/// Top of book for both tokens.
#[derive(Debug, Clone, Default)]
pub struct TopOfBook {
    pub token_id_up: Option<TopOfBookSide>,
    pub token_id_down: Option<TopOfBookSide>,
}

/// Side for entry: Up (YES) or Down (NO).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntrySide {
    Up,
    Down,
}

/// Last buy order (after fill) for TP/SL.
#[derive(Debug, Clone)]
pub struct LastBuyOrder {
    pub token_id: String,
    pub side: EntrySide,
    pub size: Decimal,
    pub price: Decimal,
    pub timestamp_ms: u64,
}

/// Pending take profit: sell when best_bid >= target_price.
#[derive(Debug, Clone)]
pub struct PendingAutoSell {
    pub token_id: String,
    pub target_price: Decimal,
    pub size: Decimal,
    pub placed_at_ms: u64,
}

/// Pending stop loss: sell when best_bid <= trigger_price.
#[derive(Debug, Clone)]
pub struct PendingStopLoss {
    pub token_id: String,
    pub entry_price: Decimal,
    pub size: Decimal,
    pub trigger_price: Decimal,
    pub placed_at_ms: u64,
}

/// Order book from CLOB REST (raw).
#[derive(Debug, Clone, Deserialize)]
pub struct OrderBookRaw {
    pub bids: Option<Vec<BookLevel>>,
    pub asks: Option<Vec<BookLevel>>,
    #[serde(rename = "min_order_size")]
    pub min_order_size: Option<String>,
    #[serde(rename = "tick_size")]
    pub tick_size: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BookLevel {
    pub price: String,
    pub size: String,
}

/// Gamma API market response (minimal).
#[derive(Debug, Clone, Deserialize)]
pub struct GammaMarket {
    #[serde(rename = "conditionId")]
    pub condition_id: Option<String>,
    pub id: Option<String>,
    #[serde(rename = "endDate")]
    pub end_date: Option<String>,
    #[serde(rename = "endDateIso")]
    pub end_date_iso: Option<String>,
    #[serde(rename = "clobTokenIds")]
    pub clob_token_ids: Option<String>,
    pub tokens: Option<Vec<GammaToken>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct GammaToken {
    #[serde(rename = "token_id")]
    pub token_id: Option<String>,
    pub outcome: Option<String>,
}

/// Gamma event response (for /events/slug/...).
#[derive(Debug, Clone, Deserialize)]
pub struct GammaEvent {
    pub markets: Option<Vec<GammaMarket>>,
}
