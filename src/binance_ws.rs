//! Binance WebSocket client for BTC/USDT AggTrade stream.
//!
//! Connects to `wss://stream.binance.com:9443/ws/btcusdt@aggTrade`, parses
//! aggregate trade events, maintains EWMA-smoothed price and % change over
//! the last `window_sec` seconds (rolling window). Used for BTC momentum signal.

use anyhow::{Context, Result};
use futures_util::StreamExt;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::collections::VecDeque;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::types::{BtcDirection, BtcPriceState};

const BINANCE_AGGTRADE_URL: &str = "wss://stream.binance.com:9443/ws/btcusdt@aggTrade";
const CONNECT_TIMEOUT_SECS: u64 = 20;
/// Default EWMA alpha if not provided (0.3 = moderate smoothing).
const DEFAULT_EWMA_ALPHA: f64 = 0.3;

/// AggTrade event from Binance (minimal fields we need).
#[derive(serde::Deserialize)]
struct AggTradeEvent {
    #[serde(rename = "p")]
    price: String,
    #[serde(rename = "T")]
    trade_time_ms: u64,
}

/// Start the Binance AggTrade WebSocket in a background task.
/// `window_sec`: % change is computed over the last N seconds (rolling window).
/// `ewma_alpha`: 0.0 = max smoothing, 1.0 = no smoothing (use raw price).
/// `up_pct` / `down_pct`: thresholds (e.g. 0.05) for direction: Up when pct_change >= up_pct, Down when pct_change <= -down_pct.
pub async fn start(
    window_sec: u64,
    ewma_alpha: f64,
    up_pct: Decimal,
    down_pct: Decimal,
) -> Arc<RwLock<BtcPriceState>> {
    let alpha = if ewma_alpha <= 0.0 || ewma_alpha > 1.0 {
        DEFAULT_EWMA_ALPHA
    } else {
        ewma_alpha
    };
    let state = Arc::new(RwLock::new(BtcPriceState {
        current_price: Decimal::ZERO,
        candle_open_price: Decimal::ZERO,
        pct_change: Decimal::ZERO,
        direction: BtcDirection::Neutral,
        last_update_ms: 0,
    }));

    let state_clone = Arc::clone(&state);
    tokio::spawn(async move {
        run_loop(state_clone, window_sec, alpha, up_pct, down_pct).await;
    });

    state
}

/// Reset candle open price to current price (kept for compatibility; rolling window is the main source).
pub async fn reset_candle_open(state: &Arc<RwLock<BtcPriceState>>) {
    let _ = state;
}

fn pct_to_direction(pct: Decimal, up_pct: Decimal, down_pct: Decimal) -> BtcDirection {
    if pct >= up_pct {
        BtcDirection::Up
    } else if pct <= -down_pct {
        BtcDirection::Down
    } else {
        BtcDirection::Neutral
    }
}

async fn run_loop(
    state: Arc<RwLock<BtcPriceState>>,
    window_sec: u64,
    ewma_alpha: f64,
    up_pct: Decimal,
    down_pct: Decimal,
) {
    let mut attempt = 0u32;
    loop {
        let connect_result = tokio::time::timeout(
            Duration::from_secs(CONNECT_TIMEOUT_SECS),
            connect_async(BINANCE_AGGTRADE_URL),
        )
        .await;

        let ws_stream = match connect_result {
            Ok(Ok((stream, _))) => {
                attempt = 0;
                tracing::info!("[BinanceWS] connected to BTC/USDT aggTrade");
                stream
            }
            Ok(Err(e)) => {
                attempt += 1;
                let delay_ms = (500u64 * 2u64.pow(attempt.min(6))).min(30_000);
                tracing::warn!(
                    "[BinanceWS] connect failed: {} — retry in {}ms",
                    e,
                    delay_ms
                );
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                continue;
            }
            Err(_) => {
                attempt += 1;
                let delay_ms = (500u64 * 2u64.pow(attempt.min(6))).min(30_000);
                tracing::warn!(
                    "[BinanceWS] connect timeout ({}s) — retry in {}ms",
                    CONNECT_TIMEOUT_SECS,
                    delay_ms
                );
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                continue;
            }
        };

        let (mut _write, mut read) = ws_stream.split();
        let mut ewma = Option::<f64>::None;
        let mut price_history: VecDeque<(u64, f64)> = VecDeque::new();

        while let Some(msg_result) = read.next().await {
            match msg_result {
                Ok(Message::Text(text)) => {
                    if let Err(e) = apply_agg_trade(
                        &state,
                        &text,
                        window_sec,
                        ewma_alpha,
                        &mut ewma,
                        &mut price_history,
                        up_pct,
                        down_pct,
                    )
                    .await
                    {
                        tracing::debug!(
                            "[BinanceWS] parse: {} | {}",
                            e,
                            text.chars().take(200).collect::<String>()
                        );
                    }
                }
                Ok(Message::Pong(_)) => {}
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!("[BinanceWS] stream error: {} — reconnecting", e);
                    break;
                }
            }
        }
        tracing::warn!("[BinanceWS] stream closed — reconnecting");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

async fn apply_agg_trade(
    state: &Arc<RwLock<BtcPriceState>>,
    text: &str,
    window_sec: u64,
    ewma_alpha: f64,
    ewma: &mut Option<f64>,
    price_history: &mut VecDeque<(u64, f64)>,
    up_pct: Decimal,
    down_pct: Decimal,
) -> Result<()> {
    let ev: AggTradeEvent = serde_json::from_str(text).context("parse aggTrade JSON")?;
    let price_f: f64 = ev.price.parse().context("parse price")?;
    let now = ev.trade_time_ms;

    let new_ewma = match *ewma {
        Some(prev) => prev + ewma_alpha * (price_f - prev),
        None => price_f,
    };
    *ewma = Some(new_ewma);

    let current_price =
        Decimal::from_str(&format!("{:.2}", new_ewma)).unwrap_or(Decimal::ZERO);
    if current_price <= Decimal::ZERO {
        return Ok(());
    }

    price_history.push_back((now, new_ewma));
    let window_ms = window_sec * 1000;
    let cutoff = now.saturating_sub(window_ms);
    while price_history.front().map(|(t, _)| *t < cutoff).unwrap_or(false) {
        price_history.pop_front();
    }

    let mut s = state.write().await;
    s.current_price = current_price;
    s.last_update_ms = now;

    if let Some(&(_ref_ts, ref_price)) = price_history.front() {
        if ref_price > 0.0 {
            let ref_dec = Decimal::from_str(&format!("{:.2}", ref_price)).unwrap_or(Decimal::ZERO);
            if ref_dec > Decimal::ZERO {
                let diff = s.current_price - ref_dec;
                s.pct_change = (diff / ref_dec) * dec!(100);
                s.direction = pct_to_direction(s.pct_change, up_pct, down_pct);
            }
        }
    }

    Ok(())
}
