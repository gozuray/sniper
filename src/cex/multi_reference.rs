//! Referencias spot BTC en más exchanges (REST). Para venues distintos de Binance/Coinbase usamos el **open**
//! de la vela **5m** cuyo bucket UTC contiene `interval_start_unix` (misma rejilla que franjas Polymarket 5m).

use anyhow::{Context, Result};
use reqwest::Client;
use rust_decimal::Decimal;
use serde_json::Value;
use std::str::FromStr;
use std::time::Duration;

pub fn bucket_5m_unix_sec(interval_start_sec: u64) -> u64 {
    (interval_start_sec / 300) * 300
}

fn decimal_from_json(v: &Value) -> Result<Decimal> {
    match v {
        Value::String(s) => Decimal::from_str(s.trim()).context("decimal desde string"),
        Value::Number(n) => Decimal::from_str(&n.to_string()).context("decimal desde number"),
        _ => anyhow::bail!("JSON no es precio"),
    }
}

/// Timestamp en ms desde distintos JSON (string / entero / f64).
fn json_ts_ms(v: &Value) -> Option<u64> {
    if let Some(s) = v.as_str() {
        return s.parse().ok();
    }
    if let Some(u) = v.as_u64() {
        return Some(u);
    }
    if let Some(i) = v.as_i64() {
        return Some(i as u64);
    }
    v.as_f64().map(|f| f as u64)
}

fn okx_response_ok(v: &Value) -> bool {
    match v.get("code") {
        Some(Value::String(s)) => s == "0",
        Some(Value::Number(n)) => n.as_i64() == Some(0) || n.as_u64() == Some(0),
        _ => false,
    }
}

fn bybit_response_ok(v: &Value) -> bool {
    match v.get("retCode") {
        Some(Value::Number(n)) => n.as_i64() == Some(0) || n.as_u64() == Some(0),
        Some(Value::String(s)) => s == "0",
        _ => false,
    }
}

fn http_ua() -> &'static str {
    "sniper/0.2 (+https://github.com; spot-ref)"
}

/// Reintento corto: justo al cambiar de intervalo varias APIs aún no publican la vela 5m nueva.
async fn retry_open_not_ready<F, Fut>(label: &'static str, attempts: u32, delay: Duration, mut fetch: F) -> Result<Decimal>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<Decimal>>,
{
    let mut last = anyhow::anyhow!("{label}: sin intentos");
    for i in 0..attempts {
        if i > 0 {
            tokio::time::sleep(delay).await;
            tracing::trace!(target: "sniper", venue = label, attempt = i + 1, "reintento vela 5m REST");
        }
        match fetch().await {
            Ok(p) => return Ok(p),
            Err(e) => last = e,
        }
    }
    Err(last)
}

/// Kraken `XBTUSD`, vela 5m OHLC.
pub async fn fetch_kraken_xbtusd_5m_open(http: &Client, interval_start_sec: u64) -> Result<Decimal> {
    let bucket = bucket_5m_unix_sec(interval_start_sec);
    let url = "https://api.kraken.com/0/public/OHLC?pair=XBTUSD&interval=5";
    let v: Value = http
        .get(url)
        .header("Accept", "application/json")
        .send()
        .await
        .context("Kraken GET OHLC")?
        .json()
        .await
        .context("Kraken OHLC JSON")?;

    if let Some(err) = v.get("error").and_then(|e| e.as_array()) {
        if !err.is_empty() {
            anyhow::bail!("Kraken API error: {err:?}");
        }
    }

    let result = v
        .get("result")
        .and_then(|r| r.as_object())
        .context("Kraken: sin result")?;
    for val in result.values() {
        let Some(rows) = val.as_array() else {
            continue;
        };
        for row in rows {
            let r = row.as_array().context("Kraken: fila OHLC")?;
            let t_sec = r
                .first()
                .and_then(|x| x.as_u64().or_else(|| x.as_i64().map(|i| i as u64)))
                .context("Kraken: tiempo vela")?;
            if t_sec == bucket {
                return decimal_from_json(r.get(1).context("Kraken: open")?);
            }
        }
    }
    anyhow::bail!("Kraken: no hay vela 5m para bucket {bucket}")
}

/// Bybit spot `BTCUSDT`, kline 5m.
pub async fn fetch_bybit_btcusdt_5m_open(http: &Client, interval_start_sec: u64) -> Result<Decimal> {
    let bucket_ms = bucket_5m_unix_sec(interval_start_sec).saturating_mul(1000);
    let url = format!(
        "https://api.bybit.com/v5/market/kline?category=spot&symbol=BTCUSDT&interval=5&start={}&limit=15",
        bucket_ms
    );
    let v: Value = http
        .get(&url)
        .header("Accept", "application/json")
        .send()
        .await
        .context("Bybit GET kline")?
        .json()
        .await
        .context("Bybit kline JSON")?;

    if !bybit_response_ok(&v) {
        let msg = v
            .get("retMsg")
            .and_then(|x| x.as_str())
            .unwrap_or("?");
        let rc = v.get("retCode").unwrap_or(&Value::Null);
        anyhow::bail!("Bybit retCode inválido: {rc}, msg={msg}");
    }

    let list = v
        .pointer("/result/list")
        .and_then(|x| x.as_array())
        .context("Bybit: sin list")?;

    for item in list {
        match item {
            Value::String(line) => {
                let mut parts = line.split_whitespace();
                let t_ms: u64 = parts
                    .next()
                    .context("Bybit: falta ts")?
                    .parse()
                    .context("Bybit: ts parse")?;
                if t_ms == bucket_ms {
                    let open_s = parts.next().context("Bybit: falta open")?;
                    return Decimal::from_str(open_s.trim()).context("Bybit open decimal");
                }
            }
            Value::Array(r) => {
                let t_ms = r
                    .first()
                    .and_then(json_ts_ms)
                    .context("Bybit: ts vela")?;
                if t_ms == bucket_ms {
                    let open_v = r.get(1).context("Bybit: open")?;
                    return decimal_from_json(open_v);
                }
            }
            _ => continue,
        }
    }

    for item in list {
        if let Value::Array(r) = item {
            let ts_ms = r.first().and_then(json_ts_ms).unwrap_or(0);
            let end_ms = ts_ms.saturating_add(300_000);
            if bucket_ms >= ts_ms && bucket_ms < end_ms {
                return decimal_from_json(r.get(1).context("Bybit: open (rango)")?);
            }
        } else if let Value::String(line) = item {
            let mut parts = line.split_whitespace();
            let Some(ts_s) = parts.next() else {
                continue;
            };
            let Ok(ts_ms) = ts_s.parse::<u64>() else {
                continue;
            };
            let end_ms = ts_ms.saturating_add(300_000);
            if bucket_ms >= ts_ms && bucket_ms < end_ms {
                let open_s = parts.next().context("Bybit: open rango")?;
                return Decimal::from_str(open_s.trim()).context("Bybit open");
            }
        }
    }

    anyhow::bail!("Bybit: no kline 5m para bucket_ms {bucket_ms}")
}

async fn okx_5m_open_once(http: &Client, interval_start_sec: u64) -> Result<Decimal> {
    let bucket_ms = bucket_5m_unix_sec(interval_start_sec).saturating_mul(1000);
    let url = "https://www.okx.com/api/v5/market/candles?instId=BTC-USDT&bar=5m&limit=100";
    let v: Value = http
        .get(url)
        .header("Accept", "application/json")
        .header("User-Agent", http_ua())
        .send()
        .await
        .context("OKX GET candles")?
        .json()
        .await
        .context("OKX candles JSON")?;

    if !okx_response_ok(&v) {
        let msg = v.get("msg").and_then(|x| x.as_str()).unwrap_or("?");
        let code = v.get("code").unwrap_or(&Value::Null);
        anyhow::bail!("OKX code inválido: {code}, msg={msg}");
    }

    let data = v
        .get("data")
        .and_then(|x| x.as_array())
        .context("OKX: sin data")?;

    if data.is_empty() {
        anyhow::bail!("OKX: data vacío (vela nueva no lista aún)");
    }

    for row in data {
        let r = row.as_array().context("OKX: fila")?;
        let ts_ms = r
            .first()
            .and_then(json_ts_ms)
            .context("OKX: ts")?;
        if ts_ms == bucket_ms {
            return decimal_from_json(r.get(1).context("OKX: open")?);
        }
    }

    for row in data {
        let r = row.as_array().context("OKX: fila rango")?;
        let ts_ms = r.first().and_then(json_ts_ms).context("OKX: ts rango")?;
        let end_ms = ts_ms.saturating_add(300_000);
        if bucket_ms >= ts_ms && bucket_ms < end_ms {
            return decimal_from_json(r.get(1).context("OKX: open rango")?);
        }
    }

    let newest = data
        .first()
        .and_then(|row| row.as_array())
        .and_then(|r| r.first())
        .and_then(json_ts_ms)
        .unwrap_or(0);
    if newest < bucket_ms && bucket_ms.saturating_sub(newest) <= 300_000 {
        anyhow::bail!("OKX: feed aún en vela anterior (newest={newest}); reintento");
    }

    anyhow::bail!("OKX: no vela 5m para bucket_ms {bucket_ms}")
}

/// OKX `BTC-USDT`, velas 5m (reintentos al cambio de intervalo).
pub async fn fetch_okx_btcusdt_5m_open(http: &Client, interval_start_sec: u64) -> Result<Decimal> {
    retry_open_not_ready("okx", 4, Duration::from_millis(320), || {
        okx_5m_open_once(http, interval_start_sec)
    })
    .await
}

async fn bitfinex_5m_open_once(http: &Client, interval_start_sec: u64) -> Result<Decimal> {
    let bucket_ms = bucket_5m_unix_sec(interval_start_sec).saturating_mul(1000);
    let end_ms = bucket_ms.saturating_add(300_000);
    let url = format!(
        "https://api-pub.bitfinex.com/v2/candles/trade:5m:tBTCUSD/hist?start={}&end={}&limit=10&sort=1",
        bucket_ms, end_ms
    );
    let rows: Vec<Value> = http
        .get(&url)
        .header("Accept", "application/json")
        .header("User-Agent", http_ua())
        .send()
        .await
        .context("Bitfinex GET candles")?
        .json()
        .await
        .context("Bitfinex candles JSON")?;

    if rows.is_empty() {
        anyhow::bail!("Bitfinex: hist vacío (vela nueva no lista aún)");
    }

    for row in &rows {
        let r = row.as_array().context("Bitfinex: fila")?;
        let mts = r
            .first()
            .and_then(json_ts_ms)
            .context("Bitfinex: mts")?;
        if mts == bucket_ms {
            return decimal_from_json(r.get(1).context("Bitfinex: open")?);
        }
    }

    for row in &rows {
        let r = row.as_array().context("Bitfinex: fila")?;
        let mts = r.first().and_then(json_ts_ms).context("Bitfinex: mts")?;
        let end_c = mts.saturating_add(300_000);
        if bucket_ms >= mts && bucket_ms < end_c {
            return decimal_from_json(r.get(1).context("Bitfinex: open")?);
        }
    }

    anyhow::bail!("Bitfinex: no vela 5m para mts {bucket_ms}")
}

/// Bitfinex `tBTCUSD`, velas trade 5m (reintentos al cambio de intervalo).
pub async fn fetch_bitfinex_btcusd_5m_open(http: &Client, interval_start_sec: u64) -> Result<Decimal> {
    retry_open_not_ready("bitfinex", 4, Duration::from_millis(320), || {
        bitfinex_5m_open_once(http, interval_start_sec)
    })
    .await
}

/// Bitstamp `BTC/USD`, OHLC step 300s.
pub async fn fetch_bitstamp_btcusd_5m_open(http: &Client, interval_start_sec: u64) -> Result<Decimal> {
    let bucket = bucket_5m_unix_sec(interval_start_sec);
    let start = bucket.saturating_sub(600);
    let url = format!(
        "https://www.bitstamp.net/api/v2/ohlc/btcusd/?step=300&limit=24&start={}",
        start
    );
    let v: Value = http
        .get(&url)
        .header("Accept", "application/json")
        .send()
        .await
        .context("Bitstamp GET ohlc")?
        .json()
        .await
        .context("Bitstamp ohlc JSON")?;

    let ohlc = v
        .pointer("/data/ohlc")
        .and_then(|x| x.as_array())
        .context("Bitstamp: sin data.ohlc")?;

    for o in ohlc {
        let ts: u64 = o
            .get("timestamp")
            .and_then(|x| x.as_str())
            .context("Bitstamp: timestamp")?
            .parse()
            .context("Bitstamp: ts")?;
        if ts == bucket {
            let open_s = o.get("open").and_then(|x| x.as_str()).context("Bitstamp: open")?;
            return Decimal::from_str(open_s.trim()).context("Bitstamp open parse");
        }
    }
    anyhow::bail!("Bitstamp: no vela 5m para bucket {bucket}")
}
