use crate::cex::{SpotFeed, SpotHistorySet};
use crate::types::{Asset, SpotSample};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
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
use futures_util::{SinkExt, StreamExt};

#[derive(Debug, Deserialize)]
struct CoinbaseRestTicker {
    price: String,
}

#[derive(Debug, Deserialize)]
struct CoinbaseTradeRow {
    time: String,
    trade_id: u64,
    price: String,
}

fn coinbase_trade_time_ms(s: &str) -> Result<u64> {
    let dt = DateTime::parse_from_rfc3339(s.trim()).context("Coinbase trade time RFC3339")?;
    Ok(dt.with_timezone(&Utc).timestamp_millis() as u64)
}

fn decimal_from_json(v: &Value) -> Result<Decimal> {
    match v {
        Value::String(s) => Decimal::from_str(s.trim()).context("decimal string"),
        Value::Number(n) => Decimal::from_str(&n.to_string()).context("decimal number"),
        _ => anyhow::bail!("tipo JSON inesperado para precio"),
    }
}

/// Timestamp de vela Coinbase (`time`): entero JSON o float (p. ej. `1774495200.0`).
fn json_unix_secs(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n
            .as_u64()
            .or_else(|| n.as_i64().map(|i| i as u64))
            .or_else(|| n.as_f64().filter(|f| f.is_finite()).map(|f| f as u64)),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Primer match público con `time >= interval_start` (pagination `after` = último `trade_id` del lote, trades más antiguos).
/// Si no hubo trades aún (p. ej. <1s tras el corte), open de vela **5m UTC** cuyo bucket contiene `interval_start`; si falla, ticker.
pub async fn fetch_btc_usd_first_trade_from_interval_start(
    http: &Client,
    interval_start_unix_sec: u64,
) -> Result<Decimal> {
    let target_ms = interval_start_unix_sec.saturating_mul(1000);
    let mut after: Option<u64> = None;
    let mut best: Option<(u64, Decimal)> = None;

    for _page in 0..48 {
        let mut url =
            "https://api.exchange.coinbase.com/products/BTC-USD/trades?limit=100".to_string();
        if let Some(id) = after {
            url.push_str(&format!("&after={id}"));
        }
        let rows: Vec<CoinbaseTradeRow> = http
            .get(&url)
            .header("Accept", "application/json")
            .send()
            .await
            .context("Coinbase GET /products/BTC-USD/trades")?
            .json()
            .await
            .context("trades JSON")?;

        if rows.is_empty() {
            break;
        }

        let mut page_min_ms = u64::MAX;
        for row in &rows {
            let ts_ms = coinbase_trade_time_ms(&row.time)?;
            page_min_ms = page_min_ms.min(ts_ms);
            if ts_ms < target_ms {
                continue;
            }
            let price = Decimal::from_str(row.price.trim()).context("trade price")?;
            best = match best {
                None => Some((ts_ms, price)),
                Some((t0, _)) if ts_ms < t0 => Some((ts_ms, price)),
                Some(x) => Some(x),
            };
        }

        if page_min_ms < target_ms {
            break;
        }

        after = Some(
            rows
                .last()
                .context("Coinbase trades: página sin último elemento")?
                .trade_id,
        );
    }

    if let Some((_, p)) = best {
        return Ok(p);
    }

    tracing::trace!(
        target: "sniper",
        interval_start_unix_sec,
        "Coinbase: aún no hay trade público t≥inicio franja; intento vela 5m REST / ticker"
    );
    match fetch_btc_usd_candle_open_5m_containing(http, interval_start_unix_sec).await {
        Ok(p) => Ok(p),
        Err(e) => {
            tracing::trace!(
                target: "sniper",
                error = %e,
                interval_start_unix_sec,
                "Coinbase: vela 5m aún no en REST (típico al abrir bucket); uso ticker"
            );
            fetch_btc_usd_ticker_price(http).await
        }
    }
}

/// Vela spot 5m (UTC) cuyo `[open, open+300)` contiene `interval_start_unix_sec` (misma rejilla que franjas Polymarket 5m en la práctica).
async fn fetch_btc_usd_candle_open_5m_containing(
    http: &Client,
    interval_start_unix_sec: u64,
) -> Result<Decimal> {
    let bucket = (interval_start_unix_sec / 300) * 300;
    let mut last_err: Option<anyhow::Error> = None;

    for attempt in 0u32..3 {
        if attempt > 0 {
            sleep(Duration::from_millis(120)).await;
        }
        match fetch_btc_usd_candle_open_5m_containing_once(http, bucket, interval_start_unix_sec).await {
            Ok(p) => return Ok(p),
            Err(e) => last_err = Some(e),
        }
    }

    Err(last_err.unwrap_or_else(|| {
        anyhow::anyhow!("Coinbase: vela 5m sin respuesta tras reintentos")
    }))
}

/// Una petición REST; al abrir el bucket la respuesta puede ir vacía unos cientos de ms.
async fn fetch_btc_usd_candle_open_5m_containing_once(
    http: &Client,
    bucket: u64,
    interval_start_unix_sec: u64,
) -> Result<Decimal> {
    // Rango ancho + ventana mínima del bucket: `end` es exclusivo en Exchange API.
    let start_req = bucket.saturating_sub(300);
    let end_req = bucket.saturating_add(900);
    let url = format!(
        "https://api.exchange.coinbase.com/products/BTC-USD/candles?granularity=300&start={start_req}&end={end_req}"
    );
    let rows: Vec<Vec<Value>> = http
        .get(&url)
        .header("Accept", "application/json")
        .send()
        .await
        .context("Coinbase GET candles 5m")?
        .json()
        .await
        .context("candles 5m JSON")?;

    let row = rows
        .iter()
        .find(|r| r.first().and_then(json_unix_secs) == Some(bucket))
        .or_else(|| {
            rows.iter().find(|r| {
                if let Some(t) = r.first().and_then(json_unix_secs) {
                    let end_t = t.saturating_add(300);
                    interval_start_unix_sec >= t && interval_start_unix_sec < end_t
                } else {
                    false
                }
            })
        })
        .filter(|r| r.len() >= 6)
        .context("Coinbase: sin vela 5m para el intervalo")?;

    // Formato Exchange: [time, low, high, open, close, volume]
    let open_v = row.get(3).context("candle 5m: campo open")?;
    decimal_from_json(open_v)
}

/// Último precio [`product ticker`](https://docs.cloud.coinbase.com/exchange/reference/exchangerestapi_getproductticker) (BTC-USD).
pub async fn fetch_btc_usd_ticker_price(http: &Client) -> Result<Decimal> {
    let url = "https://api.exchange.coinbase.com/products/BTC-USD/ticker";
    let row: CoinbaseRestTicker = http
        .get(url)
        .header("Accept", "application/json")
        .send()
        .await
        .context("Coinbase GET /products/BTC-USD/ticker")?
        .json()
        .await
        .context("Coinbase ticker JSON")?;
    Decimal::from_str(row.price.trim()).context("Coinbase ticker price parse")
}

#[derive(Debug, Deserialize)]
struct CoinbaseWsMsg {
    #[serde(rename = "type")]
    msg_type: Option<String>,
    product_id: Option<String>,
    price: Option<String>,
    size: Option<String>,
    /// Lado del maker: `sell` → comprador agresor (taker buy); `buy` → vendedor agresor.
    side: Option<String>,
}

fn parse_decimal(s: &str) -> Option<Decimal> {
    Decimal::from_str(s.trim()).ok()
}

fn asset_from_product_id(pid: &str) -> Option<Asset> {
    match pid {
        "BTC-USD" => Some(Asset::BTC),
        "ETH-USD" => Some(Asset::ETH),
        "SOL-USD" => Some(Asset::SOL),
        _ => None,
    }
}

/// Coinbase matches feed (spot).
///
/// We use `type=match` messages and derive quote volume as `price * size`.
pub struct CoinbaseMatchesFeed {
    pub url: String,
}

impl CoinbaseMatchesFeed {
    pub fn new() -> Self {
        Self {
            url: "wss://ws-feed.exchange.coinbase.com".to_string(),
        }
    }

    fn make_subscribe_message(assets: &[Asset]) -> String {
        let product_ids: Vec<&str> = assets.iter().map(|a| a.as_coinbase_product_id()).collect();
        let msg = serde_json::json!({
            "type": "subscribe",
            "channels": [
                { "name": "matches", "product_ids": product_ids }
            ]
        });
        msg.to_string()
    }
}

impl SpotFeed for CoinbaseMatchesFeed {
    fn spawn(
        self,
        shutdown: CancellationToken,
        history: SpotHistorySet,
        assets: Vec<Asset>,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            let mut attempt: u32 = 0;
            let backoff_base = Duration::from_millis(250);
            let subscribe_msg = CoinbaseMatchesFeed::make_subscribe_message(&assets);

            loop {
                if shutdown.is_cancelled() {
                    return;
                }

                match connect_async(&self.url).await {
                    Ok((ws_stream, _)) => {
                        attempt = 0;
                        let (mut write, mut read) = ws_stream.split();

                        if write.send(Message::Text(subscribe_msg.clone())).await.is_err() {
                            continue;
                        }

                        while !shutdown.is_cancelled() {
                            match read.next().await {
                                Some(Ok(Message::Text(text))) => {
                                    if let Ok(msg) = serde_json::from_str::<CoinbaseWsMsg>(&text) {
                                        if msg.msg_type.as_deref() != Some("match") {
                                            continue;
                                        }
                                        let Some(pid) = msg.product_id.as_deref() else { continue };
                                        let Some(asset) = asset_from_product_id(pid) else { continue };
                                        let Some(price_s) = msg.price.as_deref() else { continue };
                                        let Some(size_s) = msg.size.as_deref() else { continue };

                                        let (Some(price), Some(size)) = (parse_decimal(price_s), parse_decimal(size_s)) else { continue };
                                        let quote_volume = price * size;
                                        let (taker_buy_quote, taker_sell_quote) =
                                            match msg.side.as_deref() {
                                                Some("sell") => (quote_volume, Decimal::ZERO),
                                                Some("buy") => (Decimal::ZERO, quote_volume),
                                                _ => (Decimal::ZERO, Decimal::ZERO),
                                            };

                                        let now_ms = crate::cex::now_ms();
                                        let sample = SpotSample {
                                            ts_ms: now_ms,
                                            price,
                                            quote_volume,
                                            taker_buy_quote,
                                            taker_sell_quote,
                                        };

                                        let history_state = history.state_for(asset);
                                        let mut guard = history_state.write().await;
                                        guard.push(sample);
                                    }
                                }
                                Some(Ok(Message::Close(_))) => break,
                                Some(Ok(_)) => {}
                                Some(Err(_)) => break,
                                None => break,
                            }
                        }
                    }
                    Err(e) => {
                        attempt = attempt.saturating_add(1);
                        let delay = backoff_base
                            .mul_f64(2f64.powi(attempt.min(6) as i32) as f64)
                            .min(Duration::from_secs(30))
                            ;
                        tracing::warn!(
                            error = %e,
                            attempt = attempt,
                            "Coinbase WS connect failed; reconnecting"
                        );
                        sleep(delay).await;
                    }
                }
            }
        })
    }
}

