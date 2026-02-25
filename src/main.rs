mod config;
mod dedupe;
mod execution;
mod gamma;
mod orderbook;
mod position;
mod strategy;

use anyhow::{Context, Result};
use futures::StreamExt;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::str::FromStr;
use std::time::Duration;

use polymarket_client_sdk::auth::Signer as SignerTrait;
use polymarket_client_sdk::clob::types::SignatureType;

use crate::config::Config;
use crate::dedupe::{Dedupe, IntentKind};
use crate::execution::{Executor, FillStatus};
use crate::gamma::MarketInfo;
use crate::orderbook::OrderBook;
use crate::position::Position;
use crate::strategy::{Action, LiveBuyOrder};

/// Build L2 API credentials from env if POLYMARKET_API_KEY, _SECRET, _PASSPHRASE are all set.
fn api_credentials_from_env() -> Result<Option<polymarket_client_sdk::auth::Credentials>> {
    let key = match std::env::var("POLYMARKET_API_KEY") {
        Ok(k) => k,
        Err(_) => return Ok(None),
    };
    let secret = std::env::var("POLYMARKET_API_SECRET").context("POLYMARKET_API_SECRET required when POLYMARKET_API_KEY is set")?;
    let passphrase = std::env::var("POLYMARKET_API_PASSPHRASE").context("POLYMARKET_API_PASSPHRASE required when POLYMARKET_API_KEY is set")?;
    let api_key: polymarket_client_sdk::auth::ApiKey = key.parse().context("POLYMARKET_API_KEY must be a valid UUID")?;
    Ok(Some(polymarket_client_sdk::auth::Credentials::new(api_key, secret, passphrase)))
}

#[tokio::main]
async fn main() -> Result<()> {
    // Fijar proveedor crypto de rustls una sola vez por proceso (evita panic al abrir nuevas conexiones TLS/WS).
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("instalar CryptoProvider por defecto de rustls");

    // Load .env if present (optional; in production set env vars directly)
    let _ = dotenvy::dotenv();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "sniper=info".parse().unwrap()),
        )
        .init();

    let config = Config::from_env()?;
    tracing::info!(?config, "loaded configuration");

    // Signer and CLOB URL are reused across windows when AUTO_BTC5M
    let private_key =
        std::env::var("POLYMARKET_PRIVATE_KEY").context("POLYMARKET_PRIVATE_KEY is required")?;
    let signer = polymarket_client_sdk::auth::LocalSigner::from_str(&private_key)?
        .with_chain_id(Some(polymarket_client_sdk::POLYGON));

    if config.auto_btc5m {
        // Dynamic 5-min: operate on the *active* interval (current 5-min window); switch when interval closes or out of sync.
        loop {
            let slug = gamma::get_active_5min_slug();
            let now_unix = gamma::now_unix();
            let window_start = gamma::current_window_start_unix();
            tracing::info!(
                slug = %slug,
                window_start_unix = window_start,
                now_unix,
                "active 5-min market (Polymarket: btc-updown-5m-<window_start>, ventana 300s)"
            );

            let (market_up, market_down, asset_id_up, asset_id_down, executor, interval_switch_wall_time) = if config.trade_both_sides {
                let (market_up, market_down) = gamma::fetch_both_market_infos(&slug)
                    .await
                    .with_context(|| format!("fetch both outcomes for slug {slug}"))?;
                let asset_id_up: ruint::Uint<256, 4> = market_up
                    .token_id
                    .parse()
                    .context("Up TOKEN_ID from Gamma must be valid U256")?;
                let asset_id_down: ruint::Uint<256, 4> = market_down
                    .token_id
                    .parse()
                    .context("Down TOKEN_ID from Gamma must be valid U256")?;
                tracing::info!(
                    slug = %market_up.slug,
                    token_up = %market_up.token_id,
                    token_down = %market_down.token_id,
                    close_time_unix = ?market_up.close_time_unix,
                    "starting 5-min window (dual: Up + Down)"
                );
                let signer = polymarket_client_sdk::auth::LocalSigner::from_str(&private_key)?
                    .with_chain_id(Some(polymarket_client_sdk::POLYGON));
                let sdk_config = polymarket_client_sdk::clob::Config::default();
                let mut builder = polymarket_client_sdk::clob::Client::new(&config.clob_url, sdk_config)?
                    .authentication_builder(&signer)
                    .signature_type(SignatureType::GnosisSafe);
                if let Some(creds) = api_credentials_from_env()? {
                    builder = builder.credentials(creds);
                }
                let client = builder.authenticate().await?;
                let interval_switch_wall_time = tokio::time::Instant::now();
                (market_up, market_down, asset_id_up, asset_id_down, Executor::new(client, signer), interval_switch_wall_time)
            } else {
                let market = gamma::fetch_market_info(&slug, config.outcome_up)
                    .await
                    .with_context(|| format!("fetch market for slug {slug}"))?;
                let asset_id: ruint::Uint<256, 4> = market
                    .token_id
                    .parse()
                    .context("TOKEN_ID from Gamma must be valid U256")?;
                tracing::info!(
                    slug = %market.slug,
                    token_id = %market.token_id,
                    close_time_unix = ?market.close_time_unix,
                    "starting 5-min window (dynamic)"
                );
                let signer = polymarket_client_sdk::auth::LocalSigner::from_str(&private_key)?
                    .with_chain_id(Some(polymarket_client_sdk::POLYGON));
                let sdk_config = polymarket_client_sdk::clob::Config::default();
                let mut builder = polymarket_client_sdk::clob::Client::new(&config.clob_url, sdk_config)?
                    .authentication_builder(&signer)
                    .signature_type(SignatureType::GnosisSafe);
                if let Some(creds) = api_credentials_from_env()? {
                    builder = builder.credentials(creds);
                }
                let client = builder.authenticate().await?;
                let interval_switch_wall_time = tokio::time::Instant::now();
                let market_up = market.clone();
                let market_down = market;
                (market_up, market_down, asset_id, asset_id, Executor::new(client, signer), interval_switch_wall_time)
            };

            let should_switch = if config.trade_both_sides {
                run_loop_dual(
                    config.clone(),
                    executor,
                    asset_id_up,
                    asset_id_down,
                    (&market_up, &market_down, &interval_switch_wall_time),
                )
                .await?
            } else {
                run_loop(
                    config.clone(),
                    executor,
                    asset_id_up,
                    Some((&market_up, interval_switch_wall_time)),
                )
                .await?
            };

            if should_switch {
                tracing::info!("interval closed or out of sync, switching to next market");
                continue;
            }
            break;
        }
        Ok(())
    } else {
        // Single TOKEN_ID from env
        let asset_id: ruint::Uint<256, 4> = config
            .token_id
            .parse()
            .context("TOKEN_ID must be a valid U256 number")?;

        let sdk_config = polymarket_client_sdk::clob::Config::default();
        let mut builder = polymarket_client_sdk::clob::Client::new(&config.clob_url, sdk_config)?
            .authentication_builder(&signer)
            .signature_type(SignatureType::GnosisSafe);
        if let Some(creds) = api_credentials_from_env()? {
            builder = builder.credentials(creds);
        }
        let client = builder.authenticate().await?;

        tracing::info!("CLOB client authenticated");

        let executor = Executor::new(client, signer);

        run_loop(config, executor, asset_id, None).await.map(|_| ())
    }
}

/// Dual outcome: scan and trade both Up and Down; execute when price is in range on either side.
async fn run_loop_dual<S: SignerTrait + Send + Sync>(
    config: Config,
    executor: Executor<S>,
    asset_id_up: ruint::Uint<256, 4>,
    asset_id_down: ruint::Uint<256, 4>,
    interval_info: (&MarketInfo, &MarketInfo, &tokio::time::Instant),
) -> Result<bool> {
    let (market_up, _market_down, interval_switch_wall_time) = interval_info;
    let mut book_up = OrderBook::new();
    let mut book_down = OrderBook::new();
    let mut position_up = Position::new();
    let mut position_down = Position::new();
    let mut dedupe_up = Dedupe::new(config.dedupe_ttl);
    let mut dedupe_down = Dedupe::new(config.dedupe_ttl);
    let mut live_buy_up: Option<LiveBuyOrder> = None;
    let mut live_buy_down: Option<LiveBuyOrder> = None;
    let mut last_order_sync_up: Option<std::time::Instant> = None;
    let mut last_order_sync_down: Option<std::time::Instant> = None;
    let mut traded_up_this_interval = false;
    let mut traded_down_this_interval = false;
    let mut tick_count: u64 = 0;

    let mut last_tick_error: Option<(String, std::time::Instant)> = None;
    const TICK_ERROR_LOG_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(30);

    let ws_client = polymarket_client_sdk::clob::ws::Client::default();
    let asset_ids = vec![asset_id_up, asset_id_down];

    tracing::info!("subscribing to WS orderbook + prices (Up + Down)");

    let book_stream = ws_client
        .subscribe_orderbook(asset_ids.clone())
        .context("failed to subscribe to orderbook")?;
    let price_stream = ws_client
        .subscribe_prices(asset_ids.clone())
        .context("failed to subscribe to prices")?;

    let mut book_stream = Box::pin(book_stream);
    let mut price_stream = Box::pin(price_stream);

    let mut ws_first_update_logged = false;
    const PRICE_LOG_EVERY_N_TICKS: u64 = 300;

    if let Ok(snap) = executor.get_book(asset_id_up).await {
        book_up.update_best(snap.best_bid, snap.best_ask);
        tracing::info!(best_bid = ?book_up.best_bid, best_ask = ?book_up.best_ask, "initial book Up");
    }
    if let Ok(snap) = executor.get_book(asset_id_down).await {
        book_down.update_best(snap.best_bid, snap.best_ask);
        tracing::info!(best_bid = ?book_down.best_bid, best_ask = ?book_down.best_ask, "initial book Down");
    }

    let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    heartbeat.tick().await;

    loop {
        tokio::select! {
            Some(result) = book_stream.next() => {
                match result {
                    Ok(snapshot) => {
                        if snapshot.asset_id == asset_id_up {
                            if !ws_first_update_logged {
                                tracing::info!("WS orderbook: primer update en tiempo real recibido (dual)");
                                ws_first_update_logged = true;
                            }
                            let bids: Vec<(Decimal, Decimal)> = snapshot.bids.iter().map(|l| (l.price, l.size)).collect();
                            let asks: Vec<(Decimal, Decimal)> = snapshot.asks.iter().map(|l| (l.price, l.size)).collect();
                            book_up.update_from_levels(&bids, &asks);
                        } else if snapshot.asset_id == asset_id_down {
                            if !ws_first_update_logged {
                                tracing::info!("WS orderbook: primer update en tiempo real recibido (dual)");
                                ws_first_update_logged = true;
                            }
                            let bids: Vec<(Decimal, Decimal)> = snapshot.bids.iter().map(|l| (l.price, l.size)).collect();
                            let asks: Vec<(Decimal, Decimal)> = snapshot.asks.iter().map(|l| (l.price, l.size)).collect();
                            book_down.update_from_levels(&bids, &asks);
                        }
                    }
                    Err(e) => {
                        tracing::error!(?e, "WS book stream error");
                        continue;
                    }
                }
            }
            Some(result) = price_stream.next() => {
                match result {
                    Ok(price_event) => {
                        if !ws_first_update_logged {
                            tracing::info!("WS prices: primer update en tiempo real recibido (dual)");
                            ws_first_update_logged = true;
                        }
                        for change in &price_event.price_changes {
                            if change.asset_id == asset_id_up {
                                book_up.update_best(change.best_bid, change.best_ask);
                            } else if change.asset_id == asset_id_down {
                                book_down.update_best(change.best_bid, change.best_ask);
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!(?e, "WS price stream error");
                        continue;
                    }
                }
            }
            _ = heartbeat.tick() => {
                tracing::info!(
                    up_bid = ?book_up.best_bid, up_ask = ?book_up.best_ask,
                    down_bid = ?book_down.best_bid, down_ask = ?book_down.best_ask,
                    "heartbeat dual (Up + Down)"
                );
            }
            else => {
                tracing::warn!("all WS streams closed, reconnecting...");
                return Ok(false);
            }
        }

        tick_count += 1;
        if tick_count % 1000 == 0 {
            dedupe_up.cleanup();
            dedupe_down.cleanup();
        }

        if tick_count % PRICE_LOG_EVERY_N_TICKS == 0 {
            let in_range_up = book_up.best_ask.map(|a| a >= config.buy_min && a <= config.buy_max).unwrap_or(false);
            let in_range_down = book_down.best_ask.map(|a| a >= config.buy_min && a <= config.buy_max).unwrap_or(false);
            if book_up.best_bid.is_some() || book_up.best_ask.is_some() || book_down.best_bid.is_some() || book_down.best_ask.is_some() {
                tracing::info!(
                    "Up: bid={:?} ask={:?} zone={} | Down: bid={:?} ask={:?} zone={}",
                    book_up.best_bid, book_up.best_ask, in_range_up,
                    book_down.best_bid, book_down.best_ask, in_range_down
                );
            }
        }

        let stale_up = book_up.is_stale(config.stale_threshold);
        let stale_down = book_down.is_stale(config.stale_threshold);
        let now_unix = gamma::now_unix();
        let interval_info_opt = Some((market_up, *interval_switch_wall_time));

        let result_up = handle_tick(
            &config,
            &executor,
            asset_id_up,
            &mut book_up,
            &mut position_up,
            &mut dedupe_up,
            &mut live_buy_up,
            &mut traded_up_this_interval,
            stale_up,
            interval_info_opt,
            now_unix,
            &mut last_order_sync_up,
        )
        .await;
        if let Err(e) = result_up {
            let err_msg = format!("{:?}", e);
            let should_log = match &last_tick_error {
                None => true,
                Some((prev, ts)) => prev != &err_msg || ts.elapsed() >= TICK_ERROR_LOG_COOLDOWN,
            };
            if should_log {
                tracing::error!(side = "Up", ?e, "tick error");
                if err_msg.contains("not enough balance") || err_msg.contains("allowance") {
                    tracing::error!(
                        "Para VENDER (SL/TP) hace falta saldo de outcome tokens y allowance de Conditional Tokens. \
                        Revisa README: cargo run --bin check_balance y approvals (USDC + CTF)."
                    );
                }
                last_tick_error = Some((err_msg, std::time::Instant::now()));
            }
        } else {
            last_tick_error = None;
        }

        let result_down = handle_tick(
            &config,
            &executor,
            asset_id_down,
            &mut book_down,
            &mut position_down,
            &mut dedupe_down,
            &mut live_buy_down,
            &mut traded_down_this_interval,
            stale_down,
            interval_info_opt,
            now_unix,
            &mut last_order_sync_down,
        )
        .await;
        if let Err(e) = result_down {
            let err_msg = format!("{:?}", e);
            let should_log = match &last_tick_error {
                None => true,
                Some((prev, ts)) => prev != &err_msg || ts.elapsed() >= TICK_ERROR_LOG_COOLDOWN,
            };
            if should_log {
                tracing::error!(side = "Down", ?e, "tick error");
                if err_msg.contains("not enough balance") || err_msg.contains("allowance") {
                    tracing::error!(
                        "Para VENDER (SL/TP) hace falta saldo de outcome tokens y allowance de Conditional Tokens. \
                        Revisa README: cargo run --bin check_balance y approvals (USDC + CTF)."
                    );
                }
                last_tick_error = Some((err_msg, std::time::Instant::now()));
            }
        } else {
            last_tick_error = None;
        }

        let expected_slug = gamma::get_active_5min_slug();
        let is_out_of_sync = market_up.slug != expected_slug;
        let market_just_closed = market_up
            .close_time_unix
            .map(|t| now_unix >= t)
            .unwrap_or(false);
        if is_out_of_sync || market_just_closed {
            tracing::info!(
                current_slug = %market_up.slug,
                expected_slug = %expected_slug,
                market_just_closed,
                "interval switch: resubscribing to new market (dual)"
            );
            return Ok(true);
        }
    }
}

async fn run_loop<S: SignerTrait + Send + Sync>(
    config: Config,
    executor: Executor<S>,
    asset_id: ruint::Uint<256, 4>,
    interval_info: Option<(&MarketInfo, tokio::time::Instant)>,
) -> Result<bool> {
    let mut book = OrderBook::new();
    let mut position = Position::new();
    let mut dedupe = Dedupe::new(config.dedupe_ttl);
    let mut live_buy: Option<LiveBuyOrder> = None;
    let mut last_order_sync: Option<std::time::Instant> = None;
    let mut tick_count: u64 = 0;
    let mut traded_this_interval = false;

    // Throttle repeated "tick error" to avoid spamming logs (e.g. "not enough balance" every tick)
    let mut last_tick_error: Option<(String, std::time::Instant)> = None;
    const TICK_ERROR_LOG_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(30);

    let ws_client = polymarket_client_sdk::clob::ws::Client::default();
    let asset_ids = vec![asset_id];

    tracing::info!(token_id = %config.token_id, "subscribing to WS orderbook + prices");

    let book_stream = ws_client
        .subscribe_orderbook(asset_ids.clone())
        .context("failed to subscribe to orderbook")?;
    let price_stream = ws_client
        .subscribe_prices(asset_ids.clone())
        .context("failed to subscribe to prices")?;

    let mut book_stream = Box::pin(book_stream);
    let mut price_stream = Box::pin(price_stream);

    // Last printed best bid/ask to avoid flooding; show WS prices vs entry range while waiting
    let mut last_printed_bid: Option<Decimal> = None;
    let mut last_printed_ask: Option<Decimal> = None;
    let mut ws_first_update_logged = false;
    const PRICE_LOG_EVERY_N_TICKS: u64 = 300;

    match executor.get_book(asset_id).await {
        Ok(snap) => {
            book.update_best(snap.best_bid, snap.best_ask);
            tracing::info!(
                best_bid = ?book.best_bid,
                best_ask = ?book.best_ask,
                "initial book snapshot from REST"
            );
        }
        Err(e) => tracing::warn!(?e, "failed to fetch initial book via REST"),
    }

    // Wake-up cada 15s para log y chequeo de cambio de intervalo aunque el WS no envíe nada
    let mut heartbeat = tokio::time::interval(Duration::from_secs(15));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    heartbeat.tick().await; // first tick fires immediately, skip it

    loop {
        tokio::select! {
            Some(result) = book_stream.next() => {
                match result {
                    Ok(snapshot) => {
                        // Solo aplicar si es para nuestro token (el WS puede enviar varios assets).
                        if snapshot.asset_id != asset_id {
                            continue;
                        }
                        if !ws_first_update_logged {
                            tracing::info!("WS orderbook: primer update en tiempo real recibido");
                            ws_first_update_logged = true;
                        }
                        let bids: Vec<(Decimal, Decimal)> = snapshot
                            .bids
                            .iter()
                            .map(|l| (l.price, l.size))
                            .collect();
                        let asks: Vec<(Decimal, Decimal)> = snapshot
                            .asks
                            .iter()
                            .map(|l| (l.price, l.size))
                            .collect();
                        book.update_from_levels(&bids, &asks);
                    }
                    Err(e) => {
                        tracing::error!(?e, "WS book stream error");
                        continue;
                    }
                }
            }
            Some(result) = price_stream.next() => {
                match result {
                    Ok(price_event) => {
                        if !ws_first_update_logged {
                            tracing::info!("WS prices: primer update en tiempo real recibido");
                            ws_first_update_logged = true;
                        }
                        // Solo aplicar cambios de nuestro token; el batch puede incluir ambos outcomes (0.01/0.99 del otro).
                        for change in &price_event.price_changes {
                            if change.asset_id != asset_id {
                                continue;
                            }
                            book.update_best(change.best_bid, change.best_ask);
                        }
                    }
                    Err(e) => {
                        tracing::error!(?e, "WS price stream error");
                        continue;
                    }
                }
            }
            _ = heartbeat.tick() => {
                tracing::info!(
                    best_bid = ?book.best_bid,
                    best_ask = ?book.best_ask,
                    "heartbeat (cada 15s si WS no envía; comprobando cambio de intervalo)"
                );
            }
            else => {
                tracing::warn!("all WS streams closed, reconnecting...");
                return Ok(false);
            }
        }

        tick_count += 1;
        if tick_count % 1000 == 0 {
            dedupe.cleanup();
        }

        // Mostrar en terminal precios del libro (WS) y rango de entrada configurado
        let bid_changed = book.best_bid != last_printed_bid;
        let ask_changed = book.best_ask != last_printed_ask;
        if (tick_count % PRICE_LOG_EVERY_N_TICKS == 0 || bid_changed || ask_changed)
            && (book.best_bid.is_some() || book.best_ask.is_some())
        {
            let in_range = book
                .best_ask
                .map(|a| a >= config.buy_min && a <= config.buy_max)
                .unwrap_or(false);
            let bid = book.best_bid.map(|b| b.to_string()).unwrap_or_else(|| "-".into());
            let ask = book.best_ask.map(|a| a.to_string()).unwrap_or_else(|| "-".into());
            tracing::info!(
                "bid={} ask={} buy_min={} buy_max={} zone={}",
                bid,
                ask,
                config.buy_min,
                config.buy_max,
                in_range
            );
            last_printed_bid = book.best_bid;
            last_printed_ask = book.best_ask;
        }

        let stale = book.is_stale(config.stale_threshold);
        let now_unix = gamma::now_unix();

        let result = handle_tick(
            &config,
            &executor,
            asset_id,
            &mut book,
            &mut position,
            &mut dedupe,
            &mut live_buy,
            &mut traded_this_interval,
            stale,
            interval_info.map(|(m, t)| (m, t)),
            now_unix,
            &mut last_order_sync,
        )
        .await;

        if let Err(e) = result {
            let err_msg = format!("{:?}", e);
            let should_log = match &last_tick_error {
                None => true,
                Some((prev, ts)) => {
                    prev != &err_msg || ts.elapsed() >= TICK_ERROR_LOG_COOLDOWN
                }
            };
            if should_log {
                tracing::error!(?e, "tick error");
                if err_msg.contains("not enough balance") || err_msg.contains("allowance") {
                    tracing::error!(
                        "Para VENDER (SL/TP) hace falta saldo de outcome tokens y allowance de Conditional Tokens. \
                        Revisa README: cargo run --bin check_balance y approvals (USDC + CTF)."
                    );
                }
                last_tick_error = Some((err_msg, std::time::Instant::now()));
            }
        } else {
            last_tick_error = None; // clear so next error is logged
        }

        // Dynamic 5-min: detect interval close or out-of-sync and signal switch to next active market
        if let Some((market_info, _)) = interval_info {
            let expected_slug = gamma::get_active_5min_slug();
            let is_out_of_sync = market_info.slug != expected_slug;
            let market_just_closed = market_info
                .close_time_unix
                .map(|t| now_unix >= t)
                .unwrap_or(false);
            if is_out_of_sync || market_just_closed {
                tracing::info!(
                    current_slug = %market_info.slug,
                    expected_slug = %expected_slug,
                    market_just_closed = market_just_closed,
                    "interval switch: resubscribing to new market"
                );
                return Ok(true);
            }
        }
    }
}

/// Process a single tick. Implements:
///   - Stale-book gate with REST fallback for SL/TP
///   - SL > TP > Buy priority with early return
///   - One buy per interval; never buy outside [buy_min, buy_max]
///   - SL FAK retry loop for partial fills
///   - When interval_info is set: no buy within MIN_DELAY_AFTER_INTERVAL_START_SEC of interval start or switch
///   - Position sync from resting buy is throttled (ORDER_SYNC_INTERVAL_MS) so we don't block every tick on REST.
async fn handle_tick<S: SignerTrait + Send + Sync>(
    config: &Config,
    executor: &Executor<S>,
    asset_id: ruint::Uint<256, 4>,
    book: &mut OrderBook,
    position: &mut Position,
    dedupe: &mut Dedupe,
    live_buy: &mut Option<LiveBuyOrder>,
    traded_this_interval: &mut bool,
    book_is_stale: bool,
    interval_info: Option<(&MarketInfo, tokio::time::Instant)>,
    now_unix: u64,
    last_order_sync: &mut Option<std::time::Instant>,
) -> Result<()> {
    // Sync position from resting buy order at most every order_sync_interval_ms (HFT: avoid REST on every WS message).
    let sync_interval = std::time::Duration::from_millis(config.order_sync_interval_ms);
    if let Some(buy) = live_buy.as_mut() {
        let should_sync = last_order_sync
            .map(|t| t.elapsed() >= sync_interval)
            .unwrap_or(true);
        if should_sync {
            if let Ok(Some((size_matched, is_live))) = executor.get_order_matched(&buy.order_id).await {
                let delta = size_matched - buy.filled_so_far;
                if delta > dec!(0) {
                    position.add_fill(delta);
                    buy.filled_so_far = size_matched;
                    tracing::info!(order_id = %buy.order_id, size_matched = %size_matched, delta = %delta, "buy order fill synced to position");
                }
                if !is_live {
                    let order_id = buy.order_id.clone();
                    let _ = executor.cancel_order(&order_id).await;
                    *live_buy = None;
                    tracing::debug!(order_id = %order_id, "live buy order no longer on book, cleared");
                }
            }
            *last_order_sync = Some(std::time::Instant::now());
        }
    } else {
        *last_order_sync = None;
    }

    let action = strategy::evaluate(
        config,
        book,
        position,
        dedupe,
        live_buy.as_ref(),
        book_is_stale,
        *traded_this_interval,
        interval_info.map(|(m, t)| (m.close_time_unix, t)),
        now_unix,
        std::time::Instant::now(),
    );

    match action {
        Action::SendSL {
            size: _,
            mut limit_price,
        } => {
            // Vender el máximo que tenemos: usar posición actual al ejecutar (puede haber cambiado desde evaluate)
            let size = position.shares;
            if size <= dec!(0) {
                // Sin posición, no hacer nada
            } else {
            // Refresh book if stale before SL
            if book_is_stale {
                if let Ok(snap) = executor.get_book(asset_id).await {
                    book.update_best(snap.best_bid, snap.best_ask);
                    if let Some(fresh_bid) = snap.best_bid {
                        limit_price = fresh_bid;
                    }
                }
            }

            // SL FAK retry loop: intentar vender todo (partial fill → retry con el resto)
            let mut remaining = size;
            loop {
                if remaining <= dec!(0) {
                    break;
                }
                if !dedupe.can_send(IntentKind::SellSL, Some(remaining)) {
                    break;
                }
                // Nunca enviar más de lo que tenemos
                let to_sell = remaining.min(position.shares);
                if to_sell <= dec!(0) {
                    break;
                }

                let result = executor.sell_fak(asset_id, to_sell, limit_price).await?;
                dedupe.record(IntentKind::SellSL, Some(to_sell));

                if result.filled_size > dec!(0) {
                    position.subtract_fill(result.filled_size);

                    if let Some(buy) = live_buy.take() {
                        let _ = executor.cancel_order(&buy.order_id).await;
                    }
                }

                match result.status {
                    FillStatus::FullyFilled => {
                        tracing::info!("SL fully filled");
                        break;
                    }
                    FillStatus::PartiallyFilled => {
                        remaining -= result.filled_size;
                        tracing::warn!(
                            remainder = %remaining,
                            "SL partial fill, retrying immediately"
                        );
                        // Refresh best_bid for retry
                        if let Ok(snap) = executor.get_book(asset_id).await {
                            book.update_best(snap.best_bid, snap.best_ask);
                            if let Some(fresh_bid) = snap.best_bid {
                                limit_price = fresh_bid;
                            }
                        }
                    }
                    FillStatus::NotFilled | FillStatus::Placed => {
                        tracing::warn!("SL FAK got no fill");
                        break;
                    }
                }
            }
            }
            // Early return: no TP or buy this tick
        }

        Action::SendTP {
            size: _,
            mut limit_price,
        } => {
            // Vender el máximo que tenemos: usar posición actual al ejecutar
            let size = position.shares;
            if size <= dec!(0) {
                // Sin posición, no hacer nada
            } else {
            if book_is_stale {
                if let Ok(snap) = executor.get_book(asset_id).await {
                    book.update_best(snap.best_bid, snap.best_ask);
                    if let Some(fresh_bid) = snap.best_bid {
                        limit_price = fresh_bid;
                    }
                }
            }

            // TP FAK retry loop: intentar vender todo (partial fill → retry con el resto)
            let mut remaining = size;
            loop {
                if remaining <= dec!(0) {
                    break;
                }
                if !dedupe.can_send(IntentKind::SellTP, Some(remaining)) {
                    break;
                }
                let to_sell = remaining.min(position.shares);
                if to_sell <= dec!(0) {
                    break;
                }

                let result = executor.sell_fak_tp(asset_id, to_sell, limit_price).await?;
                dedupe.record(IntentKind::SellTP, Some(to_sell));

                if result.filled_size > dec!(0) {
                    position.subtract_fill(result.filled_size);

                    if let Some(buy) = live_buy.take() {
                        let _ = executor.cancel_order(&buy.order_id).await;
                    }
                }

                match result.status {
                    FillStatus::FullyFilled => {
                        tracing::info!("TP fully filled");
                        break;
                    }
                    FillStatus::PartiallyFilled => {
                        remaining -= result.filled_size;
                        tracing::warn!(
                            remainder = %remaining,
                            "TP partial fill, retrying immediately (HFT)"
                        );
                        if let Ok(snap) = executor.get_book(asset_id).await {
                            book.update_best(snap.best_bid, snap.best_ask);
                            if let Some(fresh_bid) = snap.best_bid {
                                limit_price = fresh_bid;
                            }
                        }
                    }
                    FillStatus::NotFilled | FillStatus::Placed => {
                        tracing::warn!("TP FAK got no fill");
                        break;
                    }
                }
            }
            }
            // Early return: no buy this tick
        }

        Action::PlaceBuy { size, price } => {
            // Never send buy outside configured range (defensive clamp)
            let price = price.max(config.buy_min).min(config.buy_max);
            dedupe.record(IntentKind::Buy, None);
            match executor.buy_limit(asset_id, size, price).await {
                Ok(result) => {
                    *traded_this_interval = true;
                    if result.filled_size > dec!(0) {
                        position.add_fill(result.filled_size);
                    }
                    if result.status == FillStatus::Placed
                        || result.status == FillStatus::PartiallyFilled
                    {
                        *live_buy = Some(LiveBuyOrder {
                            order_id: result.order_id,
                            price,
                            size,
                            placed_at: std::time::Instant::now(),
                            filled_so_far: result.filled_size,
                        });
                    }
                }
                Err(e) => {
                    *traded_this_interval = true;
                    return Err(e);
                }
            }
        }

        Action::CancelBuy { order_id } => {
            let _ = executor.cancel_order(&order_id).await;
            *live_buy = None;
        }

        Action::CancelAndReplaceBuy {
            cancel_order_id,
            new_size,
            new_price,
        } => {
            let _ = executor.cancel_order(&cancel_order_id).await;
            *live_buy = None;

            let new_price = new_price.max(config.buy_min).min(config.buy_max);
            dedupe.record(IntentKind::Buy, None);
            match executor.buy_limit(asset_id, new_size, new_price).await {
                Ok(result) => {
                    *traded_this_interval = true;
                    if result.filled_size > dec!(0) {
                        position.add_fill(result.filled_size);
                    }
                    if result.status == FillStatus::Placed
                        || result.status == FillStatus::PartiallyFilled
                    {
                        *live_buy = Some(LiveBuyOrder {
                            order_id: result.order_id,
                            price: new_price,
                            size: new_size,
                            placed_at: std::time::Instant::now(),
                            filled_so_far: result.filled_size,
                        });
                    }
                }
                Err(e) => {
                    *traded_this_interval = true;
                    return Err(e);
                }
            }
        }

        Action::Nothing => {}
    }

    Ok(())
}
