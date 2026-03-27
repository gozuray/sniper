mod config;
mod config_writeback;
mod cex;
mod execution;
mod paper_lab;
mod polymarket;
mod rl;
mod strategy;
mod types;
mod utils;

use crate::config::{parse_asset_symbol, CexMomentumPolicy, Config, Mode};
use crate::paper_lab::{should_run_adaptive, PaperLab};
use crate::cex::{is_fresh, now_ms, SpotHistorySet, SpotFeed};
use crate::execution::order_manager::{OrderManager, TradeFillEvent};
use crate::polymarket::client::{spawn_orderbook_ws, spawn_user_trade_ws, LivePolymarket, OrderbookTop};
use crate::polymarket::markets::{
    active_btc_5m_market, gamma_discover_open_btc_5m_interval_start_unix, is_market_open,
    polymarket_5m_interval_start_unix_et, resolve_active_markets, slug_for,
};
use crate::strategy::momentum::{
    compute_momentum_from_binance_5m_anchor, compute_momentum_snapshot, evaluate_arb_both,
    evaluate_market_signal, merge_consensus_momentum, momentum_edge_vs_asks,
};
use crate::types::{
    Asset, ASSET_COUNT, Interval, MarketKey, MomentumSnapshot, Outcome, ResolvedMarket, Signal,
    SpotIntervalState,
};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;

use anyhow::Context;
use chrono::TimeZone;
use chrono::Utc;
use chrono_tz::America::New_York;
use reqwest::Client;
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::fs::OpenOptions;
use std::io::{self, IsTerminal, Write};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::MutexGuard;
use tokio::sync::mpsc;
use tokio::time::{sleep, Duration, MissedTickBehavior};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::fmt::writer::MakeWriter;
use tracing_subscriber::prelude::*;

/// rustls 0.23+ needs exactly one process-wide `CryptoProvider`. Install `ring` before any HTTPS/WSS.
fn install_rustls_crypto_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        rustls::crypto::ring::default_provider()
            .install_default()
            .expect("install rustls ring CryptoProvider");
    });
}

/// Una sola capa `fmt`: mismo formato a consola y archivo. `tracing` llama a `make_writer` una vez
/// por evento; el `MutexGuard` vive en el `Writer` hasta cerrar el registro, así no se entremezclan
/// líneas entre hilos (típico en Consola/ConPTY con varias tareas Tokio).
#[derive(Clone)]
struct LockedStdoutAndFile {
    file: Arc<Mutex<File>>,
    serial: Arc<Mutex<()>>,
}

impl LockedStdoutAndFile {
    fn new(file: File) -> Self {
        Self {
            file: Arc::new(Mutex::new(file)),
            serial: Arc::new(Mutex::new(())),
        }
    }
}

impl<'a> MakeWriter<'a> for LockedStdoutAndFile {
    type Writer = LockedStdoutAndFileWriter<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        LockedStdoutAndFileWriter {
            file: Arc::clone(&self.file),
            _serial: self.serial.lock().expect("log serial mutex poisoned"),
        }
    }
}

struct LockedStdoutAndFileWriter<'a> {
    file: Arc<Mutex<File>>,
    _serial: MutexGuard<'a, ()>,
}

impl Write for LockedStdoutAndFileWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = buf.len();
        let mut out = io::stdout().lock();
        out.write_all(buf)?;
        out.flush()?;
        let mut f = self.file.lock().expect("log file mutex poisoned");
        f.write_all(buf)?;
        f.flush()?;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut out = io::stdout().lock();
        out.flush()?;
        self.file.lock().expect("log file mutex poisoned").flush()
    }
}

/// Sin `RUST_LOG`: solo mensajes útiles del crate `sniper` (`info`+) y `warn`+ del resto (menos ruido de dependencias).
/// Diagnóstico momentum/consenso/Gamma: `RUST_LOG=sniper::strategy::momentum=trace` o `RUST_LOG=sniper=trace`.
const DEFAULT_RUST_LOG: &str = "warn,sniper=info";

fn init_tracing(log_path: &str) -> anyhow::Result<()> {
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path)?;

    let writer = LockedStdoutAndFile::new(file);
    let ansi_stdout = io::stdout().is_terminal();

    let filter = match std::env::var("RUST_LOG") {
        Ok(s) if !s.trim().is_empty() => tracing_subscriber::EnvFilter::try_new(s.trim())
            .unwrap_or_else(|e| {
                eprintln!("RUST_LOG inválido ({e}); uso {DEFAULT_RUST_LOG}");
                tracing_subscriber::EnvFilter::new(DEFAULT_RUST_LOG)
            }),
        _ => tracing_subscriber::EnvFilter::new(DEFAULT_RUST_LOG),
    };

    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .compact()
                .with_target(false)
                .with_thread_ids(false)
                .with_thread_names(false)
                .with_ansi(ansi_stdout)
                .with_writer(writer),
        )
        .init();

    Ok(())
}

/// Periodic momentum diagnostics: CEX snapshot per asset and best Polymarket edge vs `edge_min`.
fn log_momentum_diag(
    assets: &[Asset],
    momentum_by_asset: &[Option<MomentumSnapshot>; ASSET_COUNT],
    resolved_markets: &[ResolvedMarket],
    only_active: bool,
    now_sec: u64,
    book: &OrderbookTop,
    edge_min: Decimal,
) {
    use crate::polymarket::markets::is_market_open;

    if !momentum_by_asset.iter().any(|m| m.is_some()) {
        return;
    }

    let mut parts: Vec<String> = Vec::new();
    for asset in assets {
        let idx = asset.idx();
        let mom = match &momentum_by_asset[idx] {
            Some(m) => m,
            None => continue,
        };
        let dir = match mom.direction {
            Outcome::Up => "Up",
            Outcome::Down => "Down",
        };
        let anchor_s = if mom.anchor_interval_start_unix == 0 {
            ""
        } else {
            " · Binance 5m open"
        };
        let pct = mom.pct_change * 100.0;
        let mut best: Option<(Decimal, &str)> = None;
        for m in resolved_markets {
            if m.key.asset != *asset {
                continue;
            }
            if only_active && !is_market_open(m, now_sec) {
                continue;
            }
            let (p_up, _) = match book.best_ask(m.token_id_up) {
                Some(v) => v,
                None => continue,
            };
            let (p_down, _) = match book.best_ask(m.token_id_down) {
                Some(v) => v,
                None => continue,
            };
            let e = momentum_edge_vs_asks(mom, p_up, p_down);
            if best.map_or(true, |(eb, _)| e > eb) {
                best = Some((e, m.slug.as_str()));
            }
        }
        let best_s = match best {
            Some((e, slug)) => format!("best {slug} edge={e} (need ≥ {edge_min})"),
            None => "no open book".to_string(),
        };
        parts.push(format!(
            "{asset:?} Δ{pct:+.3}% fair_up={:.3} {dir}{anchor_s} · {best_s}",
            mom.fair_prob_up
        ));
    }
    tracing::info!(target: "sniper", "mom · {}", parts.join(" │ "));
}

/// HH:MM–HH:MM en UTC y en US/Eastern (columnas alineadas en logs).
fn polymarket_slot_times_short(start_unix: u64, end_unix: u64) -> Option<(String, String)> {
    let s = Utc.timestamp_opt(start_unix as i64, 0).single()?;
    let e = Utc.timestamp_opt(end_unix as i64, 0).single()?;
    let utc = format!("{}–{} UTC", s.format("%H:%M"), e.format("%H:%M"));
    let s_et = s.with_timezone(&New_York);
    let e_et = e.with_timezone(&New_York);
    let et = format!("{}–{} ET", s_et.format("%H:%M"), e_et.format("%H:%M"));
    Some((utc, et))
}

/// Un solo bloque por franja: ventana Polymarket + libro WS + referencia CEX (misma plantilla en `arranque` y `ventana_nueva`).
async fn log_btc_5m_window_snapshot(
    phase: &str,
    m: &ResolvedMarket,
    book: &OrderbookTop,
    http: &Client,
    binance_hist: &SpotHistorySet,
    fetch_cex_anchor: bool,
    horizon_markets: usize,
    ws_token_count: usize,
) {
    let next_slug = slug_for(Asset::BTC, Interval::M5, m.close_time_unix);
    let next_end = m.close_time_unix.saturating_add(Interval::M5.sec());
    let (utc_a, et_a) = polymarket_slot_times_short(m.interval_start_unix, m.close_time_unix)
        .unwrap_or_else(|| ("—".to_string(), "—".to_string()));
    let (utc_n, et_n) =
        polymarket_slot_times_short(m.close_time_unix, next_end).unwrap_or_else(|| {
            ("—".to_string(), "—".to_string())
        });
    let libro_ok = book.tracks_token(m.token_id_up) && book.tracks_token(m.token_id_down);
    let libro = if libro_ok {
        "up+dn · ok"
    } else {
        "up+dn · falta token"
    };

    let fmt_px = |o: Option<Decimal>| {
        o.map(|p| format!("{}", p.round_dp(2)))
            .unwrap_or_else(|| "—".to_string())
    };
    let (bin_s, cb_s, kr_s, by_s, ok_s, bf_s, bs_s, precio_s, fuente_s) = if fetch_cex_anchor {
        match crate::cex::fetch_multi_venue_btc_anchor(http, m.interval_start_unix).await {
            Ok(a) => {
                let st = binance_hist.state_for(Asset::BTC);
                let mut g = st.write().await;
                let spot_ws = g.last_price;
                g.binance_5m_open = a.anchor;
                g.binance_5m_open_ms = m.interval_start_unix.saturating_mul(1000);
                g.binance_5m_kline_event_ms = now_ms();

                if !spot_ws.is_zero()
                    && !a.anchor.is_zero()
                    && (spot_ws - a.anchor).abs() > Decimal::new(100, 0)
                {
                    tracing::warn!(
                        target: "sniper",
                        phase,
                        interval_start_unix = m.interval_start_unix,
                        anchor_ref = %a.anchor.round_dp(2),
                        binance_spot_ws = %spot_ws.round_dp(2),
                        drift_usd = %(spot_ws - a.anchor).abs().round_dp(2),
                        "BTC 5m · REF ancla vs spot Binance (WS) > $100 — posible ancla mal seteada; revisar pct_vs_anchor y umbrales delta USD"
                    );
                }

                let fuente = format!(
                    "promedio {}/7 · BN+CB ≥t0; resto open 5m",
                    a.venues_ok
                );
                (
                    fmt_px(a.binance_usdt),
                    fmt_px(a.coinbase_usd),
                    fmt_px(a.kraken_usd),
                    fmt_px(a.bybit_usdt),
                    fmt_px(a.okx_usdt),
                    fmt_px(a.bitfinex_usd),
                    fmt_px(a.bitstamp_usd),
                    format!("{}", a.anchor.round_dp(2)),
                    fuente,
                )
            }
            Err(e) => {
                tracing::warn!(
                    target: "sniper",
                    error = %e,
                    phase,
                    interval_start_unix = m.interval_start_unix,
                    "BTC 5m · referencia CEX no disponible"
                );
                (
                    "—".to_string(),
                    "—".to_string(),
                    "—".to_string(),
                    "—".to_string(),
                    "—".to_string(),
                    "—".to_string(),
                    "—".to_string(),
                    "—".to_string(),
                    "error REST".to_string(),
                )
            }
        }
    } else {
        (
            "—".to_string(),
            "—".to_string(),
            "—".to_string(),
            "—".to_string(),
            "—".to_string(),
            "—".to_string(),
            "—".to_string(),
            "—".to_string(),
            "sin CEX".to_string(),
        )
    };

    // Etiquetas columna 1 alineadas (misma anchura en todas las filas).
    let msg = format!(
        "BTC 5m · {phase}\n\
  {:<14} {:>16}  {:>16}  {}\n\
  {:<14} {:>16}  {:>16}  {}\n\
  {:<14} {}\n\
  {:<14} {} mercados · {} tokens WS · {}\n\
  {:<14} {}\n\
  {:<14} {}\n\
  {:<14} {}\n\
  {:<14} {}\n\
  {:<14} {}\n\
  {:<14} {}\n\
  {:<14} {}\n\
  {:<14} {} USD · {}",
        "activo",
        utc_a,
        et_a,
        m.slug,
        "siguiente",
        utc_n,
        et_n,
        next_slug,
        "interval_unix",
        m.interval_start_unix,
        "horizonte",
        horizon_markets,
        ws_token_count,
        libro,
        "binance",
        bin_s,
        "coinbase",
        cb_s,
        "kraken",
        kr_s,
        "bybit",
        by_s,
        "okx",
        ok_s,
        "bitfinex",
        bf_s,
        "bitstamp",
        bs_s,
        "precio_ref",
        precio_s,
        fuente_s,
    );
    let line = format!(
        "═══ BTC 5m · {phase} · {utc_a} ({et_a}) · ref ${precio_s} · {fuente_s} · {libro} · {}",
        m.slug
    );
    tracing::info!(target: "sniper", "{}", line);
    tracing::trace!(target: "sniper", "{}", msg);

    if !libro_ok {
        tracing::warn!(
            target: "sniper",
            phase,
            "BTC 5m · token UP/DOWN no está en el índice del libro WS"
        );
    }
}

/// Si el mercado BTC 5m activo cambió (lista Gamma `btc-updown-5m` + ventana `[ts, ts+300)`), re-resuelve
/// slugs y reconecta el WS. `btc_5m_anchor_unix` es el sufijo del slug de la web (prioriza Gamma, si no ET local).
#[allow(clippy::too_many_arguments)]
async fn sync_polymarket_5m_interval_if_needed(
    http: &reqwest::Client,
    gamma_base_url: &str,
    assets: &[Asset],
    intervals: &[Interval],
    horizon_intervals: u32,
    last_pm_et_slot: &mut Option<u64>,
    btc_5m_anchor_unix: &mut u64,
    resolved_markets: &mut Vec<ResolvedMarket>,
    orderbook_ws_shutdown: &mut CancellationToken,
    orderbook_state: &Arc<tokio::sync::RwLock<OrderbookTop>>,
    orderbook_ws_handle: &mut tokio::task::JoinHandle<()>,
    cached_ws_token_count: &mut usize,
    last_logged_btc_5m_start: &mut Option<u64>,
) -> anyhow::Result<()> {
    loop {
        let wall_sec = now_ms() / 1000;
        let local_slot = polymarket_5m_interval_start_unix_et(wall_sec);
        let gamma_slot = match gamma_discover_open_btc_5m_interval_start_unix(http, gamma_base_url, wall_sec).await {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    target: "sniper",
                    error = %e,
                    local_slot,
                    "Gamma: no se pudo listar btc-updown-5m activos; ancla solo fórmula ET"
                );
                None
            }
        };
        let slot = gamma_slot.unwrap_or(local_slot);
        *btc_5m_anchor_unix = slot;

        if let Some(g) = gamma_slot {
            if g != local_slot {
                tracing::info!(
                    target: "sniper",
                    gamma_interval_start_unix = g,
                    local_et_interval_start_unix = local_slot,
                    "BTC 5m: intervalo en lista Gamma ≠ fórmula ET; se usa Gamma"
                );
            }
        }

        if *last_pm_et_slot == Some(slot) {
            return Ok(());
        }
        *last_pm_et_slot = Some(slot);
        *resolved_markets = resolve_active_markets(
            http,
            gamma_base_url,
            assets,
            intervals,
            horizon_intervals,
            wall_sec,
            Some(slot),
        )
        .await
        .context("resolve active polymarket markets on refresh")?;

        tracing::info!(
            target: "sniper",
            mercados = resolved_markets.len(),
            anchor_source = if gamma_slot.is_some() { "gamma" } else { "et_local" },
            "Polymarket · BTC 5m · resync mercados · WS libro"
        );

        let mut new_token_ids: HashSet<crate::types::TokenId> = HashSet::new();
        for m in resolved_markets.iter() {
            new_token_ids.insert(m.token_id_up);
            new_token_ids.insert(m.token_id_down);
        }
        let new_token_ids_vec: Vec<_> = new_token_ids.iter().copied().collect();
        *cached_ws_token_count = new_token_ids_vec.len();

        orderbook_ws_shutdown.cancel();
        *orderbook_ws_shutdown = CancellationToken::new();
        *orderbook_ws_handle = spawn_orderbook_ws(
            orderbook_ws_shutdown.clone(),
            new_token_ids_vec.clone(),
            orderbook_state.clone(),
        );

        let mut guard = orderbook_state.write().await;
        *guard = OrderbookTop::new(&new_token_ids_vec);
        *last_logged_btc_5m_start = None;
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    install_rustls_crypto_provider();

    let cfg_path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "config.toml".to_string());
    let cfg = Arc::new(Config::load_from_toml(&cfg_path).with_context(|| {
        format!(
            "load config TOML from path: {cfg_path}\n\
             Hint: from the project folder run `cargo run --release -- config.toml` (or pass the real path to your TOML).\n\
             The string `path/to/config.toml` in the README is only a placeholder, not a file on disk."
        )
    })?);

    let log_path = std::env::var("SNIPER_LOG").unwrap_or_else(|_| "sniper.log".to_string());
    init_tracing(&log_path)?;

    let shutdown = CancellationToken::new();
    let shutdown_clone = shutdown.clone();
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        shutdown_clone.cancel();
    });

    let parsed_assets: Vec<Asset> = cfg
        .assets
        .clone()
        .unwrap_or_else(|| vec!["BTC".to_string()])
        .into_iter()
        .map(|s| parse_asset_symbol(&s))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let ignored: Vec<Asset> = parsed_assets
        .iter()
        .copied()
        .filter(|a| *a != Asset::BTC)
        .collect();
    if !ignored.is_empty() {
        tracing::warn!(
            target: "sniper",
            ?ignored,
            "solo Polymarket BTC 5m: ignorando otros activos del config"
        );
    }
    let assets = vec![Asset::BTC];
    // Solo mercados up/down de 5 minutos en Polymarket (no 15m).
    const POLYMARKET_INTERVALS: &[Interval] = &[Interval::M5];

    let mut last_logged_btc_5m_start: Option<u64> = None;

    let cap = (cfg.momentum.window_sec as usize)
        .saturating_mul(200)
        .max(256)
        .min(10_000);

    let binance_hist = SpotHistorySet::new(cap);
    let coinbase_hist = SpotHistorySet::new(cap);

    let http = Client::builder()
        .user_agent("hft-momentum-polymarket-rust")
        .build()?;

    // Spawn CEX feeds (always both in Auto; caller decides freshness per-asset).
    let mut feed_handles: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    let cex_cfg = cfg.cex_config();
    if matches!(cex_cfg.mode, crate::config::CexMode::Auto | crate::config::CexMode::BinanceOnly) {
        let handle = crate::cex::binance::BinanceAggTradeFeed::new().spawn(
            shutdown.clone(),
            binance_hist.clone(),
            assets.clone(),
        );
        feed_handles.push(handle);
    }
    if matches!(cex_cfg.mode, crate::config::CexMode::Auto | crate::config::CexMode::CoinbaseOnly) {
        let handle = crate::cex::coinbase::CoinbaseMatchesFeed::new().spawn(
            shutdown.clone(),
            coinbase_hist.clone(),
            assets.clone(),
        );
        feed_handles.push(handle);
    }

    let horizon = cfg.subscription_horizon_intervals.unwrap_or(2);
    cfg.log_startup_summary(&cfg_path, &log_path, &assets, horizon);

    let boot_clock_sec = now_ms() / 1000;
    let boot_gamma = gamma_discover_open_btc_5m_interval_start_unix(&http, cfg.gamma_base_url(), boot_clock_sec)
        .await
        .ok()
        .flatten();
    let boot_local = polymarket_5m_interval_start_unix_et(boot_clock_sec);
    let boot_anchor = boot_gamma.unwrap_or(boot_local);
    if let Some(g) = boot_gamma {
        if g != boot_local {
            tracing::info!(
                target: "sniper",
                gamma_interval_start_unix = g,
                local_et_interval_start_unix = boot_local,
                "Polymarket · arranque: ancla BTC 5m desde lista Gamma (difiere de ET local)"
            );
        }
    }
    let anchor_boot_msg = format!(
        "Polymarket · ancla BTC 5m (arranque)\n\
           boot_anchor_unix       {ba}\n\
           coincide_ET_local      {et}\n\
           gamma_confirma         {ga}",
        ba = boot_anchor,
        et = if boot_anchor == boot_local { "sí" } else { "no" },
        ga = if boot_gamma.is_some() { "sí" } else { "no" },
    );
    tracing::info!(target: "sniper", "{}", anchor_boot_msg);
    let mut btc_5m_anchor_unix = boot_anchor;
    let mut resolved_markets = resolve_active_markets(
        &http,
        cfg.gamma_base_url(),
        &assets,
        POLYMARKET_INTERVALS,
        horizon,
        boot_clock_sec,
        Some(boot_anchor),
    )
    .await
    .context("resolve active polymarket markets")?;

    let now_sec_boot = now_ms() / 1000;

    let mut token_ids: HashSet<crate::types::TokenId> = HashSet::new();
    for m in &resolved_markets {
        token_ids.insert(m.token_id_up);
        token_ids.insert(m.token_id_down);
    }
    let token_ids_vec: Vec<_> = token_ids.iter().copied().collect();
    let mut cached_ws_token_count = token_ids_vec.len();

    let orderbook_state = Arc::new(tokio::sync::RwLock::new(OrderbookTop::new(&token_ids_vec)));
    let mut orderbook_ws_shutdown = CancellationToken::new();
    let mut _orderbook_ws_handle = spawn_orderbook_ws(
        orderbook_ws_shutdown.clone(),
        token_ids_vec.clone(),
        orderbook_state.clone(),
    );

    let boot_active_m =
        active_btc_5m_market(&resolved_markets, now_sec_boot, Some(btc_5m_anchor_unix));
    if let Some(m) = boot_active_m {
        last_logged_btc_5m_start = Some(m.interval_start_unix);
        let fetch_cex = matches!(
            cex_cfg.mode,
            crate::config::CexMode::Auto | crate::config::CexMode::BinanceOnly
        );
        let book_boot = orderbook_state.read().await;
        log_btc_5m_window_snapshot(
            "arranque",
            m,
            &book_boot,
            &http,
            &binance_hist,
            fetch_cex,
            resolved_markets.len(),
            cached_ws_token_count,
        )
        .await;
    }

    let paper_lab: Option<Arc<PaperLab>> = if should_run_adaptive(&cfg.mode, &cfg.adaptive_paper) {
        let ap = cfg
            .adaptive_paper
            .as_ref()
            .expect("should_run_adaptive implies adaptive_paper")
            .clone();
        let wb_path = if ap.writeback_config {
            Some(std::path::PathBuf::from(&cfg_path))
        } else {
            None
        };
        let lab = PaperLab::new(ap, wb_path)?;
        lab.bootstrap_adaptive_from_config(&cfg.momentum, &cfg.trading);
        if let Some(m) = boot_active_m {
            lab.rotate_interval(None, m.interval_start_unix, &m.slug);
        }
        Some(lab)
    } else {
        None
    };

    // Spot interval open/close used for settlement P&L.
    let spot_intervals: Arc<tokio::sync::RwLock<HashMap<(MarketKey, u64), SpotIntervalState>>> =
        Arc::new(tokio::sync::RwLock::new(HashMap::new()));

    // Polymarket live client (optional, for paper mode we simulate fills).
    let live = if cfg.mode == Mode::Live {
        Some(Arc::new(LivePolymarket::connect(&cfg).await?))
    } else {
        None
    };

    let (signal_tx, signal_rx) = mpsc::unbounded_channel::<Signal>();
    let (trade_fill_tx, trade_fill_rx) = mpsc::unbounded_channel::<TradeFillEvent>();

    // Order manager.
    let om_shutdown = shutdown.clone();
    let order_manager_handle = OrderManager::spawn(
        cfg.clone(),
        cfg.mode.clone(),
        orderbook_state.clone(),
        live.clone(),
        signal_rx,
        trade_fill_rx,
        spot_intervals.clone(),
        paper_lab.clone(),
        om_shutdown,
    );

    // Live trading: forward Polymarket WS user trade messages -> order manager fill channel.
    if cfg.mode == Mode::Live {
        let live_ref = live
            .as_ref()
            .context("live mode requires LivePolymarket client")?;

        let (user_trade_tx, mut user_trade_rx) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Some(msg) = user_trade_rx.recv().await {
                let _ = trade_fill_tx.send(TradeFillEvent::from(msg));
            }
        });

        let credentials = live_ref.credentials().clone();
        let address = live_ref.address();
        spawn_user_trade_ws(
            shutdown.clone(),
            credentials,
            address,
            user_trade_tx,
        );
    }

    // Strategy loop.
    let tick_ms = cfg.trading.tick_ms.max(1);
    let mut tick = tokio::time::interval(Duration::from_millis(tick_ms));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

    let mut last_pm_et_slot: Option<u64> = Some(boot_anchor);
    let mut last_mom_diag_ms: u64 = 0;

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                tracing::info!(target: "sniper", "shutdown");
                break;
            }
            _ = tick.tick() => {
                sync_polymarket_5m_interval_if_needed(
                    &http,
                    cfg.gamma_base_url(),
                    &assets,
                    POLYMARKET_INTERVALS,
                    horizon,
                    &mut last_pm_et_slot,
                    &mut btc_5m_anchor_unix,
                    &mut resolved_markets,
                    &mut orderbook_ws_shutdown,
                    &orderbook_state,
                    &mut _orderbook_ws_handle,
                    &mut cached_ws_token_count,
                    &mut last_logged_btc_5m_start,
                )
                .await
                .context("sync polymarket 5m interval (start of tick)")?;

                let mut wall_ms = now_ms();
                let wall_sec = wall_ms / 1000;

                // Select freshest CEX source per asset for momentum + interval prices.
                let max_stale_ms = cex_cfg.max_feed_staleness_ms;
                    let mut momentum_by_asset: [Option<crate::types::MomentumSnapshot>; ASSET_COUNT] =
                    [None, None, None];
                let mut last_price_by_asset: [Option<crate::types::Price>; ASSET_COUNT] =
                    [None, None, None];

                let mom_cfg = match &paper_lab {
                    Some(lab) => lab.effective_momentum(&cfg.momentum),
                    None => cfg.momentum.clone(),
                };

                for asset in &assets {
                    let bin_state = binance_hist.state_for(*asset);
                    let cb_state = coinbase_hist.state_for(*asset);
                    let bin_guard = bin_state.read().await;
                    let cb_guard = cb_state.read().await;

                    let allows_bn = matches!(
                        cex_cfg.mode,
                        crate::config::CexMode::Auto | crate::config::CexMode::BinanceOnly
                    );
                    let allows_cb = matches!(
                        cex_cfg.mode,
                        crate::config::CexMode::Auto | crate::config::CexMode::CoinbaseOnly
                    );
                    let bin_fresh =
                        allows_bn && is_fresh(bin_guard.last_update_ms, wall_ms, max_stale_ms);
                    let cb_fresh =
                        allows_cb && is_fresh(cb_guard.last_update_ms, wall_ms, max_stale_ms);

                    let dual_consensus = cex_cfg.momentum_policy == CexMomentumPolicy::Consensus
                        && allows_bn
                        && allows_cb
                        && bin_fresh
                        && cb_fresh;

                    let idx = asset.idx();
                    if dual_consensus {
                        let mom_bn =
                            compute_momentum_from_binance_5m_anchor(&bin_guard, wall_ms, &mom_cfg);
                        let mom_cb = compute_momentum_snapshot(
                            &cb_guard,
                            wall_ms,
                            &mom_cfg,
                            btc_5m_anchor_unix,
                        );
                        let lp_mid =
                            (bin_guard.last_price + cb_guard.last_price) / Decimal::from(2);
                        let (mom, lp) = match (mom_bn, mom_cb) {
                            (Some(a), Some(b)) if a.direction == b.direction => {
                                (Some(merge_consensus_momentum(a, b)), Some(lp_mid))
                            }
                            (Some(a), Some(b)) => {
                                tracing::trace!(
                                    target: "sniper",
                                    "mom · ✗ consensus     bn={:?} {:+.4}%  cb={:?} {:+.4}%",
                                    a.direction,
                                    a.pct_change * 100.0,
                                    b.direction,
                                    b.pct_change * 100.0
                                );
                                (None, Some(lp_mid))
                            }
                            _ => (None, Some(lp_mid)),
                        };
                        momentum_by_asset[idx] = mom;
                        last_price_by_asset[idx] = lp;
                    } else if bin_fresh {
                        momentum_by_asset[idx] = compute_momentum_from_binance_5m_anchor(
                            &bin_guard,
                            wall_ms,
                            &mom_cfg,
                        );
                        last_price_by_asset[idx] = Some(bin_guard.last_price);
                    } else if cb_fresh {
                        momentum_by_asset[idx] = compute_momentum_snapshot(
                            &cb_guard,
                            wall_ms,
                            &mom_cfg,
                            btc_5m_anchor_unix,
                        );
                        last_price_by_asset[idx] = Some(cb_guard.last_price);
                    }
                }

                // Update spot interval open/close (used later for settlement).
                {
                    let mut guard = spot_intervals.write().await;
                    for m in &resolved_markets {
                        let key = (m.key, m.interval_start_unix);
                        let entry = guard
                            .entry(key)
                            .or_insert_with(|| SpotIntervalState::new(m.key, m.interval_start_unix, m.close_time_unix));

                        let asset_idx = m.key.asset.idx();
                        if wall_sec >= m.interval_start_unix && !entry.open_set {
                            if let Some(p) = last_price_by_asset[asset_idx] {
                                entry.open_price = p;
                                entry.open_set = true;
                            }
                        }
                        if wall_sec >= m.close_time_unix && !entry.close_set {
                            if let Some(p) = last_price_by_asset[asset_idx] {
                                entry.close_price = p;
                                entry.close_set = true;
                            }
                        }
                    }
                }

                sync_polymarket_5m_interval_if_needed(
                    &http,
                    cfg.gamma_base_url(),
                    &assets,
                    POLYMARKET_INTERVALS,
                    horizon,
                    &mut last_pm_et_slot,
                    &mut btc_5m_anchor_unix,
                    &mut resolved_markets,
                    &mut orderbook_ws_shutdown,
                    &orderbook_state,
                    &mut _orderbook_ws_handle,
                    &mut cached_ws_token_count,
                    &mut last_logged_btc_5m_start,
                )
                .await
                .context("sync polymarket 5m interval (before signals)")?;

                wall_ms = now_ms();
                let wall_sec = wall_ms / 1000;

                // Evaluate signals and push into order manager.
                let only_active = cfg.only_active_markets.unwrap_or(true);
                let book_guard = orderbook_state.read().await;

                // Book staleness guard at the strategy level.
                let book_fresh = {
                    let book_ts = book_guard.last_book_event_ts_ms();
                    if let Some(max_stale) = cfg.trading.max_book_staleness_ms {
                        book_ts > 0 && wall_ms.saturating_sub(book_ts as u64) <= max_stale
                    } else {
                        true
                    }
                };

                if let Some(m) = active_btc_5m_market(&resolved_markets, wall_sec, Some(btc_5m_anchor_unix)) {
                    if last_logged_btc_5m_start != Some(m.interval_start_unix) {
                        if let Some(lab) = paper_lab.as_ref() {
                            lab.rotate_interval(last_logged_btc_5m_start, m.interval_start_unix, &m.slug);
                        }
                        last_logged_btc_5m_start = Some(m.interval_start_unix);
                        let fetch_cex = matches!(
                            cex_cfg.mode,
                            crate::config::CexMode::Auto | crate::config::CexMode::BinanceOnly
                        );
                        log_btc_5m_window_snapshot(
                            "ventana_nueva",
                            m,
                            &book_guard,
                            &http,
                            &binance_hist,
                            fetch_cex,
                            resolved_markets.len(),
                            cached_ws_token_count,
                        )
                        .await;
                    }

                    // Time-decay guard: skip signals if too close to interval close.
                    let time_remaining_sec = m.close_time_unix.saturating_sub(wall_sec);
                    let time_ok = time_remaining_sec >= cfg.trading.min_time_remaining_sec as u64;

                    // Solo este mercado (slug = franja ET actual); no otros del horizonte.
                    if book_fresh && time_ok
                        && (!only_active || is_market_open(m, wall_sec))
                        && let (Some((p_up, _)), Some((p_down, _))) = (
                        book_guard.best_ask(m.token_id_up),
                        book_guard.best_ask(m.token_id_down),
                    ) {
                        let asset_idx = m.key.asset.idx();

                        // Effective edge_min (RL-tuned in paper).
                        let effective_edge_min = match &paper_lab {
                            Some(lab) => lab.effective_edge_min(cfg.trading.edge_min),
                            None => cfg.trading.edge_min,
                        };

                        // Bid prices for spread guard.
                        let bid_up = book_guard.best_bid(m.token_id_up).map(|(p, _)| p);
                        let bid_down = book_guard.best_bid(m.token_id_down).map(|(p, _)| p);

                        // Record spread sample for RL.
                        if let Some(ref lab) = paper_lab {
                            if let Some(bu) = bid_up {
                                lab.record_spread_sample(m.interval_start_unix, p_up - bu);
                            }
                        }

                        // Arb requires fresh CEX.
                        let any_cex_fresh = momentum_by_asset.iter().any(|m| m.is_some());

                        if cfg.trading.yes_no_arb_enabled && any_cex_fresh {
                            if let Some(sig) = evaluate_arb_both(
                                m,
                                p_up,
                                p_down,
                                &cfg.trading.arb_yes_no_sum_max,
                                &effective_edge_min,
                            ) {
                                let _ = signal_tx.send(sig);
                            } else if let Some(momentum) = momentum_by_asset[asset_idx].clone() {
                                if let Some(sig) = evaluate_market_signal(
                                    m,
                                    p_up,
                                    p_down,
                                    bid_up,
                                    bid_down,
                                    momentum,
                                    &effective_edge_min,
                                    cfg.trading.max_spread_ticks,
                                ) {
                                    let _ = signal_tx.send(sig);
                                }
                            }
                        } else if let Some(momentum) = momentum_by_asset[asset_idx].clone() {
                            if let Some(sig) = evaluate_market_signal(
                                m,
                                p_up,
                                p_down,
                                bid_up,
                                bid_down,
                                momentum,
                                &effective_edge_min,
                                cfg.trading.max_spread_ticks,
                            ) {
                                let _ = signal_tx.send(sig);
                            }
                        }
                    }

                    if let Some(ref lab) = paper_lab {
                        let bin_state = binance_hist.state_for(Asset::BTC);
                        let bin_guard = bin_state.read().await;
                        let bin_ok = is_fresh(bin_guard.last_update_ms, wall_ms, max_stale_ms);
                        if bin_ok {
                            let mut oldest: Option<Decimal> = None;
                            let cutoff = wall_ms.saturating_sub(lab.impulse_window_ms());
                            bin_guard.for_each_recent(cutoff, |s| {
                                if oldest.is_none() {
                                    oldest = Some(s.price);
                                }
                            });
                            let impulse = PaperLab::impulse_from_samples(oldest, bin_guard.last_price);
                            let anchor = bin_guard.binance_5m_open;
                            let pct_vs_anchor = if anchor.is_zero() {
                                0.0
                            } else {
                                ((bin_guard.last_price / anchor) - Decimal::ONE)
                                    .to_f64()
                                    .unwrap_or(0.0)
                                    * 100.0
                            };
                            let book_ts = book_guard.last_book_event_ts_ms().max(0) as u64;
                            let cex_ts = bin_guard.last_update_ms;
                            let (lag_flag, _) = lab.lag_opportunity(impulse, book_ts, cex_ts);
                            if let (Some((p_up, _)), Some((p_down, _))) = (
                                book_guard.best_ask(m.token_id_up),
                                book_guard.best_ask(m.token_id_down),
                            ) {
                                lab.maybe_log_analysis(
                                    wall_ms,
                                    m.interval_start_unix,
                                    &m.slug,
                                    bin_guard.last_price,
                                    anchor,
                                    pct_vs_anchor,
                                    book_ts,
                                    cex_ts,
                                    impulse,
                                    p_up,
                                    p_down,
                                    lag_flag,
                                );
                            }
                        }
                    }
                }

                const MOM_DIAG_INTERVAL_MS: u64 = 30_000;
                if wall_ms.saturating_sub(last_mom_diag_ms) >= MOM_DIAG_INTERVAL_MS {
                    last_mom_diag_ms = wall_ms;
                    log_momentum_diag(
                        &assets,
                        &momentum_by_asset,
                        &resolved_markets,
                        only_active,
                        wall_sec,
                        &book_guard,
                        cfg.trading.edge_min,
                    );
                }
            }
            _ = sleep(Duration::from_millis(10)) => {}
        }
    }

    // Stop order manager task (it observes `shutdown`).
    let _ = order_manager_handle.await;
    for h in feed_handles {
        let _ = h.abort();
    }

    Ok(())
}
