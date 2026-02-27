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
/// Backoff delays (ms) when 400 not enough balance/allowance: cancel once then retry with these delays.
const BALANCE_RETRY_BACKOFF_MS: &[u64] = &[100, 200, 400];
/// Sell size precision (Polymarket CLOB): 4 decimals; quantity bought is rounded to this when selling TP/SL.
const SELL_SIZE_DECIMALS: u32 = 4;
/// Minimum valid sell size accepted by API in this bot.
const MIN_SELL_SIZE: Decimal = dec!(0.0001);
/// One base unit in shares (1e-6) — subtract from available so we never exceed balance after rounding.
const BALANCE_BUFFER_SHARES: Decimal = dec!(0.000001);

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

/// Maximum number of trades (buy + sell) allowed per interval; second trade only when the first was closed by SL.
const MAX_TRADES_PER_INTERVAL: u32 = 2;

struct RunnerState {
    config: Config,
    market: Option<ResolvedMarket>,
    /// WebSocket order book when connected; None = use REST only.
    ws_book: Option<ClobWsBook>,
    ordered_this_interval: bool,
    /// Number of buys executed this interval (max MAX_TRADES_PER_INTERVAL); re-entry only after SL.
    trades_this_interval: u32,
    /// True only when the last position in this interval was closed by SL; allows one re-entry (second trade).
    re_entry_allowed_after_sl: bool,
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

fn floor_to_decimals(x: Decimal, decimals: u32) -> Decimal {
    let factor = Decimal::from(10u64.pow(decimals));
    ((x * factor).trunc()) / factor
}

fn effective_sell_size(position_size: Decimal, available: Option<Decimal>) -> Decimal {
    let capped = available
        .map(|a| {
            // Leave 1 base unit headroom so encoded amount never exceeds balance after rounding
            let safe = (a - BALANCE_BUFFER_SHARES).max(Decimal::ZERO);
            position_size.min(safe)
        })
        .unwrap_or(position_size);
    floor_to_decimals(capped, SELL_SIZE_DECIMALS)
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

/// True if the API error indicates the position is already closed (e.g. already sold or no balance).
/// In that case we stop trying to place TP/SL and do not retry.
fn is_position_closed_error(msg: Option<&str>) -> bool {
    msg.map_or(false, |m| {
        let lower = m.to_lowercase();
        lower.contains("not enough balance")
            || lower.contains("allowance")
            || lower.contains("insufficient balance")
    })
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
    let clob_host = std::env::var("POLYMARKET_CLOB_HOST")
        .unwrap_or_else(|_| "https://clob.polymarket.com".to_string());
    let http = Client::builder().timeout(Duration::from_secs(10)).build()?;
    let clob = Arc::new(crate::clob::create_clob_client(config.dry_run)?);

    let mut state = RunnerState {
        market: None,
        ws_book: None,
        config: config.clone(),
        ordered_this_interval: false,
        trades_this_interval: 0,
        re_entry_allowed_after_sl: false,
        total_shares_this_interval: Decimal::ZERO,
        last_buy_order: None,
        pending_auto_sell: None,
        pending_stop_loss: None,
        auto_sell_placed: false,
        stop_loss_placed: false,
        interval_switch_wall_time_ms: None,
    };

    info!(
        "[IntervalSniper] started dry_run={} slug={}",
        config.dry_run, config.market_slug
    );

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
            || state
                .market
                .as_ref()
                .map(|m| now_u >= m.close_time_unix)
                .unwrap_or(true)
            || state
                .market
                .as_ref()
                .map(|m| current_slug != m.slug)
                .unwrap_or(true);

        if need_new_market {
            match fetch_market_by_slug(&http, &config.gamma_base_url, &current_slug).await {
                Ok(market) => {
                    state.ws_book = None; // drop previous WS before creating new
                    let ws_url = ClobWsBook::ws_url_from_rest_host(&clob_host);
                    match ClobWsBook::connect(&ws_url, &market.token_id_up, &market.token_id_down)
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
                    state.trades_this_interval = 0;
                    state.re_entry_allowed_after_sl = false;
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
                        if up_id.len() > 12 {
                            &up_id[..12]
                        } else {
                            up_id
                        },
                        if down_id.len() > 12 {
                            &down_id[..12]
                        } else {
                            down_id
                        }
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
                    let side_book = if is_up {
                        &top.token_id_up
                    } else {
                        &top.token_id_down
                    };
                    info!(
                        "[IntervalSniper]  POS   TP   target={}  best_bid={}  (sell when bid >= target)",
                        fmt_price(Some(&tp.target_price)),
                        fmt_price(side_book.as_ref().and_then(|s| s.best_bid.as_ref()))
                    );
                }
            }
            if let Some(ref sl) = state.pending_stop_loss {
                if !state.stop_loss_placed {
                    let is_up = sl.token_id == market.token_id_up;
                    let side_book = if is_up {
                        &top.token_id_up
                    } else {
                        &top.token_id_down
                    };
                    info!(
                        "[IntervalSniper]  POS   SL   trigger={}  best_bid={}  (sell when bid <= trigger)",
                        fmt_price(Some(&sl.trigger_price)),
                        fmt_price(side_book.as_ref().and_then(|s| s.best_bid.as_ref()))
                    );
                }
            }
        }

        // Stop loss: if pending and best_bid <= trigger_price -> sell (FAK, retry at latest bid until filled).
        // Always use position.token_id (the token we bought), never derive from book; sell_size = min(position.size, available).
        if state.config.enable_stop_loss {
            if let Some(ref sl) = state.pending_stop_loss {
                if !state.stop_loss_placed {
                    // Use book only for best_bid; token to sell is always position.token_id.
                    let is_up = sl.token_id == market.token_id_up;
                    let side_book = if is_up {
                        &top.token_id_up
                    } else {
                        &top.token_id_down
                    };
                    let best_bid = side_book
                        .as_ref()
                        .and_then(|s| s.best_bid)
                        .unwrap_or(Decimal::ZERO);
                    if best_bid > Decimal::ZERO && best_bid <= sl.trigger_price {
                        // Cancel any open orders for this token so balance is not locked (e.g. by a GTC TP order).
                        match clob.cancel_orders_for_token(&sl.token_id).await {
                            Err(e) => warn!("[IntervalSniper] cancel orders before SL failed: {} (continuing with sell)", e),
                            Ok(res) if !res.not_canceled.is_empty() => {
                                warn!("[IntervalSniper] cancel before SL: {} order(s) not canceled, balance may still be locked", res.not_canceled.len());
                            }
                            _ => {}
                        }
                        // Brief delay so CLOB/chain sees balance freed after cancel before we place sell.
                        tokio::time::sleep(Duration::from_millis(350)).await;
                        // SELL FAK must cross: limit_price = best_bid (or best_bid - tick). Use best_bid so order matches.
                        let price = round_to_tick(best_bid);
                        let position_size_real = sl.size.clone();
                        let available = clob
                            .get_available_balance(&sl.token_id)
                            .await
                            .ok()
                            .flatten();
                        let size = effective_sell_size(position_size_real, available.clone());
                        if size < MIN_SELL_SIZE {
                            warn!(
                                "[IntervalSniper] SL available too low to sell: token_id={} available_shares={:?} effective_sell_size={} min_sell_size={}",
                                sl.token_id,
                                available,
                                size,
                                MIN_SELL_SIZE
                            );
                            tokio::time::sleep(Duration::from_millis(loop_ms)).await;
                            continue;
                        }
                        let result = clob
                            .place_sell_order(
                                &sl.token_id,
                                price,
                                size.clone(),
                                state.config.stop_loss_time_in_force,
                            )
                            .await?;
                        if result.success {
                            info!(
                                "[IntervalSniper]  SELL  SL   precio_compra={}  precio_venta={}   (stop loss) — position closed, re-entry allowed if price in range (trades this interval: {}/{})",
                                fmt_decimal_2(&sl.entry_price),
                                fmt_decimal_2(&price),
                                state.trades_this_interval,
                                MAX_TRADES_PER_INTERVAL
                            );
                            state.stop_loss_placed = true;
                            state.auto_sell_placed = true;
                            state.re_entry_allowed_after_sl = true; // allow second trade this interval only after SL
                            // Clear position state so we can re-enter at target price (max 2 trades per interval).
                            state.pending_auto_sell = None;
                            state.pending_stop_loss = None;
                            state.last_buy_order = None;
                            state.total_shares_this_interval = Decimal::ZERO; // re-entry can use full size again
                        } else {
                            if result.http_status == Some(400) {
                                let ba = clob
                                    .get_balance_allowance(&sl.token_id)
                                    .await
                                    .unwrap_or_else(|e| format!("error: {}", e));
                                info!(
                                    "[IntervalSniper] SL 400 — token_id={} intento_sell_size={} balance_allowance (CONDITIONAL)={}",
                                    sl.token_id, size, ba
                                );
                            }
                            let is_no_match = result.error_msg.as_deref().map_or(false, |m| {
                                m.contains("no orders found to match")
                                    || m.contains("FAK")
                                    || m.contains("FOK")
                            });
                            // On balance/allowance error: cancel open orders once, then retry with backoff (100→200→400 ms), selling position.size.
                            let is_balance_error =
                                is_position_closed_error(result.error_msg.as_deref());
                            if is_no_match || is_balance_error {
                                if is_balance_error {
                                    info!("[IntervalSniper] stop loss: balance/allowance error, canceling open orders once and retrying with backoff");
                                } else {
                                    info!("[IntervalSniper] stop loss no match, retrying FAK at latest bid until liquidated");
                                }
                                let mut filled = false;
                                let mut canceled_once_for_balance = false;
                                let mut attempt: u32 = 0;
                                loop {
                                    attempt += 1;
                                    let delay_ms = if is_balance_error {
                                        BALANCE_RETRY_BACKOFF_MS
                                            .get((attempt as usize).saturating_sub(1))
                                            .copied()
                                            .unwrap_or(400)
                                    } else {
                                        FAK_RETRY_DELAY_MS
                                    };
                                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                                    if is_balance_error && !canceled_once_for_balance {
                                        let _ = clob.cancel_orders_for_token(&sl.token_id).await;
                                        canceled_once_for_balance = true;
                                        tokio::time::sleep(Duration::from_millis(350)).await;
                                    }
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
                                    let side_retry = if is_up {
                                        &top_retry.token_id_up
                                    } else {
                                        &top_retry.token_id_down
                                    };
                                    let bid = side_retry
                                        .as_ref()
                                        .and_then(|s| s.best_bid)
                                        .unwrap_or(Decimal::ZERO);
                                    if bid <= Decimal::ZERO {
                                        continue;
                                    }
                                    let position_size_real = sl.size.clone();
                                    let available = clob
                                        .get_available_balance(&sl.token_id)
                                        .await
                                        .ok()
                                        .flatten();
                                    let size_retry =
                                        effective_sell_size(position_size_real, available.clone());
                                    if size_retry < MIN_SELL_SIZE {
                                        warn!(
                                            "[IntervalSniper] SL available too low to sell on retry: token_id={} attempt={} available_shares={:?} effective_sell_size={} min_sell_size={}",
                                            sl.token_id,
                                            attempt,
                                            available,
                                            size_retry,
                                            MIN_SELL_SIZE
                                        );
                                        break;
                                    }
                                    let price_retry = round_to_tick(bid);
                                    let result_retry = clob
                                        .place_sell_order(
                                            &sl.token_id,
                                            price_retry,
                                            size_retry.clone(),
                                            crate::types::SellOrderTimeInForce::Fak,
                                        )
                                        .await?;
                                    if result_retry.success {
                                        info!(
                                            "[IntervalSniper]  SELL  SL   precio_compra={}  precio_venta={}   (attempt {}) — position closed, re-entry allowed if price in range (trades this interval: {}/{})",
                                            fmt_decimal_2(&sl.entry_price),
                                            fmt_decimal_2(&price_retry),
                                            attempt,
                                            state.trades_this_interval,
                                            MAX_TRADES_PER_INTERVAL
                                        );
                                        state.stop_loss_placed = true;
                                        state.auto_sell_placed = true;
                                        state.re_entry_allowed_after_sl = true; // allow second trade this interval only after SL
                                        state.pending_auto_sell = None;
                                        state.pending_stop_loss = None;
                                        state.last_buy_order = None;
                                        state.total_shares_this_interval = Decimal::ZERO;
                                        filled = true;
                                        break;
                                    }
                                    // Balance/allowance: we already canceled once; just backoff and retry with position.size (no re-cancel).
                                    if is_position_closed_error(result_retry.error_msg.as_deref()) {
                                        warn!("[IntervalSniper] stop loss retry attempt {}: balance/allowance error (cancel already done), retrying with backoff", attempt);
                                        continue;
                                    }
                                    if result_retry.http_status == Some(400) {
                                        let ba = clob
                                            .get_balance_allowance(&sl.token_id)
                                            .await
                                            .unwrap_or_else(|e| format!("error: {}", e));
                                        info!(
                                            "[IntervalSniper] SL retry 400 — token_id={} intento_sell_size={} balance_allowance (CONDITIONAL)={}",
                                            sl.token_id, size_retry, ba
                                        );
                                    }
                                    if result_retry
                                        .error_msg
                                        .as_deref()
                                        .map_or(true, |m| !m.contains("no orders found to match"))
                                    {
                                        if let Some(msg) = result_retry.error_msg {
                                            warn!("[IntervalSniper]  FAIL  SL    {}", msg);
                                        }
                                        break;
                                    }
                                }
                            } else if let Some(msg) = result.error_msg {
                                warn!("[IntervalSniper]  FAIL  SL    {}", msg);
                            }
                        }
                    }
                }
            }
        }

        // Take profit: if pending and best_bid >= target_price -> sell (FAK, retry at latest best_bid until filled).
        // Always use position.token_id (the token we bought); sell_size = min(position.size, available).
        if state.config.enable_auto_sell || state.config.auto_sell_at_max_price {
            if let Some(ref tp) = state.pending_auto_sell {
                if !state.auto_sell_placed {
                    let elapsed_sec = (now_ms_u - tp.placed_at_ms) / 1000;
                    if elapsed_sec >= state.config.min_seconds_after_buy_before_auto_sell as u64 {
                        // Use book only for best_bid; token to sell is always position.token_id.
                        let is_up = tp.token_id == market.token_id_up;
                        let side_book = if is_up {
                            &top.token_id_up
                        } else {
                            &top.token_id_down
                        };
                        let best_bid = side_book
                            .as_ref()
                            .and_then(|s| s.best_bid)
                            .unwrap_or(Decimal::ZERO);
                        let target = tp.target_price - state.config.take_profit_price_margin;
                        if best_bid >= target {
                            // Cancel any open orders for this token so balance is not locked (e.g. by a GTC SL order).
                            match clob.cancel_orders_for_token(&tp.token_id).await {
                                Err(e) => warn!("[IntervalSniper] cancel orders before TP failed: {} (continuing with sell)", e),
                                Ok(res) if !res.not_canceled.is_empty() => {
                                    warn!("[IntervalSniper] cancel before TP: {} order(s) not canceled, balance may still be locked", res.not_canceled.len());
                                }
                                _ => {}
                            }
                            // Brief delay so CLOB/chain sees balance freed after cancel before we place sell.
                            tokio::time::sleep(Duration::from_millis(350)).await;
                            let position_size_real = tp.size.clone();
                            let available = clob
                                .get_available_balance(&tp.token_id)
                                .await
                                .ok()
                                .flatten();
                            let size = effective_sell_size(position_size_real, available.clone());
                            if size < MIN_SELL_SIZE {
                                warn!(
                                "[IntervalSniper] TP available too low to sell: token_id={} available_shares={:?} effective_sell_size={} min_sell_size={}",
                                tp.token_id,
                                available,
                                size,
                                MIN_SELL_SIZE
                            );
                                tokio::time::sleep(Duration::from_millis(loop_ms)).await;
                                continue;
                            }
                            // SELL FAK must cross: use best_bid so order matches; avoid posting above bid.
                            let price = match state.config.take_profit_time_in_force {
                                crate::types::SellOrderTimeInForce::Fak => round_to_tick(best_bid),
                                _ => round_to_tick(
                                    best_bid.min(target + state.config.take_profit_price_margin),
                                ),
                            };
                            let result = clob
                                .place_sell_order(
                                    &tp.token_id,
                                    price,
                                    size.clone(),
                                    state.config.take_profit_time_in_force,
                                )
                                .await?;
                            if result.success {
                                let buy_price = state.last_buy_order.as_ref().map(|o| fmt_decimal_2(&o.price)).unwrap_or_else(|| "-".to_string());
                                info!(
                                    "[IntervalSniper]  SELL  TP   precio_compra={}  precio_venta={}   (take profit) — position closed (trades this interval: {}/{})",
                                    buy_price,
                                    fmt_decimal_2(&price),
                                    state.trades_this_interval,
                                    MAX_TRADES_PER_INTERVAL
                                );
                                state.auto_sell_placed = true;
                                state.stop_loss_placed = true;
                                state.re_entry_allowed_after_sl = false; // no re-entry after TP, only after SL
                                state.pending_auto_sell = None;
                                state.pending_stop_loss = None;
                                state.last_buy_order = None;
                                state.total_shares_this_interval = Decimal::ZERO;
                            } else {
                                if result.http_status == Some(400) {
                                    let ba = clob
                                        .get_balance_allowance(&tp.token_id)
                                        .await
                                        .unwrap_or_else(|e| format!("error: {}", e));
                                    info!(
                                        "[IntervalSniper] TP 400 — token_id={} intento_sell_size={} balance_allowance (CONDITIONAL)={}",
                                        tp.token_id, size, ba
                                    );
                                }
                                let is_no_match = result.error_msg.as_deref().map_or(false, |m| {
                                    m.contains("no orders found to match")
                                        || m.contains("FAK")
                                        || m.contains("FOK")
                                });
                                let is_balance_error =
                                    is_position_closed_error(result.error_msg.as_deref());
                                if is_no_match || is_balance_error {
                                    if is_balance_error {
                                        info!("[IntervalSniper] take profit: balance/allowance error, canceling open orders once and retrying with backoff");
                                    } else {
                                        info!("[IntervalSniper] take profit no match, retrying FAK at latest bid until liquidated");
                                    }
                                    let mut filled = false;
                                    let mut canceled_once_for_balance = false;
                                    let mut attempt: u32 = 0;
                                    loop {
                                        attempt += 1;
                                        let delay_ms = if is_balance_error {
                                            BALANCE_RETRY_BACKOFF_MS
                                                .get((attempt as usize).saturating_sub(1))
                                                .copied()
                                                .unwrap_or(400)
                                        } else {
                                            FAK_RETRY_DELAY_MS
                                        };
                                        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                                        if is_balance_error && !canceled_once_for_balance {
                                            let _ =
                                                clob.cancel_orders_for_token(&tp.token_id).await;
                                            canceled_once_for_balance = true;
                                            tokio::time::sleep(Duration::from_millis(350)).await;
                                        }
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
                                        let side_retry = if is_up {
                                            &top_retry.token_id_up
                                        } else {
                                            &top_retry.token_id_down
                                        };
                                        let bid = side_retry
                                            .as_ref()
                                            .and_then(|s| s.best_bid)
                                            .unwrap_or(Decimal::ZERO);
                                        if bid < target {
                                            continue;
                                        }
                                        let position_size_real = tp.size.clone();
                                        let available = clob
                                            .get_available_balance(&tp.token_id)
                                            .await
                                            .ok()
                                            .flatten();
                                        let size_retry = effective_sell_size(
                                            position_size_real,
                                            available.clone(),
                                        );
                                        if size_retry < MIN_SELL_SIZE {
                                            warn!(
                                                "[IntervalSniper] TP available too low to sell on retry: token_id={} attempt={} available_shares={:?} effective_sell_size={} min_sell_size={}",
                                                tp.token_id,
                                                attempt,
                                                available,
                                                size_retry,
                                                MIN_SELL_SIZE
                                            );
                                            break;
                                        }
                                        let price_retry = round_to_tick(bid);
                                        let result_retry = clob
                                            .place_sell_order(
                                                &tp.token_id,
                                                price_retry,
                                                size_retry.clone(),
                                                crate::types::SellOrderTimeInForce::Fak,
                                            )
                                            .await?;
                                        if result_retry.success {
                                            let buy_price_tp = state.last_buy_order.as_ref().map(|o| fmt_decimal_2(&o.price)).unwrap_or_else(|| "-".to_string());
                                            info!(
                                                "[IntervalSniper]  SELL  TP   precio_compra={}  precio_venta={}   (attempt {}) — position closed (trades this interval: {}/{})",
                                                buy_price_tp,
                                                fmt_decimal_2(&price_retry),
                                                attempt,
                                                state.trades_this_interval,
                                                MAX_TRADES_PER_INTERVAL
                                            );
                                            state.auto_sell_placed = true;
                                            state.stop_loss_placed = true;
                                            state.re_entry_allowed_after_sl = false; // no re-entry after TP, only after SL
                                            state.pending_auto_sell = None;
                                            state.pending_stop_loss = None;
                                            state.last_buy_order = None;
                                            state.total_shares_this_interval = Decimal::ZERO;
                                            filled = true;
                                            break;
                                        }
                                        if is_position_closed_error(
                                            result_retry.error_msg.as_deref(),
                                        ) {
                                            warn!("[IntervalSniper] take profit retry attempt {}: balance/allowance error (cancel already done), retrying with backoff", attempt);
                                            continue;
                                        }
                                        if result_retry.http_status == Some(400) {
                                            let ba = clob
                                                .get_balance_allowance(&tp.token_id)
                                                .await
                                                .unwrap_or_else(|e| format!("error: {}", e));
                                            info!(
                                                "[IntervalSniper] TP retry 400 — token_id={} intento_sell_size={} balance_allowance (CONDITIONAL)={}",
                                                tp.token_id, size_retry, ba
                                            );
                                        }
                                        if result_retry.error_msg.as_deref().map_or(true, |m| {
                                            !m.contains("no orders found to match")
                                        }) {
                                            if let Some(msg) = result_retry.error_msg {
                                                warn!("[IntervalSniper]  FAIL  TP    {}", msg);
                                            }
                                            break;
                                        }
                                    }
                                } else if let Some(msg) = result.error_msg {
                                    warn!("[IntervalSniper]  FAIL  TP    {}", msg);
                                }
                            }
                        }
                    }
                }
            }
        }

        // Buy path: up to MAX_TRADES_PER_INTERVAL per interval; re-entry only after SL (not after TP).
        let no_open_position = state.pending_auto_sell.is_none() && state.pending_stop_loss.is_none();
        let can_buy = no_open_position
            && (state.trades_this_interval == 0
                || (state.trades_this_interval == 1 && state.re_entry_allowed_after_sl));
        if can_buy {
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
                    // Enforce price within [min_buy_price, max_buy_price]: we cross the spread (best_ask + 1 tick)
                    // but never go below min nor above max. FAK must cross: limit_price >= best_ask (or "no orders found").
                    let effective_price = round_to_tick(
                        (best_ask + TICK_SIZE)
                            .max(state.config.min_buy_price)
                            .min(state.config.max_buy_price),
                    );
                    let effective_price = effective_price.max(best_ask);
                    let shares_left = state.config.size_shares - state.total_shares_this_interval;
                    // Cap at shares_left so we never order more than configured size (e.g. exactly 7 shares).
                    // Round to 2 decimals so we never send 7.24000001 when user wants 7.
                    let size = size_4_decimals(
                        shares_left
                            .min(size_available)
                            .max(min_order_size)
                            .round_dp(2),
                    );
                    let maker_amount =
                        maker_amount_2_decimals(size.clone(), effective_price.clone());
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
                            // Position must use actual filled_size from CLOB (FAK can be partial; TP/SL must sell only what we have).
                            let filled = result
                                .filled_size
                                .filter(|s| *s > Decimal::ZERO && *s >= size.clone() * dec!(0.01))
                                .unwrap_or(size.clone());
                            let filled = filled.min(size.clone());
                            state.ordered_this_interval = true;
                            state.trades_this_interval += 1;
                            state.total_shares_this_interval += filled.clone();
                            let entry_price = effective_price;
                            let entry_side = side;
                            state.last_buy_order = Some(LastBuyOrder {
                                token_id: token_id.to_string(),
                                side: entry_side,
                                size: filled.clone(),
                                price: entry_price.clone(),
                                timestamp_ms: now_ms_u,
                            });
                            let target_price = if state.config.auto_sell_at_max_price {
                                dec!(0.99)
                            } else {
                                round_to_tick(state.config.take_profit_price)
                            };
                            // Use actual bought quantity (filled), adjusted to Polymarket sell size decimals (4).
                            let base_sell_size = floor_to_decimals(
                                filled.clone().min(state.config.size_shares),
                                SELL_SIZE_DECIMALS,
                            )
                            .max(MIN_SELL_SIZE);
                            let pct_tp =
                                Decimal::from(state.config.auto_sell_quantity_percent) / dec!(100);
                            let pct_sl =
                                Decimal::from(state.config.stop_loss_quantity_percent) / dec!(100);
                            let tp_size = floor_to_decimals(base_sell_size * pct_tp, SELL_SIZE_DECIMALS)
                                .max(MIN_SELL_SIZE)
                                .min(base_sell_size);
                            let sl_size = floor_to_decimals(base_sell_size * pct_sl, SELL_SIZE_DECIMALS)
                                .max(MIN_SELL_SIZE)
                                .min(base_sell_size);
                            state.pending_auto_sell = Some(PendingAutoSell {
                                token_id: token_id.to_string(),
                                target_price,
                                size: tp_size,
                                placed_at_ms: now_ms_u,
                            });
                            let trigger_price = round_to_tick(state.config.stop_loss_price);
                            state.pending_stop_loss = Some(PendingStopLoss {
                                token_id: token_id.to_string(),
                                entry_price: entry_price.clone(),
                                size: sl_size,
                                trigger_price,
                                placed_at_ms: now_ms_u,
                            });
                            state.auto_sell_placed = false;
                            state.stop_loss_placed = false;
                            let side_str = match entry_side {
                                EntrySide::Up => "Up  ",
                                EntrySide::Down => "Down",
                            };
                            info!(
                                "[IntervalSniper]  BUY   {}  precio_compra={}   size={}   TP size={} ({}%)   SL size={} ({}%)",
                                side_str,
                                fmt_decimal_2(&entry_price),
                                fmt_decimal_2(&state.last_buy_order.as_ref().unwrap().size),
                                fmt_decimal_2(&tp_size),
                                state.config.auto_sell_quantity_percent,
                                fmt_decimal_2(&sl_size),
                                state.config.stop_loss_quantity_percent
                            );
                        } else if let Some(msg) = result.error_msg {
                            warn!("[IntervalSniper]  FAIL  BUY   {}", msg);
                        }
                    }
                }
            }
        }

        tokio::time::sleep(Duration::from_millis(loop_ms)).await;
    }
}
