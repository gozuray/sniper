use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

pub type TokenId = polymarket_client_sdk::types::U256;
pub type ConditionId = polymarket_client_sdk::types::B256;
pub type Price = Decimal;

pub const ASSET_COUNT: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Asset {
    BTC,
    ETH,
    SOL,
}

impl Asset {
    pub fn idx(self) -> usize {
        match self {
            Asset::BTC => 0,
            Asset::ETH => 1,
            Asset::SOL => 2,
        }
    }

    pub fn as_binance_symbol(self) -> &'static str {
        match self {
            Asset::BTC => "btcusdt",
            Asset::ETH => "ethusdt",
            Asset::SOL => "solusdt",
        }
    }

    pub fn as_coinbase_product_id(self) -> &'static str {
        match self {
            Asset::BTC => "BTC-USD",
            Asset::ETH => "ETH-USD",
            Asset::SOL => "SOL-USD",
        }
    }

    pub fn as_gamma_slug_prefix(self) -> &'static str {
        match self {
            Asset::BTC => "btc",
            Asset::ETH => "eth",
            Asset::SOL => "sol",
        }
    }
}

impl FromStr for Asset {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let up = s.trim().to_uppercase();
        Ok(match up.as_str() {
            "BTC" => Asset::BTC,
            "ETH" => Asset::ETH,
            "SOL" => Asset::SOL,
            _ => return Err(()),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Interval {
    #[serde(rename = "5m")]
    M5,
    #[serde(rename = "15m")]
    M15,
}

impl Interval {
    pub fn sec(self) -> u64 {
        match self {
            Interval::M5 => 300,
            Interval::M15 => 900,
        }
    }

    pub fn as_slug_suffix(self) -> &'static str {
        match self {
            Interval::M5 => "5m",
            Interval::M15 => "15m",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct MarketKey {
    pub asset: Asset,
    pub interval: Interval,
}

impl MarketKey {
    #[allow(dead_code)]
    pub fn new(asset: Asset, interval: Interval) -> Self {
        Self { asset, interval }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Up,
    Down,
}

impl Outcome {
    #[allow(dead_code)]
    pub fn opposite(self) -> Outcome {
        match self {
            Outcome::Up => Outcome::Down,
            Outcome::Down => Outcome::Up,
        }
    }
}

/// Ring-buffer sample for momentum computation.
#[derive(Debug, Clone)]
pub struct SpotSample {
    pub ts_ms: u64,
    pub price: Price,
    pub quote_volume: Price,
    /// Notional atribuido a compras agresoras (taker buy). Binance `aggTrade`: `m` == false.
    pub taker_buy_quote: Price,
    /// Notional atribuido a ventas agresoras (taker sell). Binance: `m` == true.
    pub taker_sell_quote: Price,
}

/// Per-asset state shared between spot feed tasks and the strategy.
#[derive(Debug, Clone)]
pub struct SpotAssetState {
    pub last_price: Price,
    pub last_update_ms: u64,
    pub samples: Vec<SpotSample>,
    pub head: usize,
    pub len: usize,
    /// Ancla: media de hasta 7 venues (Binance agg ≥t0, Coinbase trade/5m/ticker, Kraken/Bybit/OKX/Bitfinex/Bitstamp open vela 5m REST) al arranque/rollover.
    pub binance_5m_open: Price,
    /// `interval_start_unix` de la franja Polymarket en ms (corte para sumar `quote_volume` desde el agg WS).
    pub binance_5m_open_ms: u64,
    /// Última vez que se actualizó la referencia (`REST` al cambiar de franja o arranque); frescura del ancla.
    pub binance_5m_kline_event_ms: u64,
}

impl SpotAssetState {
    pub fn new(capacity: usize) -> Self {
        let samples = std::iter::repeat(SpotSample {
            ts_ms: 0,
            price: Price::ZERO,
            quote_volume: Price::ZERO,
            taker_buy_quote: Price::ZERO,
            taker_sell_quote: Price::ZERO,
        })
        .take(capacity.max(1))
        .collect::<Vec<_>>();
        Self {
            last_price: Price::ZERO,
            last_update_ms: 0,
            samples,
            head: 0,
            len: 0,
            binance_5m_open: Price::ZERO,
            binance_5m_open_ms: 0,
            binance_5m_kline_event_ms: 0,
        }
    }

    pub fn push(&mut self, sample: SpotSample) {
        // Cache fields we need after moving `sample` into the ring-buffer.
        let price = sample.price;
        let ts_ms = sample.ts_ms;
        let cap = self.samples.len();
        self.samples[self.head] = sample;
        self.head = (self.head + 1) % cap;
        self.len = (self.len + 1).min(cap);
        self.last_price = price;
        self.last_update_ms = ts_ms;
    }

    pub fn for_each_recent<F>(&self, cutoff_ts_ms: u64, mut f: F)
    where
        F: FnMut(&SpotSample),
    {
        if self.len == 0 {
            return;
        }
        let cap = self.samples.len();
        let oldest = (self.head + cap - self.len) % cap;
        for i in 0..self.len {
            let idx = (oldest + i) % cap;
            let s = &self.samples[idx];
            if s.ts_ms >= cutoff_ts_ms {
                f(s);
            }
        }
    }
}

/// Best bid/ask for a token (conditional outcome token).
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, Default)]
pub struct BookSide {
    pub best_bid: Price,
    pub best_bid_size: Price,
    pub best_ask: Price,
    pub best_ask_size: Price,
    pub has_bid: bool,
    pub has_ask: bool,
}

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct BookState {
    pub sides: Vec<(TokenId, BookSide)>,
}

/// Probabilidad “fuerte” coherente con `MomentumSnapshot` / señal.
pub fn momentum_p_strong_from_signal(signal: &Signal) -> Option<f64> {
    match signal {
        Signal::Momentum {
            fair_prob_up,
            outcome,
            ..
        } => Some(match outcome {
            Outcome::Up => *fair_prob_up,
            Outcome::Down => 1.0 - *fair_prob_up,
        }),
        Signal::ArbBoth { .. } => None,
    }
}

impl fmt::Display for MarketKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}-{}", self.asset, self.interval.as_slug_suffix())
    }
}

/// Fully resolved market instance for a specific interval start.
#[derive(Debug, Clone)]
pub struct ResolvedMarket {
    pub key: MarketKey,
    #[allow(dead_code)]
    pub slug: String,
    #[allow(dead_code)]
    pub condition_id: ConditionId,
    pub interval_start_unix: u64,
    pub close_time_unix: u64,
    pub token_id_up: TokenId,
    pub token_id_down: TokenId,
}

/// Decision signal for a single market interval.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum Signal {
    /// Momentum-driven single-side entry (buy one outcome).
    Momentum {
        market: MarketKey,
        interval_start_unix: u64,
        close_time_unix: u64,
        token_id_up: TokenId,
        token_id_down: TokenId,
        outcome: Outcome,
        fair_prob_up: f64,
        market_prob_side: Price,
        edge: Price,
    },
    /// Pure arbitrage: buy both sides with equal/compatible sizing.
    ArbBoth {
        market: MarketKey,
        interval_start_unix: u64,
        close_time_unix: u64,
        token_id_up: TokenId,
        token_id_down: TokenId,
        fair_prob_up: f64,
        market_prob_up: Price,
        market_prob_down: Price,
        edge: Price,
    },
}

/// Campos opcionales para `trades.jsonl` y diagnóstico (entrada).
#[derive(Debug, Clone, Default)]
pub struct TradeEntryDiag {
    /// `p_strong` del lado operado (Up: fair_up; Down: 1 − fair_up).
    pub p_strong: f64,
    pub spread: Price,
    pub time_remaining_sec: u64,
    /// (bid_sz − ask_sz) / (bid_sz + ask_sz) en el token entrado.
    pub ob_imbalance: Option<f64>,
    pub btc_spot: Option<Price>,
    /// Última acción RL aplicada al cerrar el intervalo anterior (`idx:label`), solo paper+RL.
    pub rl_action_applied: Option<String>,
}

/// Executed position tracked until interval settlement.
#[derive(Debug, Clone)]
pub struct Position {
    #[allow(dead_code)]
    pub position_id: u64,
    pub market: MarketKey,
    pub interval_start_unix: u64,
    pub close_time_unix: u64,
    #[allow(dead_code)]
    pub created_at_ms: u64,
    pub kind: Signal,
    #[allow(dead_code)]
    pub entry_diag: TradeEntryDiag,
    pub legs: Vec<PositionLeg>,
    pub settled: bool,
    /// Realized via take-profit sell before interval close.
    pub closed_via_take_profit: bool,
    /// Realized via stop-loss (bid adversity / momentum fade) antes del settle.
    pub closed_via_stop_loss: bool,
    /// Live: sell order id waiting for fill (or partial retry).
    pub take_profit_order_id: Option<String>,
    /// Live: stop-loss sell id (mismo mecanismo que TP, distinto disparador).
    pub stop_loss_order_id: Option<String>,
    /// Highest observed best_bid since position opened (for trailing SL).
    pub high_water_mark: Price,
    /// Wall time (ms) when exit order was last posted (orphan reconciliation).
    pub exit_order_posted_at_ms: Option<u64>,
    pub pnl_usdc: Price,
    pub win: bool,
    /// Precio de salida del último fill de cierre (TP/SL); settle no lo usa.
    #[allow(dead_code)]
    pub last_exit_price: Option<Price>,
}

#[derive(Debug, Clone)]
pub struct PositionLeg {
    pub token_id: TokenId,
    pub outcome: Outcome,
    pub intended_price: Price,
    pub intended_size: Price,
    pub filled_price: Price,
    pub filled_size: Price,
    pub order_id: Option<String>,
    /// Realized execution slippage vs intended price (bps); set on fill.
    pub realized_slippage_bps: Price,
}

/// A momentum summary snapshot computed from CEX samples.
#[derive(Debug, Clone, Copy)]
pub struct MomentumSnapshot {
    pub fair_prob_up: f64,
    #[allow(dead_code)]
    pub pct_change: f64,
    #[allow(dead_code)]
    pub quote_volume: Price,
    #[allow(dead_code)]
    pub strong: bool,
    pub direction: Outcome,
    /// `interval_start_unix` when using Binance 5m candle open as anchor; `0` = rolling window.
    pub anchor_interval_start_unix: u64,
}

/// Trade timestamps and window boundaries.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy)]
pub struct IntervalPrices {
    pub open_price: Price,
    pub close_price: Price,
}

#[derive(Debug, Clone)]
pub struct SpotIntervalState {
    #[allow(dead_code)]
    pub key: MarketKey,
    #[allow(dead_code)]
    pub interval_start_unix: u64,
    #[allow(dead_code)]
    pub close_time_unix: u64,
    pub open_set: bool,
    pub open_price: Price,
    pub close_set: bool,
    pub close_price: Price,
}

impl SpotIntervalState {
    pub fn new(key: MarketKey, interval_start_unix: u64, close_time_unix: u64) -> Self {
        Self {
            key,
            interval_start_unix,
            close_time_unix,
            open_set: false,
            open_price: Price::ZERO,
            close_set: false,
            close_price: Price::ZERO,
        }
    }
}

