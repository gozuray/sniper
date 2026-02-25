mod config;
mod dedupe;
mod execution;
mod orderbook;
mod position;
mod strategy;

use anyhow::{Context, Result};
use futures::StreamExt;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::str::FromStr;

use polymarket_client_sdk::auth::Signer as SignerTrait;

use crate::config::Config;
use crate::dedupe::{Dedupe, IntentKind};
use crate::execution::{Executor, FillStatus};
use crate::orderbook::OrderBook;
use crate::position::Position;
use crate::strategy::{Action, LiveBuyOrder};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "sniper=info".parse().unwrap()),
        )
        .init();

    let config = Config::from_env()?;
    tracing::info!(?config, "loaded configuration");

    // Parse token_id to U256 for WS subscriptions
    let asset_id: ruint::Uint<256, 4> = config
        .token_id
        .parse()
        .context("TOKEN_ID must be a valid U256 number")?;

    // Create signer and authenticate CLOB client
    let private_key =
        std::env::var("POLYMARKET_PRIVATE_KEY").context("POLYMARKET_PRIVATE_KEY is required")?;
    let signer = polymarket_client_sdk::auth::LocalSigner::from_str(&private_key)?
        .with_chain_id(Some(polymarket_client_sdk::POLYGON));

    let sdk_config = polymarket_client_sdk::clob::Config::default();
    let client = polymarket_client_sdk::clob::Client::new(&config.clob_url, sdk_config)?
        .authentication_builder(&signer)
        .authenticate()
        .await?;

    tracing::info!("CLOB client authenticated");

    let executor = Executor::new(client, signer, asset_id);

    run_loop(config, executor, asset_id).await
}

async fn run_loop<S: SignerTrait + Send + Sync>(
    config: Config,
    executor: Executor<S>,
    asset_id: ruint::Uint<256, 4>,
) -> Result<()> {
    let mut book = OrderBook::new();
    let mut position = Position::new();
    let mut dedupe = Dedupe::new(config.dedupe_ttl);
    let mut live_buy: Option<LiveBuyOrder> = None;
    let mut tick_count: u64 = 0;

    // Subscribe to WS streams
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

    // Fetch initial book snapshot via REST
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
                break;
            }
        }

        // ── Tick processing ────────────────────────────────────────
        tick_count += 1;
        if tick_count % 1000 == 0 {
            dedupe.cleanup();
        }

        let stale = book.is_stale(config.stale_threshold);

        let result = handle_tick(
            &config,
            &executor,
            &mut book,
            &mut position,
            &mut dedupe,
            &mut live_buy,
            stale,
        )
        .await;

        if let Err(e) = result {
            tracing::error!(?e, "tick error");
        }
    }

    Ok(())
}

/// Process a single tick. Implements:
///   - Stale-book gate with REST fallback for SL/TP
///   - SL > TP > Buy priority with early return
///   - SL FAK retry loop for partial fills
async fn handle_tick<S: SignerTrait + Send + Sync>(
    config: &Config,
    executor: &Executor<S>,
    book: &mut OrderBook,
    position: &mut Position,
    dedupe: &mut Dedupe,
    live_buy: &mut Option<LiveBuyOrder>,
    book_is_stale: bool,
) -> Result<()> {
    let action = strategy::evaluate(
        config,
        book,
        position,
        dedupe,
        live_buy.as_ref(),
        book_is_stale,
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
            let result = executor.buy_limit(size, price).await?;
            dedupe.record(IntentKind::Buy, None);

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

            let result = executor.buy_limit(new_size, new_price).await?;
            dedupe.record(IntentKind::Buy, None);

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
