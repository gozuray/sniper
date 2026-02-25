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

use crate::config::Config;
use crate::dedupe::{Dedupe, IntentKind};
use crate::execution::{Executor, FillStatus};
use crate::gamma::MarketInfo;
use crate::orderbook::OrderBook;
use crate::position::Position;
use crate::strategy::{Action, LiveBuyOrder};

#[tokio::main]
async fn main() -> Result<()> {
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
        // Dynamic 5-min: operate on previous interval slug; switch when interval closes or out of sync.
        loop {
            let slug = gamma::get_previous_5min_slug();
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
            let client = polymarket_client_sdk::clob::Client::new(&config.clob_url, sdk_config)?
                .authentication_builder(&signer)
                .authenticate()
                .await?;

            let executor = Executor::new(client, signer, asset_id);
            let interval_switch_wall_time = tokio::time::Instant::now();

            let should_switch = run_loop(
                config.clone(),
                executor,
                asset_id,
                Some((&market, interval_switch_wall_time)),
            )
            .await?;

            if should_switch {
                tracing::info!("interval closed or out of sync, switching to next market");
                continue;
            }
            break;
        }
    } else {
        // Single TOKEN_ID from env
        let asset_id: ruint::Uint<256, 4> = config
            .token_id
            .parse()
            .context("TOKEN_ID must be a valid U256 number")?;

        let sdk_config = polymarket_client_sdk::clob::Config::default();
        let client = polymarket_client_sdk::clob::Client::new(&config.clob_url, sdk_config)?
            .authentication_builder(&signer)
            .authenticate()
            .await?;

        tracing::info!("CLOB client authenticated");

        let executor = Executor::new(client, signer, asset_id);

        run_loop(config, executor, asset_id, None).await
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
    let mut tick_count: u64 = 0;
    let mut traded_this_interval = false;

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

    match executor.get_book().await {
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

    loop {
        tokio::select! {
            Some(result) = book_stream.next() => {
                match result {
                    Ok(snapshot) => {
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
                        for change in &price_event.price_changes {
                            book.update_best(change.best_bid, change.best_ask);
                        }
                    }
                    Err(e) => {
                        tracing::error!(?e, "WS price stream error");
                        continue;
                    }
                }
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

        let stale = book.is_stale(config.stale_threshold);
        let now_unix = gamma::now_unix();

        let result = handle_tick(
            &config,
            &executor,
            &mut book,
            &mut position,
            &mut dedupe,
            &mut live_buy,
            &mut traded_this_interval,
            stale,
            interval_info.map(|(m, t)| (m, t)),
            now_unix,
        )
        .await;

        if let Err(e) = result {
            tracing::error!(?e, "tick error");
        }

        // Dynamic 5-min: detect interval close or out-of-sync and signal switch
        if let Some((market_info, _)) = interval_info {
            let expected_slug = gamma::get_previous_5min_slug();
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
async fn handle_tick<S: SignerTrait + Send + Sync>(
    config: &Config,
    executor: &Executor<S>,
    book: &mut OrderBook,
    position: &mut Position,
    dedupe: &mut Dedupe,
    live_buy: &mut Option<LiveBuyOrder>,
    traded_this_interval: &mut bool,
    book_is_stale: bool,
    interval_info: Option<(&MarketInfo, tokio::time::Instant)>,
    now_unix: u64,
) -> Result<()> {
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
    );

    match action {
        Action::SendSL {
            size,
            mut limit_price,
        } => {
            // Refresh book if stale before SL
            if book_is_stale {
                if let Ok(snap) = executor.get_book().await {
                    book.update_best(snap.best_bid, snap.best_ask);
                    if let Some(fresh_bid) = snap.best_bid {
                        limit_price = fresh_bid;
                    }
                }
            }

            // SL FAK retry loop for partial fills
            let mut remaining = size;
            loop {
                if remaining <= dec!(0) {
                    break;
                }
                if !dedupe.can_send(IntentKind::SellSL, Some(remaining)) {
                    break;
                }

                let result = executor.sell_fak(remaining, limit_price).await?;
                dedupe.record(IntentKind::SellSL, Some(remaining));

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
                        if let Ok(snap) = executor.get_book().await {
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
            // Early return: no TP or buy this tick
        }

        Action::SendTP {
            size,
            mut limit_price,
        } => {
            if book_is_stale {
                if let Ok(snap) = executor.get_book().await {
                    book.update_best(snap.best_bid, snap.best_ask);
                    if let Some(fresh_bid) = snap.best_bid {
                        limit_price = fresh_bid;
                    }
                }
            }

            let result = executor.sell_limit(size, limit_price).await?;
            dedupe.record(IntentKind::SellTP, Some(size));

            if result.filled_size > dec!(0) {
                position.subtract_fill(result.filled_size);
            }

            tracing::info!(status = ?result.status, "TP result");
            // Early return: no buy this tick
        }

        Action::PlaceBuy { size, price } => {
            // Never send buy outside configured range
            let price = price.max(config.buy_min).min(config.buy_max);
            let result = executor.buy_limit(size, price).await?;
            dedupe.record(IntentKind::Buy, None);
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
                });
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

            // Never send buy outside configured range
            let new_price = new_price.max(config.buy_min).min(config.buy_max);
            let result = executor.buy_limit(new_size, new_price).await?;
            dedupe.record(IntentKind::Buy, None);
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
                });
            }
        }

        Action::Nothing => {}
    }

    Ok(())
}
