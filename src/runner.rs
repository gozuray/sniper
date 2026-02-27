//! Main loop: interval switch, top-of-book, buy in range, TP/SL.

#[allow(unused_imports)]
use crate::clob::{ClobClient, LimitOrderParams, OrderSide, OrderType};
use crate::clob_ws_book::ClobWsBook;
use crate::config::{current_5min_slug, load_config};
use crate::market::fetch_market_by_slug;
use crate::orderbook::fetch_top_of_book;
use crate::types::{
    Config, EntrySide, LastBuyOrder, PendingAutoSell, PendingStopLoss, ResolvedMarket, TopOfBook,
};
use anyhow::Result;
use reqwest::Client;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};
use tracing::{info, warn};

const TICK_SIZE: Decimal = dec!(0.01);
const CLOB_DEFAULT_MIN_ORDER_SIZE: Decimal = dec!(5);
/// Log order book and TP/SL status every this many loop ticks (e.g. 10 → ~1s if loop_ms=100).
const LOG_BOOK_EVERY_TICKS: u64 = 10;
/// Delay between FAK retries when no match (ms). Kept low for near-instant retries.
const FAK_RETRY_DELAY_MS: u64 = 30;
/// Max FAK retries to liquidate position.
const FAK_MAX_RETRIES: u32 = 50;

/// True if top has at least one side with book data (for WS fallback to REST).
fn top_has_book_data(top: &TopOfBook) -> bool {
    let up_ok = top
        .token_id_up
        .as_ref()
        .map(|s| s.best_ask.is_some() || s.best_bid.is_some())
        .unwrap_or(false);
    let down_ok = top
        .token_id_down
        .as_ref()
        .map(|s| s.best_ask.is_some() || s.best_bid.is_some())
        .unwrap_or(false);
    up_ok || down_ok
}

struct RunnerState {
    config: Config,
    market: Option<ResolvedMarket>,
    /// WebSocket order book when connected; None = use REST only.
    ws_book: Option<ClobWsBook>,
    ordered_this_interval: bool,
    total_shares_this_interval: Decimal,
    last_buy_order: Option<LastBuyOrder>,
    pending_auto_sell: Option<PendingAutoSell>,
    pending_stop_loss: Option<PendingStopLoss>,
    auto_sell_placed: bool,
    stop_loss_placed: bool,
    interval_switch_wall_time_ms: Option<u64>,
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

fn seconds_to_close(now_unix: u64, close_time_unix: u64) -> u64 {
    close_time_unix.saturating_sub(now_unix)
}

fn round_to_tick(price: Decimal) -> Decimal {
    let ticks = (price / TICK_SIZE).round();
    (ticks * TICK_SIZE).round_dp(2)
}

fn maker_amount_2_decimals(size: Decimal, price: Decimal) -> Decimal {
    (size * price).round_dp(2)
}

fn size_4_decimals(size: Decimal) -> Decimal {
    size.round_dp(4)
}

fn fmt_price(p: Option<&Decimal>) -> String {
    p.map(fmt_decimal_2).unwrap_or_else(|| "-".to_string())
}

/// Format a decimal with exactly 2 decimal places (e.g. 0.4 → "0.40", 10.5 → "10.50").
fn fmt_decimal_2(d: &Decimal) -> String {
    let r = d.round_dp(2);
    let s = r.to_string();
    if let Some((int_part, frac_part)) = s.split_once('.') {
        let frac = if frac_part.len() > 2 {
            &frac_part[..2]
        } else {
            frac_part
        };
        let frac_padded = format!("{:0<2}", frac);
        format!("{}.{}", int_part, frac_padded)
    } else {
        format!("{}.00", s)
    }
}

/// Format seconds for log: at least 2 digits with leading zero (e.g. 9 → "09", 209 → "209").
fn fmt_secs(n: u64) -> String {
    format!("{:02}", n)
}

/// Choose entry side: Up or Down with higher best ask in [min_buy_price, max_buy_price], with min liquidity.
fn choose_side(
    config: &Config,
    book: &TopOfBook,
    min_order_size: Decimal,
) -> Option<(EntrySide, Decimal, Decimal)> {
    let up = book.token_id_up.as_ref()?;
    let down = book.token_id_down.as_ref()?;
    let up_ask = config.allow_buy_up.then(|| up.best_ask).flatten()?;
    let down_ask = config.allow_buy_down.then(|| down.best_ask).flatten()?;
    let up_size = up.best_ask_size.unwrap_or(Decimal::ZERO);
    let down_size = down.best_ask_size.unwrap_or(Decimal::ZERO);

    let in_range = |p: Decimal| p >= config.min_buy_price && p <= config.max_buy_price;

    let mut candidates: Vec<(EntrySide, Decimal, Decimal)> = Vec::new();
    if in_range(up_ask) && up_size >= min_order_size {
        candidates.push((EntrySide::Up, up_ask, up_size));
    }
    if in_range(down_ask) && down_size >= min_order_size {
        candidates.push((EntrySide::Down, down_ask, down_size));
    }
    candidates.sort_by(|a, b| b.1.cmp(&a.1)); // higher price first
    candidates.into_iter().next()
}

pub async fn run() -> Result<()> {
    let config = load_config()?;
    let clob_host = std::env::var("POLYMARKET_CLOB_HOST").unwrap_or_else(|_| "https://clob.polymarket.com".to_string());
    let http = Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    let clob = Arc::new(crate::clob::create_clob_client(config.dry_run)?);

    let mut state = RunnerState {
        market: None,
        ws_book: None,
        config: config.clone(),
        ordered_this_interval: false,
        total_shares_this_interval: Decimal::ZERO,
        last_buy_order: None,
        pending_auto_sell: None,
        pending_stop_loss: None,
        auto_sell_placed: false,
        stop_loss_placed: false,
        interval_switch_wall_time_ms: None,
    };

    info!("[IntervalSniper] started dry_run={} slug={}", config.dry_run, config.market_slug);

    let loop_ms = config.loop_ms;
    let mut tick_count: u64 = 0;

    loop {
        tick_count += 1;
        let now_u = now_unix();
        let now_ms_u = now_ms();

        // Refresh market if needed (interval switch) — always use current 5-min window slug
        // e.g. 5:15–5:20 → btc-updown-5m-1772169300, 5:20–5:25 → btc-updown-5m-1772169600
        let current_slug = current_5min_slug(config.interval_market);
        let need_new_market = state.market.is_none()
            || state.market.as_ref().map(|m| now_u >= m.close_time_unix).unwrap_or(true)
            || state.market.as_ref().map(|m| current_slug != m.slug).unwrap_or(true);

        if need_new_market {
            match fetch_market_by_slug(&http, &config.gamma_base_url, &current_slug).await {
                Ok(market) => {
                    state.ws_book = None; // drop previous WS before creating new
                    let ws_url = ClobWsBook::ws_url_from_rest_host(&clob_host);
                    match ClobWsBook::connect(
                        &ws_url,
                        &market.token_id_up,
                        &market.token_id_down,
                    )
                    .await
                    {
                        Ok(ws) => {
                            state.ws_book = Some(ws);
                            info!("[IntervalSniper] WebSocket order book connected (real-time)");
                        }
                        Err(e) => {
                            warn!(
                                "[IntervalSniper] WebSocket book connect failed: {}, using REST",
                                e
                            );
                        }
                    }
                    state.market = Some(market.clone());
                    state.ordered_this_interval = false;
                    state.total_shares_this_interval = Decimal::ZERO;
                    state.last_buy_order = None;
                    state.pending_auto_sell = None;
                    state.pending_stop_loss = None;
                    state.auto_sell_placed = false;
                    state.stop_loss_placed = false;
                    state.interval_switch_wall_time_ms = Some(now_ms_u);
                    let up_id = market.token_id_up.trim();
                    let down_id = market.token_id_down.trim();
                    info!(
                        "[IntervalSniper] interval switch -> {} (Up token={}... Down token={}...)",
                        market.slug,
                        if up_id.len() > 12 { &up_id[..12] } else { up_id },
                        if down_id.len() > 12 { &down_id[..12] } else { down_id }
                    );
                }
                Err(e) => {
                    warn!("[IntervalSniper] fetch market failed: {}", e);
                    tokio::time::sleep(Duration::from_millis(loop_ms)).await;
                    continue;
                }
            }
        }

        let market = match &state.market {
            Some(m) => m,
            None => {
                tokio::time::sleep(Duration::from_millis(loop_ms)).await;
                continue;
            }
        };

        let secs_to_close = seconds_to_close(now_u, market.close_time_unix);

        // Top of book: WebSocket (instant) when connected, else REST. Fallback to REST if WS has no data yet.
        let top = if let Some(ref ws) = state.ws_book {
            let t = ws.get_top_of_book().await;
            if top_has_book_data(&t) {
                t
            } else {
                fetch_top_of_book(
                    &http,
                    &clob_host,
                    &market.token_id_up,
                    &market.token_id_down,
                )
                .await
                .unwrap_or(t)
            }
        } else {
            match fetch_top_of_book(
                &http,
                &clob_host,
                &market.token_id_up,
                &market.token_id_down,
            )
            .await
            {
                Ok(t) => t,
                Err(e) => {
                    warn!("[IntervalSniper] order book fetch failed: {}", e);
                    tokio::time::sleep(Duration::from_millis(loop_ms)).await;
                    continue;
                }
            }
        };

        // Periodic log: order book scan (real-time visibility)
        if tick_count % LOG_BOOK_EVERY_TICKS == 0 {
            let up = top.token_id_up.as_ref();
            let down = top.token_id_down.as_ref();
            info!(
                "[IntervalSniper] order book Up bid={} ask={} | Down bid={} ask={} | secs_to_close={}",
                fmt_price(up.and_then(|s| s.best_bid.as_ref())),
                fmt_price(up.and_then(|s| s.best_ask.as_ref())),
                fmt_price(down.and_then(|s| s.best_bid.as_ref())),
                fmt_price(down.and_then(|s| s.best_ask.as_ref())),
                fmt_secs(secs_to_close)
            );
            // When position open, log TP/SL monitoring so user sees we're checking for fills
            if let Some(ref tp) = state.pending_auto_sell {
                if !state.auto_sell_placed {
                    let is_up = tp.token_id == market.token_id_up;
                    let side_book = if is_up { &top.token_id_up } else { &top.token_id_down };
                    info!(
                        "[IntervalSniper] position open: TP target={} best_bid={} (sell when bid >= target)",
                        fmt_price(Some(&tp.target_price)),
                        fmt_price(side_book.as_ref().and_then(|s| s.best_bid.as_ref()))
                    );
                }
            }
            if let Some(ref sl) = state.pending_stop_loss {
                if !state.stop_loss_placed {
                    let is_up = sl.token_id == market.token_id_up;
                    let side_book = if is_up { &top.token_id_up } else { &top.token_id_down };
                    info!(
                        "[IntervalSniper] position open: SL trigger={} best_bid={} (sell when bid <= trigger)",
                        fmt_price(Some(&sl.trigger_price)),
                        fmt_price(side_book.as_ref().and_then(|s| s.best_bid.as_ref()))
                    );
                }
            }
        }

        // Stop loss: if pending and best_bid <= trigger_price -> sell (FAK, retry at latest bid until filled)
        if state.config.enable_stop_loss {
            if let Some(ref sl) = state.pending_stop_loss {
                if !state.stop_loss_placed {
                    let is_up = sl.token_id == market.token_id_up;
                    let side_book = if is_up { &top.token_id_up } else { &top.token_id_down };
                    let best_bid = side_book.as_ref().and_then(|s| s.best_bid).unwrap_or(Decimal::ZERO);
                    if best_bid > Decimal::ZERO && best_bid <= sl.trigger_price {
                        let price = round_to_tick(best_bid);
                        let size = size_4_decimals(sl.size.clone());
                        let result = clob
                            .place_sell_order(
                                &sl.token_id,
                                price,
                                size.clone(),
                                state.config.stop_loss_time_in_force,
                            )
                            .await?;
                        if result.success {
                            info!("[IntervalSniper] stop loss executed @ {}", fmt_price(Some(&price)));
                            state.stop_loss_placed = true;
                        } else {
                            let is_no_match = result.error_msg.as_deref().map_or(false, |m| {
                                m.contains("no orders found to match") || m.contains("FAK") || m.contains("FOK")
                            });
                            if is_no_match {
                                info!("[IntervalSniper] stop loss no match, retrying FAK at latest bid until liquidated");
                                let mut filled = false;
                                for attempt in 0..FAK_MAX_RETRIES {
                                    tokio::time::sleep(Duration::from_millis(FAK_RETRY_DELAY_MS)).await;
                                    let top_retry = if let Some(ref ws) = state.ws_book {
                                        ws.get_top_of_book().await
                                    } else {
                                        match fetch_top_of_book(
                                            &http,
                                            &clob_host,
                                            &market.token_id_up,
                                            &market.token_id_down,
                                        )
                                        .await
                                        {
                                            Ok(t) => t,
                                            Err(_) => continue,
                                        }
                                    };
                                    let side_retry = if is_up { &top_retry.token_id_up } else { &top_retry.token_id_down };
                                    let bid = side_retry.as_ref().and_then(|s| s.best_bid).unwrap_or(Decimal::ZERO);
                                    if bid <= Decimal::ZERO {
                                        continue;
                                    }
                                    let price_retry = round_to_tick(bid);
                                    let result_retry = clob
                                        .place_sell_order(
                                            &sl.token_id,
                                            price_retry,
                                            size.clone(),
                                            crate::types::SellOrderTimeInForce::Fak,
                                        )
                                        .await?;
                                    if result_retry.success {
                                        info!("[IntervalSniper] stop loss executed @ {} (attempt {})", fmt_price(Some(&price_retry)), attempt + 1);
                                        state.stop_loss_placed = true;
                                        filled = true;
                                        break;
                                    }
                                    if result_retry.error_msg.as_deref().map_or(true, |m| !m.contains("no orders found to match")) {
                                        if let Some(msg) = result_retry.error_msg {
                                            warn!("[IntervalSniper] stop loss failed: {}", msg);
                                        }
                                        break;
                                    }
                                }
                                if !filled {
                                    warn!("[IntervalSniper] stop loss FAK retries exhausted, will retry next tick");
                                }
                            } else if let Some(msg) = result.error_msg {
                                warn!("[IntervalSniper] stop loss failed: {}", msg);
                            }
                        }
                    }
                }
            }
        }

        // Take profit: if pending and best_bid >= target_price -> sell (FAK, retry at latest bid until filled)
        if state.config.enable_auto_sell || state.config.auto_sell_at_max_price {
            if let Some(ref tp) = state.pending_auto_sell {
                if !state.auto_sell_placed {
                    let elapsed_sec = (now_ms_u - tp.placed_at_ms) / 1000;
                    if elapsed_sec >= state.config.min_seconds_after_buy_before_auto_sell as u64 {
                        let is_up = tp.token_id == market.token_id_up;
                        let side_book = if is_up { &top.token_id_up } else { &top.token_id_down };
                        let best_bid = side_book.as_ref().and_then(|s| s.best_bid).unwrap_or(Decimal::ZERO);
                        let target = tp.target_price - state.config.take_profit_price_margin;
                        if best_bid >= target {
                            let price = round_to_tick(best_bid.min(target + state.config.take_profit_price_margin));
                            let size = size_4_decimals(tp.size.clone());
                            let result = clob
                                .place_sell_order(
                                    &tp.token_id,
                                    price,
                                    size.clone(),
                                    state.config.take_profit_time_in_force,
                                )
                                .await?;
                            if result.success {
                                info!("[IntervalSniper] take profit executed @ {}", fmt_price(Some(&price)));
                                state.auto_sell_placed = true;
                            } else {
                                let is_no_match = result.error_msg.as_deref().map_or(false, |m| {
                                    m.contains("no orders found to match") || m.contains("FAK") || m.contains("FOK")
                                });
                                if is_no_match {
                                    info!("[IntervalSniper] take profit no match, retrying FAK at latest bid until liquidated");
                                    let mut filled = false;
                                    for attempt in 0..FAK_MAX_RETRIES {
                                        tokio::time::sleep(Duration::from_millis(FAK_RETRY_DELAY_MS)).await;
                                        let top_retry = if let Some(ref ws) = state.ws_book {
                                            ws.get_top_of_book().await
                                        } else {
                                            match fetch_top_of_book(
                                                &http,
                                                &clob_host,
                                                &market.token_id_up,
                                                &market.token_id_down,
                                            )
                                            .await
                                            {
                                                Ok(t) => t,
                                                Err(_) => continue,
                                            }
                                        };
                                        let side_retry = if is_up { &top_retry.token_id_up } else { &top_retry.token_id_down };
                                        let bid = side_retry.as_ref().and_then(|s| s.best_bid).unwrap_or(Decimal::ZERO);
                                        if bid < target {
                                            continue;
                                        }
                                        let price_retry = round_to_tick(bid.min(target + state.config.take_profit_price_margin));
                                        let result_retry = clob
                                            .place_sell_order(
                                                &tp.token_id,
                                                price_retry,
                                                size.clone(),
                                                crate::types::SellOrderTimeInForce::Fak,
                                            )
                                            .await?;
                                        if result_retry.success {
                                            info!("[IntervalSniper] take profit executed @ {} (attempt {})", fmt_price(Some(&price_retry)), attempt + 1);
                                            state.auto_sell_placed = true;
                                            filled = true;
                                            break;
                                        }
                                        if result_retry.error_msg.as_deref().map_or(true, |m| !m.contains("no orders found to match")) {
                                            if let Some(msg) = result_retry.error_msg {
                                                warn!("[IntervalSniper] take profit failed: {}", msg);
                                            }
                                            break;
                                        }
                                    }
                                    if !filled {
                                        warn!("[IntervalSniper] take profit FAK retries exhausted, will retry next tick");
                                    }
                                } else if let Some(msg) = result.error_msg {
                                    warn!("[IntervalSniper] take profit failed: {}", msg);
                                }
                            }
                        }
                    }
                }
            }
        }

        // Buy path: one order per interval, in window, side with higher best ask in range
        if !state.ordered_this_interval {
            let in_window = state.config.no_window_all_intervals
                || secs_to_close <= state.config.seconds_before_close as u64;
            let sec_since_start = 300u64.saturating_sub(secs_to_close);
            let min_after_open = state.config.min_seconds_after_market_open.max(3);
            let can_buy_after_open = sec_since_start >= min_after_open as u64;
            if let Some(switch_ms) = state.interval_switch_wall_time_ms {
                let elapsed_ms = now_ms_u.saturating_sub(switch_ms);
                if elapsed_ms < (min_after_open as u64) * 1000 {
                    // Skip first N seconds after interval switch
                    tokio::time::sleep(Duration::from_millis(loop_ms)).await;
                    continue;
                }
            }

            if in_window && can_buy_after_open {
                let min_order_size = CLOB_DEFAULT_MIN_ORDER_SIZE;
                if let Some((side, best_ask, size_available)) =
                    choose_side(&state.config, &top, min_order_size)
                {
                    let token_id = match side {
                        EntrySide::Up => &market.token_id_up,
                        EntrySide::Down => &market.token_id_down,
                    };
                    let effective_price = round_to_tick(
                        (best_ask + TICK_SIZE).min(state.config.max_buy_price),
                    );
                    let shares_left = state.config.size_shares - state.total_shares_this_interval;
                    let size = size_4_decimals(
                        shares_left.min(size_available).max(min_order_size),
                    );
                    let maker_amount = maker_amount_2_decimals(size.clone(), effective_price.clone());
                    if size >= min_order_size && size > Decimal::ZERO {
                        let order_type = OrderType::Fak;
                        let params = LimitOrderParams {
                            token_id: token_id.to_string(),
                            side: OrderSide::Buy,
                            price: effective_price.clone(),
                            size: size.clone(),
                            expiration_unix: None,
                            post_only: false,
                            fee_rate_bps: None,
                        };
                        let result = clob.place_limit_order(params, order_type).await?;
                        if result.success {
                            state.ordered_this_interval = true;
                            state.total_shares_this_interval += size.clone();
                            let entry_price = effective_price;
                            let entry_side = side;
                            state.last_buy_order = Some(LastBuyOrder {
                                token_id: token_id.to_string(),
                                side: entry_side,
                                size: size.clone(),
                                price: entry_price.clone(),
                                timestamp_ms: now_ms_u,
                            });
                            let target_price = if state.config.auto_sell_at_max_price {
                                dec!(0.99)
                            } else {
                                round_to_tick(
                                    (entry_price * (Decimal::ONE + state.config.auto_sell_profit_percent / dec!(100)))
                                        .min(Decimal::ONE),
                                )
                            };
                            state.pending_auto_sell = Some(PendingAutoSell {
                                token_id: token_id.to_string(),
                                target_price,
                                size: size.clone() * Decimal::from(state.config.auto_sell_quantity_percent) / dec!(100),
                                placed_at_ms: now_ms_u,
                            });
                            let trigger_price = round_to_tick(
                                entry_price * (Decimal::ONE - state.config.stop_loss_percent / dec!(100)),
                            );
                            state.pending_stop_loss = Some(PendingStopLoss {
                                token_id: token_id.to_string(),
                                entry_price: entry_price.clone(),
                                size: size * Decimal::from(state.config.stop_loss_quantity_percent) / dec!(100),
                                trigger_price,
                                placed_at_ms: now_ms_u,
                            });
                            state.auto_sell_placed = false;
                            state.stop_loss_placed = false;
                            info!(
                                "[IntervalSniper] buy {} @ {} size={}",
                                match entry_side {
                                    EntrySide::Up => "Up",
                                    EntrySide::Down => "Down",
                                },
                                fmt_decimal_2(&entry_price),
                                fmt_decimal_2(&size)
                            );
                        } else if let Some(msg) = result.error_msg {
                            warn!("[IntervalSniper] buy failed: {}", msg);
                        }
                    }
                }
            }
        }

        tokio::time::sleep(Duration::from_millis(loop_ms)).await;
    }
}
