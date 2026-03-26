use crate::types::{Asset, SpotAssetState};
use anyhow::Result;
use reqwest::Client;
use rust_decimal::Decimal;
use std::sync::Arc;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

pub mod binance;
pub mod coinbase;
pub mod multi_reference;

/// Precios de referencia por venue al inicio de franja + media usada como ancla (`binance_5m_open`).
#[derive(Debug, Clone)]
pub struct BtcVenueAnchors {
    pub binance_usdt: Option<Decimal>,
    pub coinbase_usd: Option<Decimal>,
    pub kraken_usd: Option<Decimal>,
    pub bybit_usdt: Option<Decimal>,
    pub okx_usdt: Option<Decimal>,
    pub bitfinex_usd: Option<Decimal>,
    pub bitstamp_usd: Option<Decimal>,
    /// Media de los venues que respondieron.
    pub anchor: Decimal,
    /// Cuántos venues devolvieron precio (0–7).
    pub venues_ok: u32,
}

/// Shared per-asset state updated by CEX WS tasks.
#[derive(Debug, Clone)]
pub struct SpotHistorySet {
    pub states: [Arc<RwLock<SpotAssetState>>; crate::types::ASSET_COUNT],
}

impl SpotHistorySet {
    pub fn new(capacity_per_asset: usize) -> Self {
        let make = || Arc::new(RwLock::new(SpotAssetState::new(capacity_per_asset)));
        Self {
            states: [make(), make(), make()],
        }
    }

    pub fn state_for(&self, asset: Asset) -> Arc<RwLock<SpotAssetState>> {
        self.states[asset.idx()].clone()
    }
}

/// Generic interface for a CEX spot feed task.
pub trait SpotFeed: Send + 'static {
    /// Start the feed task. It must update `SpotHistorySet` until `shutdown` is cancelled.
    fn spawn(self, shutdown: CancellationToken, history: SpotHistorySet, assets: Vec<Asset>) -> JoinHandle<()>;
}

/// Helper: decide if a given history is considered fresh.
pub fn is_fresh(last_update_ms: u64, now_ms: u64, max_staleness_ms: u64) -> bool {
    now_ms.saturating_sub(last_update_ms) <= max_staleness_ms
}

/// Helper to convert unix ms.
pub fn now_ms() -> u64 {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    ms as u64
}

/// Referencia al inicio de franja: **Binance** (primer agg ≥ `t0`), **Coinbase** (trade ≥ `t0`, fallback vela 5m/ticker),
/// **Kraken / Bybit / OKX / Bitfinex / Bitstamp** (open vela **5m** REST en el bucket UTC de `interval_start`).
/// La ancla es la **media** de los venues que respondieron.
pub async fn fetch_multi_venue_btc_anchor(
    http: &Client,
    interval_start_unix_sec: u64,
) -> Result<BtcVenueAnchors> {
    let (
        r_bn,
        r_cb,
        r_kr,
        r_by,
        r_ok,
        r_bf,
        r_bs,
    ) = tokio::join!(
        crate::cex::binance::fetch_binance_first_agg_price_from_interval_start(
            http,
            "BTCUSDT",
            interval_start_unix_sec,
        ),
        crate::cex::coinbase::fetch_btc_usd_first_trade_from_interval_start(
            http,
            interval_start_unix_sec,
        ),
        crate::cex::multi_reference::fetch_kraken_xbtusd_5m_open(http, interval_start_unix_sec),
        crate::cex::multi_reference::fetch_bybit_btcusdt_5m_open(http, interval_start_unix_sec),
        crate::cex::multi_reference::fetch_okx_btcusdt_5m_open(http, interval_start_unix_sec),
        crate::cex::multi_reference::fetch_bitfinex_btcusd_5m_open(http, interval_start_unix_sec),
        crate::cex::multi_reference::fetch_bitstamp_btcusd_5m_open(http, interval_start_unix_sec),
    );

    if let Err(ref e) = r_bn {
        tracing::warn!(error = %e, venue = "binance", "referencia apertura franja falló");
    }
    if let Err(ref e) = r_cb {
        tracing::warn!(error = %e, venue = "coinbase", "referencia apertura franja falló");
    }
    for (name, res) in [
        ("kraken", &r_kr),
        ("bybit", &r_by),
        ("okx", &r_ok),
        ("bitfinex", &r_bf),
        ("bitstamp", &r_bs),
    ] {
        if let Err(e) = res {
            tracing::trace!(error = %e, venue = name, "referencia 5m REST falló");
        }
    }

    let binance_usdt = r_bn.ok();
    let coinbase_usd = r_cb.ok();
    let kraken_usd = r_kr.ok();
    let bybit_usdt = r_by.ok();
    let okx_usdt = r_ok.ok();
    let bitfinex_usd = r_bf.ok();
    let bitstamp_usd = r_bs.ok();

    let mut sum = Decimal::ZERO;
    let mut n: u32 = 0;
    for opt in [
        &binance_usdt,
        &coinbase_usd,
        &kraken_usd,
        &bybit_usdt,
        &okx_usdt,
        &bitfinex_usd,
        &bitstamp_usd,
    ] {
        if let Some(d) = opt {
            sum += *d;
            n += 1;
        }
    }

    if n == 0 {
        anyhow::bail!("ningún venue devolvió precio para la franja");
    }

    let anchor = sum / Decimal::from(n);

    Ok(BtcVenueAnchors {
        binance_usdt,
        coinbase_usd,
        kraken_usd,
        bybit_usdt,
        okx_usdt,
        bitfinex_usd,
        bitstamp_usd,
        anchor,
        venues_ok: n,
    })
}

