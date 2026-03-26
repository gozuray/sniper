use crate::cex::{SpotFeed, SpotHistorySet};
use crate::types::{Asset, SpotSample};
use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use reqwest::Client;
use rust_decimal::Decimal;
use serde::Deserialize;
use serde_json::Value;
use std::str::FromStr;
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tokio_tungstenite::{connect_async, tungstenite::Message};

#[derive(Debug, Deserialize)]
struct StreamWrapperAgg {
    stream: String,
    data: AggTradeData,
}

#[derive(Debug, Deserialize)]
struct AggTradeData {
    /// Price (as string).
    #[serde(rename = "p")]
    price: String,
    /// Quote volume (as string, quote asset amount).
    #[serde(rename = "q")]
    quote_volume: String,
    /// Trade time (ms).
    #[serde(rename = "T")]
    trade_time_ms: u64,
    /// Was the buyer the market maker? `false` → compra agresora (taker buy).
    #[serde(rename = "m")]
    is_buyer_maker: Option<bool>,
}

fn parse_decimal(s: &str) -> Result<Decimal> {
    Decimal::from_str(s.trim()).context("decimal parse")
}

fn json_u64(v: &Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_i64().map(|i| i as u64))
}

/// Primer aggTrade spot con `T >= interval_start` (REST). Si aún no hubo trades en la franja, último `ticker/price`.
pub async fn fetch_binance_first_agg_price_from_interval_start(
    http: &Client,
    symbol: &str,
    interval_start_unix_sec: u64,
) -> Result<Decimal> {
    let sym = symbol.trim().to_uppercase();
    let start_ms = interval_start_unix_sec.saturating_mul(1000);
    let url = format!(
        "https://api.binance.com/api/v3/aggTrades?symbol={sym}&startTime={start_ms}&limit=1000"
    );
    let rows: Vec<Value> = http
        .get(url)
        .header("Accept", "application/json")
        .send()
        .await
        .context("Binance GET /api/v3/aggTrades")?
        .json()
        .await
        .context("aggTrades JSON")?;

    for row in rows.iter() {
        let Some(t) = row.get("T").and_then(json_u64) else {
            continue;
        };
        if t < start_ms {
            continue;
        }
        let Some(p) = row.get("p").and_then(|v| v.as_str()) else {
            continue;
        };
        return parse_decimal(p);
    }

    tracing::warn!(
        target: "sniper",
        symbol = %sym,
        interval_start_unix_sec,
        "Binance: ningún aggTrade con T >= inicio de franja; uso ticker/price"
    );
    fetch_binance_ticker_price(http, &sym).await
}

#[derive(Debug, Deserialize)]
struct BinanceTickerPrice {
    #[serde(rename = "price")]
    price: String,
}

/// Último [`symbolPriceTicker`](https://developers.binance.com/docs/binance-spot-api-docs/rest-api#symbol-price-ticker) (spot).
pub async fn fetch_binance_ticker_price(http: &Client, symbol: &str) -> Result<Decimal> {
    let sym = symbol.trim().to_uppercase();
    let url = format!("https://api.binance.com/api/v3/ticker/price?symbol={sym}");
    let row: BinanceTickerPrice = http
        .get(url)
        .header("Accept", "application/json")
        .send()
        .await
        .context("Binance GET /api/v3/ticker/price")?
        .json()
        .await
        .context("ticker/price JSON")?;
    parse_decimal(&row.price)
}

fn asset_from_stream(stream: &str) -> Option<Asset> {
    let lower = stream.to_ascii_lowercase();
    if lower.starts_with("btcusdt") {
        Some(Asset::BTC)
    } else if lower.starts_with("ethusdt") {
        Some(Asset::ETH)
    } else if lower.starts_with("solusdt") {
        Some(Asset::SOL)
    } else {
        None
    }
}

/// Binance spot `aggTrade` por WS (sin klines).
pub struct BinanceAggTradeFeed {
    pub url: String,
}

impl BinanceAggTradeFeed {
    pub fn new() -> Self {
        Self {
            url: "wss://stream.binance.com:9443/stream".to_string(),
        }
    }

    fn make_streams_url(&self, assets: &[Asset]) -> String {
        let mut streams: Vec<String> = Vec::new();
        for a in assets {
            let sym = a.as_binance_symbol();
            streams.push(format!("{sym}@aggTrade"));
        }
        format!("{}?streams={}", self.url, streams.join("/"))
    }
}

impl SpotFeed for BinanceAggTradeFeed {
    fn spawn(
        self,
        shutdown: CancellationToken,
        history: SpotHistorySet,
        assets: Vec<Asset>,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut attempt: u32 = 0;
            let url = self.make_streams_url(&assets);
            let backoff_base = Duration::from_millis(250);

            loop {
                if shutdown.is_cancelled() {
                    return;
                }

                match connect_async(&url).await {
                    Ok((ws_stream, _)) => {
                        attempt = 0;
                        let (mut write, read) = ws_stream.split();

                        let mut ping_tick = tokio::time::interval(Duration::from_secs(20));
                        ping_tick.tick().await;

                        let mut read = read;
                        while !shutdown.is_cancelled() {
                            tokio::select! {
                                _ = ping_tick.tick() => {
                                    if write.send(Message::Ping(vec![])).await.is_err() {
                                        break;
                                    }
                                }
                                msg = read.next() => {
                                    match msg {
                                        Some(Ok(Message::Text(text))) => {
                                            if let Ok(wrapper) = serde_json::from_str::<StreamWrapperAgg>(&text) {
                                                if let Some(asset) = asset_from_stream(&wrapper.stream) {
                                                    let price = match parse_decimal(&wrapper.data.price) {
                                                        Ok(p) => p,
                                                        Err(_) => continue,
                                                    };
                                                    let qty = match parse_decimal(&wrapper.data.quote_volume) {
                                                        Ok(v) => v,
                                                        Err(_) => continue,
                                                    };
                                                    // Binance aggTrade `q` = base-asset qty (BTC).
                                                    // Convert to quote notional (USDT) to match Coinbase and config units.
                                                    let notional = price * qty;
                                                    let (taker_buy_quote, taker_sell_quote) =
                                                        match wrapper.data.is_buyer_maker {
                                                            Some(true) => {
                                                                // Maker fue comprador → vendedor agresor.
                                                                (Decimal::ZERO, notional)
                                                            }
                                                            Some(false) => (notional, Decimal::ZERO),
                                                            None => (Decimal::ZERO, Decimal::ZERO),
                                                        };
                                                    let sample = SpotSample {
                                                        ts_ms: wrapper.data.trade_time_ms,
                                                        price,
                                                        quote_volume: notional,
                                                        taker_buy_quote,
                                                        taker_sell_quote,
                                                    };
                                                    let history_state = history.state_for(asset);
                                                    let mut guard = history_state.write().await;
                                                    guard.push(sample);
                                                }
                                            }
                                        }
                                        Some(Ok(Message::Close(_))) => break,
                                        Some(Ok(_)) => {}
                                        Some(Err(_)) => break,
                                        None => break,
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => {
                        attempt = attempt.saturating_add(1);
                        let delay = backoff_base
                            .mul_f64(2f64.powi(attempt.min(6) as i32) as f64)
                            .min(Duration::from_secs(30));
                        tracing::warn!(
                            error = %e,
                            attempt = attempt,
                            "Binance WS connect failed; reconnecting"
                        );
                        sleep(delay).await;
                    }
                }
            }
        })
    }
}
