//! Main loop: interval switch, top-of-book, buy in range, TP/SL.

#[allow(unused_imports)]
use crate::clob::{AssetType, BalanceAllowanceParams, ClobClient, LimitOrderParams, OrderSide, OrderType};
use crate::clob_ws_book::ClobWsBook;
use crate::clob_ws_user::ClobWsUser;
use crate::config::{current_5min_slug, load_config};
use crate::market::fetch_market_by_slug;
use crate::orderbook::fetch_top_of_book;
use crate::redeem;
use crate::session_log::{ExitType, SessionLog};
use crate::telegram_log::TelegramLog;
use crate::types::{
    Config, EntrySide, LastBuyOrder, PendingAutoSell, PendingStopLoss, ResolvedMarket, TopOfBook,
    OrderStrategy,
};
use anyhow::Result;
use ethers::signers::Signer;
use reqwest::Client;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::str::FromStr;
use std::sync::Arc;
use std::time::{Duration, Instant, UNIX_EPOCH};
use tracing::{debug, info, trace, warn};

const TICK_SIZE: Decimal = dec!(0.01);
const CLOB_DEFAULT_MIN_ORDER_SIZE: Decimal = dec!(5);
/// Log order book and TP/SL status every this many loop ticks (e.g. 10 → ~1s if loop_ms=100).
const LOG_BOOK_EVERY_TICKS: u64 = 10;
/// Delay between SL FAK retries on no-match or transient errors (ms).
#[allow(dead_code)]
const SL_FOK_RETRY_DELAY_MS: u64 = 20;
/// When SL limit is placed, recheck bid this often (ms) and cancel+replace if bid dropped — fast follow-down.
const SL_FOLLOW_DOWN_MS: u64 = 50;
/// Max follow-down retries so we don't block the main loop (e.g. 20 × 50ms = 1s of tight follow).
const SL_FOLLOW_DOWN_MAX_RETRIES: u32 = 20;
/// Sell size precision (Polymarket CLOB): 4 decimals; quantity bought is rounded to this when selling TP/SL.
const SELL_SIZE_DECIMALS: u32 = 4;
/// Minimum valid sell size accepted by API in this bot.
const MIN_SELL_SIZE: Decimal = dec!(0.0001);
/// Below this we consider position closed (dust); avoids spamming the API with tiny amounts the exchange rejects.
const DUST_THRESHOLD: Decimal = dec!(0.001);
/// When SL limit order is resting but available balance for the token is <= this, treat as position closed (fill detected via balance; WS/REST may have missed the event).
const SL_BALANCE_DUST_CLOSE: Decimal = dec!(0.05);
/// Extra margin above SL trigger so we keep trying to place the SL limit on every tick while price is at or slightly above trigger (avoids missing the window if price bounces).
const SL_TRIGGER_MARGIN: Decimal = dec!(0.01);
/// Polymarket may reject small sell sizes with "invalid amounts"; below this treat as dust and consider position closed.
const TP_SL_DUST_SIZE: Decimal = dec!(0.01);
/// API reported fill below this is treated like 0 — run full WS/REST reconciliation (exchange often filled fully).
#[allow(dead_code)]
const TINY_FILL_THRESHOLD: Decimal = dec!(0.01);
/// One base unit in shares (1e-6) — subtract from available so we never exceed balance after rounding.
const BALANCE_BUFFER_SHARES: Decimal = dec!(0.000001);
/// Retry interval (ms) when placing TP/SL after WS fill until REST balance reflects the fill or interval ends.
const TP_SL_BALANCE_RETRY_MS: u64 = 200;
/// Interval (ms) for logging CLOB balance and buy→balance-reflected delay.
const BALANCE_LOG_INTERVAL_MS: u64 = 1000;
/// When WS is present: wait this long before using REST get_order/balance fallback. WS can take 1–3 ticks to deliver the fill; this gives it priority so we log "fill first: user WS" when possible.
const PENDING_GTC_REST_CHECK_MS: u64 = 400;
/// When no WS user channel, check REST every tick from buy (0 = no delay).
const PENDING_GTC_NO_WS_FALLBACK_MS: u64 = 0;

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

/// Update per-interval min/max best_bid and last_best_bid_for_position from current book.
fn update_interval_bids(
    state: &mut RunnerState,
    token_id_up: &str,
    _token_id_down: &str,
    top: &TopOfBook,
) {
    if let Some(ref up) = top.token_id_up {
        if let Some(bid) = up.best_bid {
            state.interval_min_bid_up = Some(
                state.interval_min_bid_up.map(|m| m.min(bid)).unwrap_or(bid),
            );
            state.interval_max_bid_up = Some(
                state.interval_max_bid_up.map(|m| m.max(bid)).unwrap_or(bid),
            );
        }
    }
    if let Some(ref down) = top.token_id_down {
        if let Some(bid) = down.best_bid {
            state.interval_min_bid_down = Some(
                state.interval_min_bid_down.map(|m| m.min(bid)).unwrap_or(bid),
            );
            state.interval_max_bid_down = Some(
                state.interval_max_bid_down.map(|m| m.max(bid)).unwrap_or(bid),
            );
        }
    }
    if state.pending_auto_sell.is_some() || state.pending_stop_loss.is_some() {
        let token_id = state
            .pending_stop_loss
            .as_ref()
            .map(|s| s.token_id.as_str())
            .or_else(|| state.pending_auto_sell.as_ref().map(|t| t.token_id.as_str()));
        if let Some(tid) = token_id {
            state.last_best_bid_for_position = if tid == token_id_up {
                top.token_id_up.as_ref().and_then(|s| s.best_bid)
            } else {
                top.token_id_down.as_ref().and_then(|s| s.best_bid)
            };
        }
    }
}

/// Maximum number of trades (buy + sell) allowed per interval; second trade only when the first was closed by SL.
#[allow(dead_code)]
const MAX_TRADES_PER_INTERVAL: u32 = 2;

struct RunnerState {
    config: Config,
    market: Option<ResolvedMarket>,
    /// WebSocket order book when connected; None = use REST only.
    ws_book: Option<ClobWsBook>,
    /// WebSocket user channel for real-time order/trade updates (fills without waiting for balance).
    ws_user: Option<Arc<ClobWsUser>>,
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
    /// Order ID of the GTC TP limit order when placed (cancel when price drops below entry).
    tp_limit_order_id: Option<String>,
    /// Size actually placed for the current TP limit order (used for fill detection when there was partial SL).
    tp_placed_size: Option<Decimal>,
    /// Total size filled at TP so far this position (partial TP then price retrace → track for remaining size).
    tp_cumulative_filled: Decimal,
    /// Filled size of current TP order (for delta from WS; HFT priority).
    tp_last_order_filled: Decimal,
    /// Consecutive TP limit placement failures due to balance/allowance errors.
    tp_limit_balance_retries: u32,
    /// Last wall time (ms) we did REST fallback check for TP order status; throttle to every 5s.
    tp_limit_last_rest_check_ms: Option<u64>,
    /// Order ID of the GTC SL limit order (cancel and replace at new best_bid when bid drops).
    sl_limit_order_id: Option<String>,
    /// Price at which the current SL limit order was placed (to detect when to cancel and replace).
    sl_limit_order_price: Option<Decimal>,
    /// Total size filled so far in this SL exit (across possibly multiple limit orders); 100% = position closed.
    sl_cumulative_filled: Decimal,
    /// Filled size already accounted for the current SL order (to compute delta from WS).
    sl_last_order_filled: Decimal,
    /// Last wall time (ms) we did REST fallback check for SL order status; throttle to every 5s.
    sl_limit_last_rest_check_ms: Option<u64>,
    interval_switch_wall_time_ms: Option<u64>,
    /// Session log (JSONL) when MM_SESSION_LOG=true.
    session_log: Option<SessionLog>,
    /// Per-interval min/max best_bid for session log (ranged 0.01–0.99).
    interval_min_bid_up: Option<Decimal>,
    interval_max_bid_up: Option<Decimal>,
    interval_min_bid_down: Option<Decimal>,
    interval_max_bid_down: Option<Decimal>,
    /// Last best_bid for position side (for MARKET_CLOSE exit_price).
    last_best_bid_for_position: Option<Decimal>,
    /// Last time we logged CLOB balance (ms); log every BALANCE_LOG_INTERVAL_MS.
    last_balance_log_ms: Option<u64>,
    /// When the CLOB balance first reflected the last buy (ms); used to log delay since purchase.
    balance_reflected_at_ms: Option<u64>,
    /// True after we logged "delay desde compra hasta que se reflejó en CLOB" once this position; avoids spam.
    balance_delay_clob_logged: bool,
    /// Last (Up, Down) balance we logged; only log again when balance changes.
    last_logged_balance_up: Option<Decimal>,
    last_logged_balance_down: Option<Decimal>,
    /// GTC order placed but no filled_size in response: wait for fill from ws_user or balance.
    pending_gtc_order_id: Option<String>,
    pending_gtc_token_id: Option<String>,
    pending_gtc_side: Option<EntrySide>,
    pending_gtc_price: Option<Decimal>,
    pending_gtc_requested_size: Option<Decimal>,
    pending_gtc_timestamp_ms: Option<u64>,
    /// Last filled size we observed for the pending GTC (for logging partial fill deltas).
    pending_gtc_last_observed_filled: Option<Decimal>,
    /// Per-tick fill deltas for the pending GTC order (for log line "N partials: a, b, c").
    pending_gtc_fill_deltas: Vec<Decimal>,
    /// Cached result of get_available_balance per token (TTL 3s). (token_id, value, instant).
    allowance_cache: Option<(String, Decimal, Instant)>,
    /// Optional Telegram log: enqueues messages in background (no delay in hot path).
    telegram: Option<TelegramLog>,
    /// Keep the Telegram sender task alive.
    telegram_handle: Option<tokio::task::JoinHandle<()>>,
}

/// Parse balance from balance-allowance raw JSON; balance is in 6-decimal raw units (6620 = 0.00662 shares).
fn format_balance_allowance_hint(raw: &str) -> String {
    let json: serde_json::Value = match serde_json::from_str(raw) {
        Ok(j) => j,
        Err(_) => return String::new(),
    };
    let raw_val: Decimal = match json.get("balance") {
        Some(serde_json::Value::String(s)) => match Decimal::from_str(s) {
            Ok(d) => d,
            Err(_) => return String::new(),
        },
        Some(serde_json::Value::Number(n)) => match n.as_u64().map(Decimal::from).or_else(|| n.as_i64().map(Decimal::from)) {
            Some(d) => d,
            None => return String::new(),
        },
        _ => return String::new(),
    };
    let shares = raw_val / dec!(1000000);
    format!(
        " (balance {} raw = {} shares — if low, CLOB has not updated after fill yet)",
        raw_val,
        fmt_decimal_2(&shares)
    )
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

fn effective_sell_size(
    position_size: Decimal,
    available: Option<Decimal>,
    min_order_size: Decimal,
) -> Decimal {
    let capped = available
        .map(|a| {
            // Solo restar buffer si available es significativo
            let safe = if a > dec!(0.01) {
                (a - BALANCE_BUFFER_SHARES).max(Decimal::ZERO)
            } else {
                a
            };
            position_size.min(safe)
        })
        .unwrap_or(position_size);

    let result = floor_to_decimals(capped, SELL_SIZE_DECIMALS);

    // Validar que result sea >= MIN_SELL_SIZE
    if result < MIN_SELL_SIZE {
        if result >= min_order_size - dec!(0.01)
            && available.map_or(false, |a| a >= min_order_size)
        {
            min_order_size
        } else {
            Decimal::ZERO // Retornar 0 explícito en lugar de dust
        }
    } else if result < min_order_size
        && result >= min_order_size - dec!(0.01)
        && available.map_or(false, |a| a >= min_order_size)
    {
        min_order_size
    } else {
        result
    }
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

/// True if the API error is "invalid amounts, maker and taker amount must be higher than 0",
/// or "size lower than the minimum". Balance is slow to update; treat as position closed to stop retry spam.
fn is_invalid_amounts_error(msg: Option<&str>) -> bool {
    msg.map_or(false, |m| {
        let lower = m.to_lowercase();
        lower.contains("invalid amounts") || lower.contains("maker and taker amount") || lower.contains("lower than the minimum")
    })
}

/// Clear pending GTC buy state so further fills from the same order are not treated as a new position.
/// Call when position is closed by TP or SL (possibly after a partial fill) so we don't reopen on same order.
fn clear_pending_gtc(state: &mut RunnerState) {
    state.pending_gtc_order_id = None;
    state.pending_gtc_token_id = None;
    state.pending_gtc_side = None;
    state.pending_gtc_price = None;
    state.pending_gtc_requested_size = None;
    state.pending_gtc_timestamp_ms = None;
    state.pending_gtc_last_observed_filled = None;
    state.pending_gtc_fill_deltas.clear();
}

/// Get available balance for a token for TP/SL.
/// Priority: WS user fill state for *balance*, but always cap by REST allowance when WS is used,
/// because the WS has no allowance info — the server enforces min(balance, allowance) on every sell.
/// Falls back to full REST balance-allowance (which returns min(balance,allowance)) when WS has no data.
/// REST result is cached with TTL 3s to avoid calling get_available_balance on every tick.
/// When `sl_loop` is true: if REST returns 0/dust (e.g. balance still "locked" after cancelling TP),
/// trust WS balance so we keep trying to place the SL sell instead of assuming position closed externally.
async fn get_available_for_sell(
    clob: &dyn ClobClient,
    ws_user: Option<&ClobWsUser>,
    token_id: &str,
    cache: &mut Option<(String, Decimal, Instant)>,
    sl_loop: bool,
) -> Option<Decimal> {
    let ws_balance = if let Some(ws) = ws_user {
        ws.get_balance_for_token(token_id).await
    } else {
        None
    };

    const REST_CACHE_TTL: Duration = Duration::from_secs(3);
    let rest_effective = if let Some((cached_token, cached_val, cached_at)) = cache {
        if cached_token == token_id && cached_at.elapsed() < REST_CACHE_TTL {
            Some(*cached_val)
        } else {
            let fresh = clob.get_available_balance(token_id).await.ok().flatten();
            if let Some(v) = fresh {
                *cache = Some((token_id.to_string(), v, Instant::now()));
            }
            fresh
        }
    } else {
        let fresh = clob.get_available_balance(token_id).await.ok().flatten();
        if let Some(v) = fresh {
            *cache = Some((token_id.to_string(), v, Instant::now()));
        }
        fresh
    };

    match (ws_balance, rest_effective) {
        // Both sources: use the minimum — WS has the freshest balance, REST has allowance enforcement.
        // In SL loop: if REST says 0/dust (often after cancel-TP lag), trust WS so we don't assume "closed externally".
        (Some(ws), Some(rest)) => {
            if sl_loop && rest <= DUST_THRESHOLD && ws > DUST_THRESHOLD {
                Some(ws)
            } else {
                Some(ws.min(rest))
            }
        }
        // Only WS: no REST data; trust WS but it may exceed allowance — caller will hit 400 if allowance is low.
        (Some(ws), None) => Some(ws),
        // Only REST: normal path without WS.
        (None, rest) => rest,
    }
}

/// Log balance immediately after a buy (debug level).
/// When `known_fill` is Some, use that size for the bought side instead of querying WS/REST — eliminates post-fill balance query and logs "fill (instant)".
async fn log_balance_after_buy(
    clob: &dyn ClobClient,
    market: &ResolvedMarket,
    ws_user: Option<&ClobWsUser>,
    buy_timestamp_ms: Option<u64>,
    bought_side: Option<EntrySide>,
    known_fill: Option<(EntrySide, Decimal)>,
) {
    let (up, down, source) = if let Some((side, size)) = known_fill {
        // Use the fill size we already have — no query for the bought side.
        let up = if side == EntrySide::Up {
            Some(size)
        } else {
            let from_ws = if let Some(ws) = ws_user {
                ws.get_balance_for_token(&market.token_id_up).await
            } else {
                None
            };
            if from_ws.is_some() {
                from_ws
            } else {
                clob.get_available_balance(&market.token_id_up)
                    .await
                    .ok()
                    .flatten()
            }
        };
        let down = if side == EntrySide::Down {
            Some(size)
        } else {
            let from_ws = if let Some(ws) = ws_user {
                ws.get_balance_for_token(&market.token_id_down).await
            } else {
                None
            };
            if from_ws.is_some() {
                from_ws
            } else {
                clob.get_available_balance(&market.token_id_down)
                    .await
                    .ok()
                    .flatten()
            }
        };
        (up, down, "fill (instant)")
    } else if let Some(ws) = ws_user {
        let up_ws = ws.get_balance_for_token(&market.token_id_up).await;
        let down_ws = ws.get_balance_for_token(&market.token_id_down).await;
        let up = if up_ws.is_some() {
            up_ws
        } else {
            clob.get_available_balance(&market.token_id_up)
                .await
                .ok()
                .flatten()
        };
        let down = if down_ws.is_some() {
            down_ws
        } else {
            clob.get_available_balance(&market.token_id_down)
                .await
                .ok()
                .flatten()
        };
        let source = if up_ws.is_some() && down_ws.is_some() {
            "WS"
        } else {
            "REST"
        };
        (up, down, source)
    } else {
        let up = clob
            .get_available_balance(&market.token_id_up)
            .await
            .ok()
            .flatten();
        let down = clob
            .get_available_balance(&market.token_id_down)
            .await
            .ok()
            .flatten();
        (up, down, "REST")
    };
    let up_str = up
        .as_ref()
        .map(fmt_decimal_2)
        .unwrap_or_else(|| "-".to_string());
    let down_str = down
        .as_ref()
        .map(fmt_decimal_2)
        .unwrap_or_else(|| "-".to_string());
    debug!(
        "[IntervalSniper] Balance after buy ({}):  Up={}  Down={}",
        source, up_str, down_str
    );
    let bought_balance = match bought_side {
        Some(EntrySide::Up) => up,
        Some(EntrySide::Down) => down,
        None => None,
    };
    if let Some(ts) = buy_timestamp_ms {
        if bought_balance.map_or(false, |b| b > dec!(0.2)) {
            let delay_ms = now_ms().saturating_sub(ts);
            debug!(
                "[IntervalSniper] delay desde compra hasta balance visible: {} ms",
                delay_ms
            );
        }
    }
}

/// Fetch CLOB balance for both tokens and log every BALANCE_LOG_INTERVAL_MS when holding a position.
/// "WS balance after fill" / "Balance from fill (instant)" only when the position was bought this interval; otherwise "WS balance" or "CLOB balance (REST)".
async fn log_clob_balance_if_due(
    clob: &dyn ClobClient,
    market: &ResolvedMarket,
    state: &mut RunnerState,
    now_ms_u: u64,
    ws_user: Option<&ClobWsUser>,
) -> Result<()> {
    // Solo imprimir después de una compra en este intervalo; al terminar el intervalo se limpia la posición y dejamos de loguear.
    const MIN_BALANCE_FOR_DELAY_LOG: Decimal = dec!(0.2);
    let has_position = state.pending_auto_sell.is_some() || state.pending_stop_loss.is_some();
    if !has_position {
        return Ok(());
    }
    let due = state
        .last_balance_log_ms
        .map_or(true, |t| now_ms_u.saturating_sub(t) >= BALANCE_LOG_INTERVAL_MS);
    if !due {
        return Ok(());
    }

    // Prefer known fill from last_buy_order for position token when WS has no data yet — avoids REST fallback for logging.
    let position_token_id = state
        .pending_auto_sell
        .as_ref()
        .map(|t| t.token_id.as_str())
        .or_else(|| state.pending_stop_loss.as_ref().map(|s| s.token_id.as_str()));
    let known_fill_for_position = state
        .last_buy_order
        .as_ref()
        .filter(|b| position_token_id == Some(b.token_id.as_str()))
        .map(|b| b.size.clone());

    let (up, down, balance_from_ws, balance_from_fill_instant) = if let Some(ws) = ws_user {
        let up_ws = ws.get_balance_for_token(&market.token_id_up).await;
        let down_ws = ws.get_balance_for_token(&market.token_id_down).await;
        let up = if up_ws.is_some() {
            up_ws
        } else if known_fill_for_position.as_ref().zip(position_token_id).map_or(false, |(_, tid)| tid == market.token_id_up) {
            known_fill_for_position.clone()
        } else {
            clob.get_available_balance(&market.token_id_up)
                .await
                .ok()
                .flatten()
        };
        let down = if down_ws.is_some() {
            down_ws
        } else if known_fill_for_position.as_ref().zip(position_token_id).map_or(false, |(_, tid)| tid == market.token_id_down) {
            known_fill_for_position.clone()
        } else {
            clob.get_available_balance(&market.token_id_down)
                .await
                .ok()
                .flatten()
        };
        let from_fill_instant = (up_ws.is_none() && position_token_id == Some(market.token_id_up.as_str()) && known_fill_for_position.is_some())
            || (down_ws.is_none() && position_token_id == Some(market.token_id_down.as_str()) && known_fill_for_position.is_some());
        (up, down, up_ws.is_some() || down_ws.is_some(), from_fill_instant)
    } else {
        let up = if known_fill_for_position.as_ref().zip(position_token_id).map_or(false, |(_, tid)| tid == market.token_id_up) {
            known_fill_for_position.clone()
        } else {
            clob.get_available_balance(&market.token_id_up)
                .await
                .ok()
                .flatten()
        };
        let down = if known_fill_for_position.as_ref().zip(position_token_id).map_or(false, |(_, tid)| tid == market.token_id_down) {
            known_fill_for_position.clone()
        } else {
            clob.get_available_balance(&market.token_id_down)
                .await
                .ok()
                .flatten()
        };
        let from_fill_instant = known_fill_for_position.is_some();
        (up, down, false, from_fill_instant)
    };

    // Cap position-token balance by inferred remaining when in SL: WS can aggregate stale orders
    // or miss the SELL fill, showing e.g. 72.13 instead of ~0 after SL fill. Use min(ws, inferred).
    let (mut up, mut down) = if let (Some(tid), Some(ref _sl), Some(ref buy)) = (
        position_token_id,
        state.pending_stop_loss.as_ref(),
        state.last_buy_order.as_ref(),
    ) {
        if buy.token_id == *tid {
            let inferred = buy.size.clone() - state.sl_cumulative_filled.clone();
            let up_capped = if *tid == market.token_id_up {
                up.map(|u| u.min(inferred.clone()))
            } else {
                up
            };
            let down_capped = if *tid == market.token_id_down {
                down.map(|d| d.min(inferred))
            } else {
                down
            };
            (up_capped, down_capped)
        } else {
            (up, down)
        }
    } else {
        (up, down)
    };

    // If we have open position and haven't recorded "balance reflected" yet, check now.
    if state.balance_reflected_at_ms.is_none()
        && state.last_buy_order.is_some()
        && (state.pending_auto_sell.is_some() || state.pending_stop_loss.is_some())
    {
        let token_id = state
            .pending_auto_sell
            .as_ref()
            .map(|t| t.token_id.as_str())
            .or_else(|| state.pending_stop_loss.as_ref().map(|s| s.token_id.as_str()));
        let expected = state.last_buy_order.as_ref().map(|b| b.size.clone());
        if let (Some(tid), Some(ref exp)) = (token_id, expected) {
            let available = if tid == market.token_id_up { up } else { down };
            let threshold = (exp.clone() * dec!(0.99)).max(exp.clone() - dec!(0.01));
            if balance_from_ws || balance_from_fill_instant {
                // Balance from WS or from known fill: we knew it at fill time → delay 0.
                state.balance_reflected_at_ms = state
                    .balance_reflected_at_ms
                    .or_else(|| state.last_buy_order.as_ref().map(|b| b.timestamp_ms));
            } else if available.map_or(false, |a| a >= threshold && a > MIN_BALANCE_FOR_DELAY_LOG) {
                state.balance_reflected_at_ms = Some(now_ms_u);
            }
        }
    }
    // Only say "after fill" when the position came from a buy in *this* interval. WS can still
    // return balance for the other token from previous intervals, so avoid misleading "after fill".
    let fill_in_this_interval = state
        .last_buy_order
        .as_ref()
        .map_or(false, |b| (b.timestamp_ms / 1000) >= market.interval_start_unix);

    // When WS returns 0 for the position token right after a buy (e.g. re-entry: WS still has
    // previous SL sell, new BUY not yet reflected), show at least known_fill so we don't display misleading 0.
    if fill_in_this_interval {
        if let (Some(ref kf), Some(tid)) = (known_fill_for_position.as_ref(), position_token_id) {
            let known = (*kf).clone();
            if *tid == market.token_id_up && up.as_ref().map_or(true, |u| *u < known) {
                up = Some(known.clone());
            } else if *tid == market.token_id_down && down.as_ref().map_or(true, |d| *d < known) {
                down = Some(known);
            }
        }
    }

    let up_str = up
        .as_ref()
        .map(fmt_decimal_2)
        .unwrap_or_else(|| "-".to_string());
    let down_str = down
        .as_ref()
        .map(fmt_decimal_2)
        .unwrap_or_else(|| "-".to_string());
    let balance_changed = up != state.last_logged_balance_up || down != state.last_logged_balance_down;
    if balance_changed {
        if balance_from_ws && fill_in_this_interval {
            info!(
                "[IntervalSniper] WS balance after fill:  Up={}  Down={}  (0ms)",
                up_str, down_str
            );
            state.balance_delay_clob_logged = true;
        } else if balance_from_fill_instant && fill_in_this_interval {
            info!(
                "[IntervalSniper] Balance from fill (instant)  Up={}  Down={}  (0ms)",
                up_str, down_str
            );
            state.balance_delay_clob_logged = true;
        } else if balance_from_ws || balance_from_fill_instant {
            info!(
                "[IntervalSniper] WS balance:  Up={}  Down={}",
                up_str, down_str
            );
        } else {
            info!(
                "[IntervalSniper] CLOB balance (REST)  Up={}  Down={}",
                up_str, down_str
            );
        }
        state.last_logged_balance_up = up;
        state.last_logged_balance_down = down;
    }
    if !state.balance_delay_clob_logged {
        if let (Some(reflected_ms), Some(ref buy)) = (state.balance_reflected_at_ms, &state.last_buy_order) {
            let token_id = state
                .pending_auto_sell
                .as_ref()
                .map(|t| t.token_id.as_str())
                .or_else(|| state.pending_stop_loss.as_ref().map(|s| s.token_id.as_str()));
            let available = token_id.and_then(|tid| {
                if tid == market.token_id_up { up } else { down }
            });
            if available.map_or(false, |a| a > MIN_BALANCE_FOR_DELAY_LOG) {
                let delay_ms = reflected_ms.saturating_sub(buy.timestamp_ms);
                if !balance_from_ws && !balance_from_fill_instant {
                    info!(
                        "[IntervalSniper] delay desde compra hasta que se reflejó en CLOB: {} ms",
                        delay_ms
                    );
                }
                state.balance_delay_clob_logged = true;
            }
        }
    }
    state.last_balance_log_ms = Some(now_ms_u);
    Ok(())
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

/// Choose entry side when triggering on best bid: side with best_bid in [min_buy_price, max_buy_price] and enough ask liquidity.
/// Used for GTC limit entry: when best bid touches range, place limit at max_buy_price + 1 tick.
fn choose_side_by_bid(
    config: &Config,
    book: &TopOfBook,
    min_order_size: Decimal,
) -> Option<(EntrySide, Decimal, Decimal)> {
    let up = book.token_id_up.as_ref()?;
    let down = book.token_id_down.as_ref()?;
    let up_bid = config.allow_buy_up.then(|| up.best_bid).flatten()?;
    let down_bid = config.allow_buy_down.then(|| down.best_bid).flatten()?;
    let up_size = up.best_ask_size.unwrap_or(Decimal::ZERO);
    let down_size = down.best_ask_size.unwrap_or(Decimal::ZERO);

    let in_range = |p: Decimal| p >= config.min_buy_price && p <= config.max_buy_price;

    let mut candidates: Vec<(EntrySide, Decimal, Decimal)> = Vec::new();
    if in_range(up_bid) && up_size >= min_order_size {
        candidates.push((EntrySide::Up, up_bid, up_size));
    }
    if in_range(down_bid) && down_size >= min_order_size {
        candidates.push((EntrySide::Down, down_bid, down_size));
    }
    candidates.sort_by(|a, b| b.1.cmp(&a.1)); // higher best_bid first
    candidates.into_iter().next()
}

/// Delay between redeem tx submissions to avoid RPC rate limit.
const REDEEM_DELAY_BETWEEN_TXS_MS: u64 = 500;

async fn redeem_run_once(
    http: &Client,
    clob_host: &str,
    rpc_url: &str,
    wallet: &ethers::signers::LocalWallet,
) {
    // Use FUNDER_ADDRESS (proxy wallet) for positions API if set; otherwise EOA. Positions are under proxy on Polymarket.
    let (user_addr, funder_address) = match std::env::var("FUNDER_ADDRESS")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .map(|s| s.trim().trim_start_matches("0x").to_string())
    {
        Some(ref s) => {
            let addr = format!("0x{}", s);
            let funder = addr.parse::<ethers::types::Address>().ok();
            (addr, funder)
        }
        None => (format!("{:#x}", wallet.address()), None),
    };
    match redeem::fetch_resolved_condition_ids_from_positions(http, clob_host, &user_addr).await {
        Ok(condition_ids) => {
            if condition_ids.is_empty() {
                info!("[Redeem] no redeemable positions this run (Data API checked)");
                return;
            }
            let use_safe = funder_address.map(|f| f != wallet.address()).unwrap_or(false);
            info!(
                "[Redeem] run: {} resolved market(s) to try (signer from PRIVATE_KEY={:#x}, {} path)",
                condition_ids.len(),
                wallet.address(),
                if use_safe { "Safe" } else { "EOA" }
            );
            for cid in &condition_ids {
                match redeem::redeem_positions(wallet, rpc_url, cid, funder_address).await {
                    Ok(success) => {
                        if success {
                            info!(
                                "[Redeem] redeemed condition_id={}..",
                                &cid[..cid.len().min(18)]
                            );
                        }
                    }
                    Err(e) => {
                        warn!(
                            "[Redeem] failed condition_id={}..: {:?}",
                            &cid[..cid.len().min(18)],
                            e
                        );
                    }
                }
                tokio::time::sleep(Duration::from_millis(REDEEM_DELAY_BETWEEN_TXS_MS)).await;
            }
        }
        Err(e) => {
            warn!("[Redeem] fetch positions failed: {}", e);
        }
    }
}

async fn redeem_loop(
    http: Client,
    clob_host: String,
    rpc_url: String,
    wallet: ethers::signers::LocalWallet,
    redeem_interval_sec: u64,
) {
    // Run once immediately (like the JS: redeemAllWinnings(); setInterval(...)).
    redeem_run_once(&http, &clob_host, &rpc_url, &wallet).await;

    let mut interval = tokio::time::interval(Duration::from_secs(redeem_interval_sec));
    interval.tick().await; // consume the immediate first tick so we don't run twice at startup
    loop {
        interval.tick().await;
        redeem_run_once(&http, &clob_host, &rpc_url, &wallet).await;
    }
}

pub async fn run() -> Result<()> {
    let config = load_config()?;
    let clob_host = std::env::var("POLYMARKET_CLOB_HOST")
        .unwrap_or_else(|_| "https://clob.polymarket.com".to_string());
    // HFT: short timeout so the main loop is not blocked by slow REST (fail fast, retry next tick)
    let http = Client::builder().timeout(Duration::from_secs(2)).build()?;
    let clob = Arc::new(crate::clob::create_clob_client(config.dry_run)?);

    if config.redeem_enabled && !config.dry_run {
        let rpc_url = std::env::var("POLYGON_RPC_URL")
            .unwrap_or_else(|_| "https://polygon-rpc.com".to_string());
        let pk = std::env::var("PRIVATE_KEY")
            .or_else(|_| std::env::var("POLYMARKET_PRIVATE_KEY"));
        if let Ok(pk) = pk {
            let wallet = pk
                .trim()
                .strip_prefix("0x")
                .unwrap_or(pk.trim())
                .parse::<ethers::signers::LocalWallet>()
                .ok();
            if let Some(wallet) = wallet {
                let http_redeem = http.clone();
                let clob_host = clob_host.clone();
                let redeem_interval_sec = config.redeem_interval_sec;
                tokio::spawn(async move {
                    redeem_loop(http_redeem, clob_host, rpc_url, wallet, redeem_interval_sec)
                        .await
                });
                info!(
                    "[IntervalSniper] redeem task started: every {}s for all resolved positions (CLOB + CTF)",
                    redeem_interval_sec
                );
            } else {
                warn!("[IntervalSniper] redeem enabled but PRIVATE_KEY invalid, skip redeem task");
            }
        } else {
            warn!("[IntervalSniper] redeem enabled but PRIVATE_KEY not set, skip redeem task");
        }
    }

    let mut state = RunnerState {
        market: None,
        ws_book: None,
        ws_user: None,
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
        tp_limit_order_id: None,
        tp_placed_size: None,
        tp_cumulative_filled: Decimal::ZERO,
        tp_last_order_filled: Decimal::ZERO,
        tp_limit_balance_retries: 0,
        tp_limit_last_rest_check_ms: None,
        sl_limit_order_id: None,
        sl_limit_order_price: None,
        sl_cumulative_filled: Decimal::ZERO,
        sl_last_order_filled: Decimal::ZERO,
        sl_limit_last_rest_check_ms: None,
        interval_switch_wall_time_ms: None,
        session_log: None,
        interval_min_bid_up: None,
        interval_max_bid_up: None,
        interval_min_bid_down: None,
        interval_max_bid_down: None,
        last_best_bid_for_position: None,
        last_balance_log_ms: None,
        balance_reflected_at_ms: None,
        balance_delay_clob_logged: false,
        last_logged_balance_up: None,
        last_logged_balance_down: None,
        pending_gtc_order_id: None,
        pending_gtc_token_id: None,
        pending_gtc_side: None,
        pending_gtc_price: None,
        pending_gtc_requested_size: None,
        pending_gtc_timestamp_ms: None,
        pending_gtc_last_observed_filled: None,
        pending_gtc_fill_deltas: vec![],
        allowance_cache: None,
        telegram: None,
        telegram_handle: None,
    };

    // Telegram: background task sends logs; main loop only enqueues (try_send), so no delay.
    if config.telegram_bot_token.is_some() && config.telegram_chat_id.is_some() {
        let (telegram, handle) = TelegramLog::new(
            config.telegram_bot_token.clone(),
            config.telegram_chat_id.clone(),
        );
        state.telegram = Some(telegram.clone());
        state.telegram_handle = Some(handle.unwrap());
    }

    if config.session_log_enabled {
        let session_start_ms = now_ms();
        state.session_log = SessionLog::new(
            session_start_ms,
            &config.session_log_dir,
            state.telegram.clone(),
            config.telegram_msg_format,
        )?;
    }

    info!(
        "[IntervalSniper] started dry_run={} slug={}",
        config.dry_run, config.market_slug
    );
    info!(
        "[Config] strategy={:?} tp={} sl={} size={} min_buy={} max_buy={} loop_ms={} session_log={}",
        config.order_strategy,
        fmt_decimal_2(&config.take_profit_price),
        fmt_decimal_2(&config.stop_loss_price),
        fmt_decimal_2(&config.size_shares),
        fmt_decimal_2(&config.min_buy_price),
        fmt_decimal_2(&config.max_buy_price),
        config.loop_ms,
        config.session_log_enabled,
    );
    info!(
        "[Config] allow_up={} allow_down={} auto_sell={} stop_loss={} dry_run={} redeem={}",
        config.allow_buy_up,
        config.allow_buy_down,
        config.enable_auto_sell,
        config.enable_stop_loss,
        config.dry_run,
        config.redeem_enabled,
    );

    let loop_ms = config.loop_ms;
    let mut tick_count: u64 = 0;

    loop {
        tick_count += 1;
        let now_u = now_unix();
        let now_ms_u = now_ms();

        // Poll GTC fill first (before interval switch) so we never drop a fill when the interval boundary crosses this tick.
        // Fill state comes from WS user channel (TRADE + order UPDATE events). If "GTC partial fill" never appears
        // but the order filled on Polymarket, the WS may have missed the event (reconnect, event_type casing, or no TRADE/UPDATE received).
        if let Some(market) = state.market.as_ref().cloned() {
            if let (Some(ref order_id), Some(ws_user)) = (
                state.pending_gtc_order_id.as_ref(),
                state.ws_user.as_ref().map(|a| a.as_ref()),
            ) {
                if let Some((filled_size, ws_event_type)) = ws_user.get_order_filled_size_with_type(order_id).await {
                    let requested = state
                        .pending_gtc_requested_size
                        .as_ref()
                        .cloned()
                        .unwrap_or(Decimal::ZERO);
                    // Cap by REST size_matched when available so we never assume more filled than exchange (avoids partial→TP for full size→balance error→cancel).
                    let filled = {
                        let from_ws = filled_size.min(requested.clone());
                        match clob.get_order(order_id).await {
                            Ok(info) => info
                                .get("size_matched")
                                .and_then(|v| v.as_str())
                                .and_then(|s| Decimal::from_str(s).ok())
                                .map(|rest_matched| from_ws.min(rest_matched))
                                .unwrap_or(from_ws),
                            _ => from_ws,
                        }
                    };
                    // Log each partial fill (for "N partials: a, b, c" in final BUY line); no logic change.
                    let last = state.pending_gtc_last_observed_filled.unwrap_or(Decimal::ZERO);
                    if filled > last {
                        let delta = filled.clone() - last;
                        state.pending_gtc_fill_deltas.push(delta);
                        state.pending_gtc_last_observed_filled = Some(filled.clone());
                        info!(
                            "[IntervalSniper] GTC partial fill: +{} (total {}/{})",
                            fmt_decimal_2(&delta),
                            fmt_decimal_2(&filled),
                            fmt_decimal_2(&requested)
                        );
                    }
                    // Place TP/SL on any fill >= MIN; if fill grows (partials), update position and replace TP/SL with new total size.
                    let prev_filled = state
                        .last_buy_order
                        .as_ref()
                        .and_then(|b| {
                            if b.order_id.as_ref() == state.pending_gtc_order_id.as_ref() {
                                Some(b.size.clone())
                            } else {
                                None
                            }
                        })
                        .unwrap_or(Decimal::ZERO);
                    if filled >= MIN_SELL_SIZE
                        && filled > prev_filled
                        && state.pending_gtc_token_id.is_some()
                        && state.pending_gtc_side.is_some()
                        && state.pending_gtc_price.is_some()
                    {
                        let order_id_full = order_id.to_string();
                        let token_id = state.pending_gtc_token_id.as_ref().unwrap().clone();
                        let entry_side = state.pending_gtc_side.unwrap();
                        let entry_price = state.pending_gtc_price.as_ref().unwrap().clone();
                        let is_additional_fill = prev_filled >= MIN_SELL_SIZE;
                        // BUG FIX: when is_additional_fill fires (entry fill grew e.g. 13.99→14.00 due to
                        // REST cap releasing), the resting TP order may have already filled on the exchange.
                        // If WS confirms the TP is filled, skip cancel-and-recreate: the TP detection block
                        // later this iteration will detect the fill and close the position cleanly.
                        let tp_already_filled_ws = if is_additional_fill {
                            if let Some(ref oid) = state.tp_limit_order_id {
                                let tp_size_check =
                                    state.tp_placed_size.unwrap_or(state.config.size_shares);
                                ws_user
                                    .get_order_filled_size_sell_with_type(oid)
                                    .await
                                    .map_or(false, |(ws_filled, _)| {
                                        ws_filled >= tp_size_check * dec!(0.99)
                                    })
                            } else {
                                false
                            }
                        } else {
                            false
                        };
                        if is_additional_fill {
                            if !tp_already_filled_ws {
                                // Fill grew (e.g. 11 -> 12): cancel only TP/SL orders so we replace with total size (do not cancel resting GTC buy).
                                if let Some(ref oid) = state.tp_limit_order_id {
                                    let _ = clob.cancel_order(oid).await;
                                }
                                if let Some(ref oid) = state.sl_limit_order_id {
                                    let _ = clob.cancel_order(oid).await;
                                }
                            } else {
                                info!(
                                    "[IntervalSniper] additional fill ({} -> {}): TP order already filled via WS — skipping cancel/recreate, TP detection will close position this iteration",
                                    fmt_decimal_2(&prev_filled), fmt_decimal_2(&filled)
                                );
                            }
                            state.total_shares_this_interval += filled.clone() - prev_filled.clone();
                        } else {
                            state.trades_this_interval += 1;
                            state.total_shares_this_interval += filled.clone();
                        }
                        state.last_buy_order = Some(LastBuyOrder {
                            order_id: state.pending_gtc_order_id.clone(),
                            token_id: token_id.clone(),
                            side: entry_side,
                            size: filled.clone(),
                            price: entry_price.clone(),
                            timestamp_ms: state.pending_gtc_timestamp_ms.unwrap_or(now_ms_u),
                        });
                        if !is_additional_fill {
                            if let Some(ref mut log) = state.session_log {
                                let _ = log.log_order_filled(
                                    &market.slug,
                                    market.interval_start_unix,
                                    market.close_time_unix,
                                    now_ms_u,
                                    &order_id_full,
                                    filled.clone(),
                                    "ws_user",
                                );
                            }
                        }
                        let fill_lag_ms = now_ms_u.saturating_sub(state.pending_gtc_timestamp_ms.unwrap_or(now_ms_u));
                        let target_price = if state.config.auto_sell_at_max_price {
                            dec!(0.99)
                        } else {
                            round_to_tick(state.config.take_profit_price)
                        };
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
                        if !tp_already_filled_ws {
                        state.pending_auto_sell = Some(PendingAutoSell {
                            token_id: token_id.clone(),
                            target_price,
                            size: tp_size,
                            placed_at_ms: now_ms_u,
                        });
                        state.pending_stop_loss = Some(PendingStopLoss {
                            token_id,
                            entry_price: entry_price.clone(),
                            size: sl_size,
                            trigger_price: round_to_tick(state.config.stop_loss_price),
                            placed_at_ms: now_ms_u,
                        });
                        state.allowance_cache = None;
                        state.auto_sell_placed = false;
                        state.stop_loss_placed = false;
                        // Reset SL/TP cumulative state for new position (re-entry); otherwise log_clob_balance caps by previous position's sl_cumulative_filled.
                        state.sl_cumulative_filled = Decimal::ZERO;
                        state.sl_last_order_filled = Decimal::ZERO;
                        state.tp_cumulative_filled = Decimal::ZERO;
                        state.tp_last_order_filled = Decimal::ZERO;
                        state.tp_limit_order_id = None;
                        state.tp_placed_size = None;
                        state.sl_limit_order_id = None;
                        state.sl_limit_order_price = None;
                        }
                        let partials_str = if state.pending_gtc_fill_deltas.len() >= 2 {
                            let parts: Vec<String> = state.pending_gtc_fill_deltas.iter().map(fmt_decimal_2).collect();
                            format!(" ({} partials: {})", state.pending_gtc_fill_deltas.len(), parts.join(", "))
                        } else {
                            String::new()
                        };
                        // Clear GTC state only when order is fully filled, so we keep polling for more partials.
                        if filled >= requested {
                            state.pending_gtc_order_id = None;
                            state.pending_gtc_token_id = None;
                            state.pending_gtc_side = None;
                            state.pending_gtc_price = None;
                            state.pending_gtc_requested_size = None;
                            state.pending_gtc_timestamp_ms = None;
                            state.pending_gtc_last_observed_filled = None;
                            state.pending_gtc_fill_deltas.clear();
                        }
                        let side_str = match entry_side {
                            EntrySide::Up => "Up  ",
                            EntrySide::Down => "Down",
                        };
                        info!(
                            "[IntervalSniper]  BUY   {}  @ {}   size={}{} (fill first: WS {})   fill_lag={}ms   TP size={} ({}%)   SL size={} ({}%)",
                            side_str,
                            fmt_decimal_2(&entry_price),
                            fmt_decimal_2(&filled),
                            partials_str,
                            ws_event_type,
                            fill_lag_ms,
                            fmt_decimal_2(&tp_size),
                            state.config.auto_sell_quantity_percent,
                            fmt_decimal_2(&sl_size),
                            state.config.stop_loss_quantity_percent
                        );
                        log_balance_after_buy(
                            clob.as_ref().as_ref(),
                            &market,
                            Some(ws_user),
                            state.last_buy_order.as_ref().map(|b| b.timestamp_ms),
                            state.last_buy_order.as_ref().map(|b| b.side),
                            Some((entry_side, filled.clone())),
                        )
                        .await;
                    }
                } else {
                    let waited_ms = now_ms_u.saturating_sub(state.pending_gtc_timestamp_ms.unwrap_or(0));
                    if waited_ms >= PENDING_GTC_REST_CHECK_MS
                        && state.pending_gtc_token_id.is_some()
                        && state.pending_gtc_requested_size.is_some()
                        && state.pending_gtc_side.is_some()
                        && state.pending_gtc_price.is_some()
                    {
                        let token_id = state.pending_gtc_token_id.as_ref().unwrap().clone();
                        let requested = state.pending_gtc_requested_size.as_ref().unwrap().clone();
                        // REST get_order: detect fill when WS missed the event (more reliable than balance which can lag).
                        let rest_order_filled = match clob.get_order(order_id).await {
                            Ok(info) => {
                                let status = info.get("status").and_then(|v| v.as_str()).unwrap_or("");
                                let size_matched = info
                                    .get("size_matched")
                                    .and_then(|v| v.as_str())
                                    .and_then(|s| Decimal::from_str(s).ok())
                                    .unwrap_or(Decimal::ZERO);
                                if (status.contains("MATCHED") || status.eq_ignore_ascii_case("FILLED"))
                                    && size_matched >= requested.clone() * dec!(0.99)
                                {
                                    Some(size_matched.min(requested.clone()))
                                } else {
                                    None
                                }
                            }
                            _ => None,
                        };
                        if let Some(filled) = rest_order_filled {
                            let last = state.pending_gtc_last_observed_filled.unwrap_or(Decimal::ZERO);
                            if filled > last {
                                let delta = filled.clone() - last;
                                state.pending_gtc_fill_deltas.push(delta);
                                state.pending_gtc_last_observed_filled = Some(filled.clone());
                            }
                            let entry_side = state.pending_gtc_side.unwrap();
                            let entry_price = state.pending_gtc_price.as_ref().unwrap().clone();
                            state.trades_this_interval += 1;
                            state.total_shares_this_interval += filled.clone();
                            state.last_buy_order = Some(LastBuyOrder {
                                order_id: state.pending_gtc_order_id.clone(),
                                token_id: token_id.clone(),
                                side: entry_side,
                                size: filled.clone(),
                                price: entry_price.clone(),
                                timestamp_ms: state.pending_gtc_timestamp_ms.unwrap_or(now_ms_u),
                            });
                            if let Some(ref mut log) = state.session_log {
                                let _ = log.log_order_filled(
                                    &market.slug,
                                    market.interval_start_unix,
                                    market.close_time_unix,
                                    now_ms_u,
                                    order_id,
                                    filled.clone(),
                                    "REST get_order",
                                );
                            }
                            let target_price = if state.config.auto_sell_at_max_price {
                                dec!(0.99)
                            } else {
                                round_to_tick(state.config.take_profit_price)
                            };
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
                                token_id: token_id.clone(),
                                target_price,
                                size: tp_size,
                                placed_at_ms: now_ms_u,
                            });
                            state.pending_stop_loss = Some(PendingStopLoss {
                                token_id,
                                entry_price: entry_price.clone(),
                                size: sl_size,
                                trigger_price: round_to_tick(state.config.stop_loss_price),
                                placed_at_ms: now_ms_u,
                            });
                            state.allowance_cache = None;
                            state.auto_sell_placed = false;
                            state.stop_loss_placed = false;
                            state.sl_cumulative_filled = Decimal::ZERO;
                            state.sl_last_order_filled = Decimal::ZERO;
                            state.tp_cumulative_filled = Decimal::ZERO;
                            state.tp_last_order_filled = Decimal::ZERO;
                            state.tp_limit_order_id = None;
                            state.tp_placed_size = None;
                            state.sl_limit_order_id = None;
                            state.sl_limit_order_price = None;
                            let partials_str = if state.pending_gtc_fill_deltas.len() >= 2 {
                                let parts: Vec<String> = state.pending_gtc_fill_deltas.iter().map(fmt_decimal_2).collect();
                                format!(" ({} partials: {})", state.pending_gtc_fill_deltas.len(), parts.join(", "))
                            } else {
                                String::new()
                            };
                            state.pending_gtc_order_id = None;
                            state.pending_gtc_token_id = None;
                            state.pending_gtc_side = None;
                            state.pending_gtc_price = None;
                            state.pending_gtc_requested_size = None;
                            state.pending_gtc_timestamp_ms = None;
                            state.pending_gtc_last_observed_filled = None;
                            state.pending_gtc_fill_deltas.clear();
                            let side_str = match entry_side {
                                EntrySide::Up => "Up  ",
                                EntrySide::Down => "Down",
                            };
                            info!(
                                "[IntervalSniper]  BUY   {}  @ {}   size={}{} (fill: REST get_order)   TP size={} ({}%)   SL size={} ({}%)",
                                side_str,
                                fmt_decimal_2(&entry_price),
                                fmt_decimal_2(&filled),
                                partials_str,
                                fmt_decimal_2(&tp_size),
                                state.config.auto_sell_quantity_percent,
                                fmt_decimal_2(&sl_size),
                                state.config.stop_loss_quantity_percent
                            );
                            log_balance_after_buy(
                                clob.as_ref().as_ref(),
                                &market,
                                Some(ws_user),
                                state.last_buy_order.as_ref().map(|b| b.timestamp_ms),
                                state.last_buy_order.as_ref().map(|b| b.side),
                                Some((entry_side, filled.clone())),
                            )
                            .await;
                        } else {
                            // Prefer WS balance (instant after fill event) before falling back to REST balance.
                            let ws_bal = ws_user.get_balance_for_token(&token_id).await;
                            let (av_opt, bal_source) = if let Some(b) = ws_bal {
                                (Some(b), "REST, balance from WS")
                            } else {
                                let rest = clob.as_ref().get_available_balance(&token_id).await.ok().flatten();
                                (rest, "REST, balance from REST")
                            };
                            if let Some(av) = av_opt {
                            let threshold = (requested.clone() * dec!(0.99)).max(requested.clone() - dec!(0.01));
                            if av >= threshold && av >= MIN_SELL_SIZE {
                                let filled = av.min(requested);
                                let last = state.pending_gtc_last_observed_filled.unwrap_or(Decimal::ZERO);
                                if filled > last {
                                    let delta = filled.clone() - last;
                                    state.pending_gtc_fill_deltas.push(delta);
                                    state.pending_gtc_last_observed_filled = Some(filled.clone());
                                }
                                let entry_side = state.pending_gtc_side.unwrap();
                                let entry_price = state.pending_gtc_price.as_ref().unwrap().clone();
                                state.trades_this_interval += 1;
                                state.total_shares_this_interval += filled.clone();
                                state.last_buy_order = Some(LastBuyOrder {
                                    order_id: state.pending_gtc_order_id.clone(),
                                    token_id: token_id.clone(),
                                    side: entry_side,
                                    size: filled.clone(),
                                    price: entry_price.clone(),
                                    timestamp_ms: state.pending_gtc_timestamp_ms.unwrap_or(now_ms_u),
                                });
                                if let Some(ref mut log) = state.session_log {
                                    let _ = log.log_order_filled(
                                        &market.slug,
                                        market.interval_start_unix,
                                        market.close_time_unix,
                                        now_ms_u,
                                        order_id,
                                        filled.clone(),
                                        bal_source,
                                    );
                                }
                                let target_price = if state.config.auto_sell_at_max_price {
                                    dec!(0.99)
                                } else {
                                    round_to_tick(state.config.take_profit_price)
                                };
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
                                    token_id: token_id.clone(),
                                    target_price,
                                    size: tp_size,
                                    placed_at_ms: now_ms_u,
                                });
                                state.pending_stop_loss = Some(PendingStopLoss {
                                    token_id,
                                    entry_price: entry_price.clone(),
                                    size: sl_size,
                                    trigger_price: round_to_tick(state.config.stop_loss_price),
                                    placed_at_ms: now_ms_u,
                                });
                                state.allowance_cache = None;
                                state.auto_sell_placed = false;
                                state.stop_loss_placed = false;
                                // Reset SL/TP cumulative state for new position (re-entry).
                                state.sl_cumulative_filled = Decimal::ZERO;
                                state.sl_last_order_filled = Decimal::ZERO;
                                state.tp_cumulative_filled = Decimal::ZERO;
                                state.tp_last_order_filled = Decimal::ZERO;
                                state.tp_limit_order_id = None;
                                state.tp_placed_size = None;
                                state.sl_limit_order_id = None;
                                state.sl_limit_order_price = None;
                                let partials_str = if state.pending_gtc_fill_deltas.len() >= 2 {
                                    let parts: Vec<String> = state.pending_gtc_fill_deltas.iter().map(fmt_decimal_2).collect();
                                    format!(" ({} partials: {})", state.pending_gtc_fill_deltas.len(), parts.join(", "))
                                } else {
                                    String::new()
                                };
                                state.pending_gtc_order_id = None;
                                state.pending_gtc_token_id = None;
                                state.pending_gtc_side = None;
                                state.pending_gtc_price = None;
                                state.pending_gtc_requested_size = None;
                                state.pending_gtc_timestamp_ms = None;
                                state.pending_gtc_last_observed_filled = None;
                                state.pending_gtc_fill_deltas.clear();
                                let side_str = match entry_side {
                                    EntrySide::Up => "Up  ",
                                    EntrySide::Down => "Down",
                                };
                                info!(
                                    "[IntervalSniper]  BUY   {}  @ {}   size={}{} (fill first: {})   TP size={} ({}%)   SL size={} ({}%)",
                                    side_str,
                                    fmt_decimal_2(&entry_price),
                                    fmt_decimal_2(&filled),
                                    partials_str,
                                    bal_source,
                                    fmt_decimal_2(&tp_size),
                                    state.config.auto_sell_quantity_percent,
                                    fmt_decimal_2(&sl_size),
                                    state.config.stop_loss_quantity_percent
                                );
                                log_balance_after_buy(
                                    clob.as_ref().as_ref(),
                                    &market,
                                    Some(ws_user),
                                    state.last_buy_order.as_ref().map(|b| b.timestamp_ms),
                                    state.last_buy_order.as_ref().map(|b| b.side),
                                    Some((entry_side, filled.clone())),
                                )
                                .await;
                            }
                            }
                        }
                    }
                }
            }
            if state.pending_gtc_order_id.is_some()
                && state.ws_user.is_none()
                && state.pending_gtc_token_id.is_some()
                && state.pending_gtc_requested_size.is_some()
                && state.pending_gtc_side.is_some()
                && state.pending_gtc_price.is_some()
            {
                let waited_ms = now_ms_u.saturating_sub(state.pending_gtc_timestamp_ms.unwrap_or(0));
                if waited_ms >= PENDING_GTC_NO_WS_FALLBACK_MS {
                    let order_id = state.pending_gtc_order_id.as_ref().unwrap().clone();
                    let token_id = state.pending_gtc_token_id.as_ref().unwrap().clone();
                    let requested = state.pending_gtc_requested_size.as_ref().unwrap().clone();
                    let rest_order_filled = match clob.get_order(&order_id).await {
                        Ok(info) => {
                            let status = info.get("status").and_then(|v| v.as_str()).unwrap_or("");
                            let size_matched = info
                                .get("size_matched")
                                .and_then(|v| v.as_str())
                                .and_then(|s| Decimal::from_str(s).ok())
                                .unwrap_or(Decimal::ZERO);
                            if (status.contains("MATCHED") || status.eq_ignore_ascii_case("FILLED"))
                                && size_matched >= requested.clone() * dec!(0.99)
                            {
                                Some(size_matched.min(requested.clone()))
                            } else {
                                None
                            }
                        }
                        _ => None,
                    };
                    if let Some(filled) = rest_order_filled {
                        let last = state.pending_gtc_last_observed_filled.unwrap_or(Decimal::ZERO);
                        if filled > last {
                            let delta = filled.clone() - last;
                            state.pending_gtc_fill_deltas.push(delta);
                            state.pending_gtc_last_observed_filled = Some(filled.clone());
                        }
                        let entry_side = state.pending_gtc_side.unwrap();
                        let entry_price = state.pending_gtc_price.as_ref().unwrap().clone();
                        state.trades_this_interval += 1;
                        state.total_shares_this_interval += filled.clone();
                        state.last_buy_order = Some(LastBuyOrder {
                            order_id: state.pending_gtc_order_id.clone(),
                            token_id: token_id.clone(),
                            side: entry_side,
                            size: filled.clone(),
                            price: entry_price.clone(),
                            timestamp_ms: state.pending_gtc_timestamp_ms.unwrap_or(now_ms_u),
                        });
                        if let Some(ref mut log) = state.session_log {
                            let _ = log.log_order_filled(
                                &market.slug,
                                market.interval_start_unix,
                                market.close_time_unix,
                                now_ms_u,
                                &order_id,
                                filled.clone(),
                                "REST get_order (no WS)",
                            );
                        }
                        let target_price = if state.config.auto_sell_at_max_price {
                            dec!(0.99)
                        } else {
                            round_to_tick(state.config.take_profit_price)
                        };
                        let base_sell_size = floor_to_decimals(
                            filled.clone().min(state.config.size_shares),
                            SELL_SIZE_DECIMALS,
                        )
                        .max(MIN_SELL_SIZE);
                        let pct_tp = Decimal::from(state.config.auto_sell_quantity_percent) / dec!(100);
                        let pct_sl = Decimal::from(state.config.stop_loss_quantity_percent) / dec!(100);
                        let tp_size = floor_to_decimals(base_sell_size * pct_tp, SELL_SIZE_DECIMALS)
                            .max(MIN_SELL_SIZE)
                            .min(base_sell_size);
                        let sl_size = floor_to_decimals(base_sell_size * pct_sl, SELL_SIZE_DECIMALS)
                            .max(MIN_SELL_SIZE)
                            .min(base_sell_size);
                        state.pending_auto_sell = Some(PendingAutoSell {
                            token_id: token_id.clone(),
                            target_price,
                            size: tp_size,
                            placed_at_ms: now_ms_u,
                        });
                        state.pending_stop_loss = Some(PendingStopLoss {
                            token_id,
                            entry_price: entry_price.clone(),
                            size: sl_size,
                            trigger_price: round_to_tick(state.config.stop_loss_price),
                            placed_at_ms: now_ms_u,
                        });
                        state.allowance_cache = None;
                        state.auto_sell_placed = false;
                        state.stop_loss_placed = false;
                        state.sl_cumulative_filled = Decimal::ZERO;
                        state.sl_last_order_filled = Decimal::ZERO;
                        state.tp_cumulative_filled = Decimal::ZERO;
                        state.tp_last_order_filled = Decimal::ZERO;
                        state.tp_limit_order_id = None;
                        state.tp_placed_size = None;
                        state.sl_limit_order_id = None;
                        state.sl_limit_order_price = None;
                        let partials_str = if state.pending_gtc_fill_deltas.len() >= 2 {
                            let parts: Vec<String> = state.pending_gtc_fill_deltas.iter().map(fmt_decimal_2).collect();
                            format!(" ({} partials: {})", state.pending_gtc_fill_deltas.len(), parts.join(", "))
                        } else {
                            String::new()
                        };
                        state.pending_gtc_order_id = None;
                        state.pending_gtc_token_id = None;
                        state.pending_gtc_side = None;
                        state.pending_gtc_price = None;
                        state.pending_gtc_requested_size = None;
                        state.pending_gtc_timestamp_ms = None;
                        state.pending_gtc_last_observed_filled = None;
                        state.pending_gtc_fill_deltas.clear();
                        let side_str = match entry_side {
                            EntrySide::Up => "Up  ",
                            EntrySide::Down => "Down",
                        };
                        info!(
                            "[IntervalSniper]  BUY   {}  @ {}   size={}{} (fill: REST get_order, no WS)   TP size={} ({}%)   SL size={} ({}%)",
                            side_str,
                            fmt_decimal_2(&entry_price),
                            fmt_decimal_2(&filled),
                            partials_str,
                            fmt_decimal_2(&tp_size),
                            state.config.auto_sell_quantity_percent,
                            fmt_decimal_2(&sl_size),
                            state.config.stop_loss_quantity_percent
                        );
                        log_balance_after_buy(
                            clob.as_ref().as_ref(),
                            &market,
                            None,
                            state.last_buy_order.as_ref().map(|b| b.timestamp_ms),
                            state.last_buy_order.as_ref().map(|b| b.side),
                            Some((entry_side, filled.clone())),
                        )
                        .await;
                    } else if let Ok(Some(av)) = clob.as_ref().get_available_balance(&token_id).await {
                        let threshold = (requested.clone() * dec!(0.99)).max(requested.clone() - dec!(0.01));
                        if av >= threshold && av >= MIN_SELL_SIZE {
                            let filled = av.min(requested);
                            let last = state.pending_gtc_last_observed_filled.unwrap_or(Decimal::ZERO);
                            if filled > last {
                                let delta = filled.clone() - last;
                                state.pending_gtc_fill_deltas.push(delta);
                                state.pending_gtc_last_observed_filled = Some(filled.clone());
                            }
                            let entry_side = state.pending_gtc_side.unwrap();
                            let entry_price = state.pending_gtc_price.as_ref().unwrap().clone();
                            state.trades_this_interval += 1;
                            state.total_shares_this_interval += filled.clone();
                            state.last_buy_order = Some(LastBuyOrder {
                                order_id: state.pending_gtc_order_id.clone(),
                                token_id: token_id.clone(),
                                side: entry_side,
                                size: filled.clone(),
                                price: entry_price.clone(),
                                timestamp_ms: state.pending_gtc_timestamp_ms.unwrap_or(now_ms_u),
                            });
                            if let Some(ref mut log) = state.session_log {
                                let _ = log.log_order_filled(
                                    &market.slug,
                                    market.interval_start_unix,
                                    market.close_time_unix,
                                    now_ms_u,
                                    &order_id,
                                    filled.clone(),
                                    "rest_balance",
                                );
                            }
                            let target_price = if state.config.auto_sell_at_max_price {
                                dec!(0.99)
                            } else {
                                round_to_tick(state.config.take_profit_price)
                            };
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
                                token_id: token_id.clone(),
                                target_price,
                                size: tp_size,
                                placed_at_ms: now_ms_u,
                            });
                            state.pending_stop_loss = Some(PendingStopLoss {
                                token_id,
                                entry_price: entry_price.clone(),
                                size: sl_size,
                                trigger_price: round_to_tick(state.config.stop_loss_price),
                                placed_at_ms: now_ms_u,
                            });
                            state.allowance_cache = None;
                            state.auto_sell_placed = false;
                            state.stop_loss_placed = false;
                            state.sl_cumulative_filled = Decimal::ZERO;
                            state.sl_last_order_filled = Decimal::ZERO;
                            state.tp_cumulative_filled = Decimal::ZERO;
                            state.tp_last_order_filled = Decimal::ZERO;
                            state.tp_limit_order_id = None;
                            state.tp_placed_size = None;
                            state.sl_limit_order_id = None;
                            state.sl_limit_order_price = None;
                            let partials_str = if state.pending_gtc_fill_deltas.len() >= 2 {
                                let parts: Vec<String> = state.pending_gtc_fill_deltas.iter().map(fmt_decimal_2).collect();
                                format!(" ({} partials: {})", state.pending_gtc_fill_deltas.len(), parts.join(", "))
                            } else {
                                String::new()
                            };
                            state.pending_gtc_order_id = None;
                            state.pending_gtc_token_id = None;
                            state.pending_gtc_side = None;
                            state.pending_gtc_price = None;
                            state.pending_gtc_requested_size = None;
                            state.pending_gtc_timestamp_ms = None;
                            state.pending_gtc_last_observed_filled = None;
                            state.pending_gtc_fill_deltas.clear();
                            let side_str = match entry_side {
                                EntrySide::Up => "Up  ",
                                EntrySide::Down => "Down",
                            };
                            info!(
                                "[IntervalSniper]  BUY   {}  @ {}   size={}{} (fill first: REST, no WS)   TP size={} ({}%)   SL size={} ({}%)",
                                side_str,
                                fmt_decimal_2(&entry_price),
                                fmt_decimal_2(&filled),
                                partials_str,
                                fmt_decimal_2(&tp_size),
                                state.config.auto_sell_quantity_percent,
                                fmt_decimal_2(&sl_size),
                                state.config.stop_loss_quantity_percent
                            );
                            log_balance_after_buy(
                                clob.as_ref().as_ref(),
                                &market,
                                None,
                                state.last_buy_order.as_ref().map(|b| b.timestamp_ms),
                                state.last_buy_order.as_ref().map(|b| b.side),
                                Some((entry_side, filled.clone())),
                            )
                            .await;
                        }
                    }
                }
            }
        }

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
            let old_market_for_end_log = state.market.clone();

            // Before logging ABANDONED: check via REST if the TP limit order was already filled
            // on the exchange (WS/loop may have missed the fill event at the interval boundary).
            if let Some(tp_oid) = state.tp_limit_order_id.clone() {
                if state.pending_auto_sell.is_some() {
                    match clob.get_order(&tp_oid).await {
                        Ok(order_info) => {
                            let status = order_info.get("status").and_then(|v| v.as_str()).unwrap_or("");
                            let size_matched = order_info
                                .get("size_matched")
                                .and_then(|v| v.as_str())
                                .and_then(|s| Decimal::from_str(s).ok())
                                .unwrap_or(Decimal::ZERO);
                            let tp_size = state.tp_placed_size.clone()
                                .or_else(|| state.pending_auto_sell.as_ref().map(|t| t.size.clone()))
                                .unwrap_or(Decimal::ZERO);
                            if (status.contains("MATCHED") || status.eq_ignore_ascii_case("FILLED"))
                                && size_matched >= tp_size * dec!(0.99)
                            {
                                // TP was filled — log correctly instead of ABANDONED
                                let exit_price = state.pending_auto_sell.as_ref()
                                    .map(|t| round_to_tick(t.target_price))
                                    .unwrap_or(Decimal::ZERO);
                                if let Some(ref buy) = state.last_buy_order {
                                    let pnl = (exit_price - buy.price) * size_matched.clone();
                                    let roi_pct = ((exit_price / buy.price) - Decimal::ONE) * dec!(100);
                                    let held_sec = now_ms_u.saturating_sub(buy.timestamp_ms) / 1000;
                                    info!(
                                        "[CLOSED] TP  {} entry={} exit={} size={} pnl={:+.4} ({:+.2}%) held={}s (detected at interval close via REST)",
                                        match buy.side { EntrySide::Up => "Up", EntrySide::Down => "Down" },
                                        fmt_decimal_2(&buy.price), fmt_decimal_2(&exit_price),
                                        fmt_decimal_2(&size_matched), pnl, roi_pct, held_sec
                                    );
                                }
                                info!("[IntervalSniper] ✓ TP limit filled @ interval boundary — position closed (REST confirmed)");
                                state.tp_limit_order_id = None;
                                state.tp_placed_size = None;
                                state.tp_cumulative_filled = Decimal::ZERO;
                                state.tp_last_order_filled = Decimal::ZERO;
                                state.tp_limit_balance_retries = 0;
                                state.sl_limit_order_id = None;
                                state.sl_limit_order_price = None;
                                state.sl_cumulative_filled = Decimal::ZERO;
                                state.sl_last_order_filled = Decimal::ZERO;
                                state.pending_auto_sell = None;
                                state.pending_stop_loss = None;
                                state.last_buy_order = None;
                                clear_pending_gtc(&mut state);
                                state.allowance_cache = None;
                            }
                        }
                        Err(e) => {
                            let err_str = e.to_string();
                            let is_404 = err_str.contains("404");
                            if is_404 {
                                // 404 at interval close usually means: order was cancelled when market resolved,
                                // or (rarely) order was filled and API no longer returns it. Use balance as fallback.
                                if let Some(ref tp) = state.pending_auto_sell {
                                    if let Ok(Some(bal)) = clob.get_available_balance(&tp.token_id).await {
                                        if bal < DUST_THRESHOLD {
                                            // Balance ~0 → position was closed (TP filled or settled at resolution)
                                            if let Some(ref buy) = state.last_buy_order {
                                                let exit_price = round_to_tick(tp.target_price.clone());
                                                let pnl = (exit_price.clone() - buy.price) * buy.size.clone();
                                                let roi_pct = ((exit_price / buy.price) - Decimal::ONE) * dec!(100);
                                                let held_sec = now_ms_u.saturating_sub(buy.timestamp_ms) / 1000;
                                                info!(
                                                    "[CLOSED] TP  {} entry={} exit={} size={} pnl={:+.4} ({:+.2}%) held={}s (inferred at interval close: get_order 404, balance≈0)",
                                                    match buy.side { EntrySide::Up => "Up", EntrySide::Down => "Down" },
                                                    fmt_decimal_2(&buy.price), fmt_decimal_2(&exit_price),
                                                    fmt_decimal_2(&buy.size), pnl, roi_pct, held_sec
                                                );
                                            }
                                            info!("[IntervalSniper] ✓ TP inferred filled @ interval boundary (get_order 404, balance≈0)");
                                            state.tp_limit_order_id = None;
                                            state.tp_placed_size = None;
                                            state.tp_cumulative_filled = Decimal::ZERO;
                                            state.tp_last_order_filled = Decimal::ZERO;
                                            state.tp_limit_balance_retries = 0;
                                            state.sl_limit_order_id = None;
                                            state.sl_limit_order_price = None;
                                            state.sl_cumulative_filled = Decimal::ZERO;
                                            state.sl_last_order_filled = Decimal::ZERO;
                                            state.pending_auto_sell = None;
                                            state.pending_stop_loss = None;
                                            state.last_buy_order = None;
                                            clear_pending_gtc(&mut state);
                                            state.allowance_cache = None;
                                        } else {
                                            warn!(
                                                "[IntervalSniper] TP order returned 404 at interval close (order likely cancelled when market resolved); balance={} — position still open",
                                                fmt_decimal_2(&bal)
                                            );
                                        }
                                    } else {
                                        warn!("[IntervalSniper] could not check TP order at interval close: {} (get_order 404; balance check failed)", e);
                                    }
                                } else {
                                    warn!("[IntervalSniper] could not check TP order at interval close: {}", e);
                                }
                            } else {
                                warn!("[IntervalSniper] could not check TP order at interval close: {}", e);
                            }
                        }
                    }
                }
            }

            // Log position close (MARKET_CLOSE) and interval summary for the market we're leaving
            if let Some(ref old_market) = state.market {
                if state.pending_auto_sell.is_some() || state.pending_stop_loss.is_some() {
                    if let Some(ref buy) = state.last_buy_order {
                        let last_bid = state.last_best_bid_for_position.unwrap_or(Decimal::ZERO);
                        let pnl = (last_bid - buy.price) * buy.size.clone();
                        let held_sec = now_ms_u.saturating_sub(buy.timestamp_ms) / 1000;
                        warn!(
                            "[ABANDONED] {} entry={} last_bid={} size={} unrealized_pnl={:+.4} held={}s — interval closed with open position",
                            match buy.side { EntrySide::Up => "Up", EntrySide::Down => "Down" },
                            fmt_decimal_2(&buy.price), fmt_decimal_2(&last_bid),
                            fmt_decimal_2(&buy.size), pnl, held_sec
                        );
                    }
                }
                if let Some(ref mut log) = state.session_log {
                    if state.pending_auto_sell.is_some() || state.pending_stop_loss.is_some() {
                        let (side, entry_price, entry_time_ms, size) =
                            if let Some(ref buy) = state.last_buy_order {
                                (
                                    buy.side,
                                    buy.price,
                                    buy.timestamp_ms,
                                    buy.size.clone(),
                                )
                            } else if let Some(ref sl) = state.pending_stop_loss {
                                let side = if sl.token_id == old_market.token_id_up {
                                    EntrySide::Up
                                } else {
                                    EntrySide::Down
                                };
                                (
                                    side,
                                    sl.entry_price,
                                    sl.placed_at_ms,
                                    sl.size.clone(),
                                )
                            } else if let Some(ref tp) = state.pending_auto_sell {
                                let side = if tp.token_id == old_market.token_id_up {
                                    EntrySide::Up
                                } else {
                                    EntrySide::Down
                                };
                                (
                                    side,
                                    state.pending_stop_loss.as_ref().map(|s| s.entry_price).unwrap_or(Decimal::ZERO),
                                    tp.placed_at_ms,
                                    tp.size.clone(),
                                )
                            } else {
                                (EntrySide::Up, Decimal::ZERO, now_ms_u, Decimal::ZERO)
                            };
                        let exit_price = state.last_best_bid_for_position.unwrap_or(Decimal::ZERO);
                        if size > Decimal::ZERO {
                            let entry_order_id = state
                                .last_buy_order
                                .as_ref()
                                .and_then(|b| b.order_id.as_deref());
                            let _ = log.log_position_close(
                                &old_market.slug,
                                old_market.interval_start_unix,
                                old_market.close_time_unix,
                                side,
                                entry_price,
                                exit_price,
                                entry_time_ms,
                                now_ms_u,
                                ExitType::MarketClose,
                                size,
                                None,
                                entry_order_id,
                                None,
                                state.interval_min_bid_up,
                                state.interval_max_bid_up,
                                state.interval_min_bid_down,
                                state.interval_max_bid_down,
                                None,
                                None,
                                None,
                                false,
                            );
                        }
                    }
                    let _ = log.log_interval_summary(
                        &old_market.slug,
                        old_market.interval_start_unix,
                        old_market.close_time_unix,
                        state.interval_min_bid_up,
                        state.interval_max_bid_up,
                        state.interval_min_bid_down,
                        state.interval_max_bid_down,
                    );
                }
            }
            match fetch_market_by_slug(&http, &config.gamma_base_url, &current_slug).await {
                Ok(market) => {
                    // Log interval summary before reset
                    if let Some(ref old_market) = old_market_for_end_log {
                        if state.trades_this_interval > 0 || state.ordered_this_interval {
                            info!(
                                "[INTERVAL] END {}  trades={}  total_size={}  {}",
                                old_market.slug.chars().rev().take(14).collect::<String>().chars().rev().collect::<String>(),
                                state.trades_this_interval,
                                fmt_decimal_2(&state.total_shares_this_interval),
                                if state.pending_auto_sell.is_some() { "position=OPEN(abandoned)" } else { "position=closed" }
                            );
                        }
                    }
                    state.ws_book = None; // drop previous WS before creating new (per-market)
                    // ws_user is persistent: connect once with empty markets to receive all fills (no race on interval switch)
                    let ws_url = ClobWsBook::ws_url_from_rest_host(&clob_host);
                    info!("[WS] connecting order book for new interval...");
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
                    if !state.config.dry_run && state.ws_user.is_none() {
                        let ws_user_url = ClobWsUser::ws_url_from_rest_host(&clob_host);
                        // Empty markets = receive events for all markets (Polymarket API); avoids race with subscription delay
                        info!("[WS] connecting user channel...");
                        match ClobWsUser::connect(&ws_user_url, &[]).await {
                            Ok(ws_u) => {
                                state.ws_user = Some(Arc::new(ws_u));
                                info!("[IntervalSniper] WebSocket user channel connected (order/trade updates, all markets)");
                                info!("[WS] user channel active — fills via WS (0ms lag), REST is fallback only");
                            }
                            Err(e) => {
                                warn!(
                                    "[IntervalSniper] WebSocket user channel connect failed: {}, using balance for fills",
                                    e
                                );
                            }
                        }
                    }
                    // Clear accumulated WS fill state so each interval starts at 0:
                    // old tokens (previous interval) and new tokens (this interval).
                    if let Some(ref ws) = state.ws_user {
                        if let Some(ref old_market) = state.market {
                            ws.clear_token_state(&old_market.token_id_up).await;
                            ws.clear_token_state(&old_market.token_id_down).await;
                        }
                        ws.clear_token_state(&market.token_id_up).await;
                        ws.clear_token_state(&market.token_id_down).await;
                    }
                    state.market = Some(market.clone());
                    if !config.dry_run {
                        let has_position = state.pending_auto_sell.is_some() || state.pending_stop_loss.is_some();
                        let clob_check = Arc::clone(&clob);
                        let token_up = market.token_id_up.clone();
                        let token_down = market.token_id_down.clone();
                        tokio::spawn(async move {
                            for (label, token) in [("Up", token_up.as_str()), ("Down", token_down.as_str())] {
                                match clob_check.as_ref().get_balance_allowance(token).await {
                                    Ok(raw) => {
                                        if let Ok(json) = serde_json::from_str::<serde_json::Value>(&raw) {
                                            let bal = json.get("balance").and_then(|v| v.as_str()).unwrap_or("?");
                                            let allow = json.get("allowance").and_then(|v| v.as_str()).unwrap_or("?");
                                            if has_position {
                                                info!("[Allowance] {} token balance={} allowance={}", label, bal, allow);
                                            } else {
                                                debug!("[Allowance] {} token balance={} allowance={}", label, bal, allow);
                                            }
                                        }
                                    }
                                    Err(e) => {
                                        if has_position {
                                            warn!("[Allowance] could not fetch for {} token: {}", label, e);
                                        } else {
                                            debug!("[Allowance] could not fetch for {} token: {}", label, e);
                                        }
                                    }
                                }
                            }
                        });
                    }
                    // Cancel any resting GTC buy order from the previous interval so we don't
                    // leave it on the book and allow a second buy in the new interval.
                    if let Some(ref token_id) = state.pending_gtc_token_id {
                        if state.pending_gtc_order_id.is_some() && !config.dry_run {
                            match clob.cancel_orders_for_token(token_id).await {
                                Ok(res) => {
                                    if !res.canceled.is_empty() {
                                        info!(
                                            "[IntervalSniper] canceled {} resting GTC order(s) at interval switch",
                                            res.canceled.len()
                                        );
                                    }
                                    if !res.not_canceled.is_empty() {
                                        warn!(
                                            "[IntervalSniper] {} order(s) not canceled at interval switch: {:?}",
                                            res.not_canceled.len(),
                                            res.not_canceled.keys().collect::<Vec<_>>()
                                        );
                                    }
                                }
                                Err(e) => {
                                    warn!(
                                        "[IntervalSniper] failed to cancel resting GTC order at interval switch: {}",
                                        e
                                    );
                                }
                            }
                        }
                    }
                    state.ordered_this_interval = false;
                    state.trades_this_interval = 0;
                    state.re_entry_allowed_after_sl = false;
                    state.total_shares_this_interval = Decimal::ZERO;
                    state.last_buy_order = None; state.balance_reflected_at_ms = None; state.balance_delay_clob_logged = false; state.last_logged_balance_up = None; state.last_logged_balance_down = None;
                    state.pending_gtc_order_id = None;
                    state.pending_gtc_token_id = None;
                    state.pending_gtc_side = None;
                    state.pending_gtc_price = None;
                    state.pending_gtc_requested_size = None;
                    state.pending_gtc_timestamp_ms = None;
                    state.pending_gtc_last_observed_filled = None;
                    state.pending_gtc_fill_deltas.clear();
                    state.pending_auto_sell = None;
                    state.pending_stop_loss = None;
                    state.auto_sell_placed = false;
                    state.stop_loss_placed = false;
                    state.tp_limit_order_id = None;
                    state.tp_placed_size = None;
                    state.tp_cumulative_filled = Decimal::ZERO;
                    state.tp_last_order_filled = Decimal::ZERO;
                    state.tp_limit_balance_retries = 0;
                    state.tp_limit_last_rest_check_ms = None;
                    state.sl_limit_order_id = None;
                    state.sl_limit_order_price = None;
                    state.sl_cumulative_filled = Decimal::ZERO;
                    state.sl_last_order_filled = Decimal::ZERO;
                    state.sl_limit_last_rest_check_ms = None;
                    state.allowance_cache = None;
                    state.interval_switch_wall_time_ms = Some(now_ms_u);
                    state.interval_min_bid_up = None;
                    state.interval_max_bid_up = None;
                    state.interval_min_bid_down = None;
                    state.interval_max_bid_down = None;
                    state.last_best_bid_for_position = None;
                    state.last_balance_log_ms = None;
                    state.balance_reflected_at_ms = None; state.balance_delay_clob_logged = false; state.last_logged_balance_up = None; state.last_logged_balance_down = None;
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

        let market = match state.market.as_ref() {
            Some(m) => m.clone(),
            None => {
                tokio::time::sleep(Duration::from_millis(loop_ms)).await;
                continue;
            }
        };

        let secs_to_close = seconds_to_close(now_u, market.close_time_unix);

        // Log CLOB balance every 1000 ms and, after a buy, delay until balance reflected.
        let ws_user_arc = state.ws_user.clone();
        let ws_user_ref = ws_user_arc.as_ref().map(|a| a.as_ref());
        let _ = log_clob_balance_if_due(clob.as_ref().as_ref(), &market, &mut state, now_ms_u, ws_user_ref).await;

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

        let token_id_up = market.token_id_up.clone();
        let token_id_down = market.token_id_down.clone();
        if let Some(ref ws) = state.ws_user {
            ws.set_active_token_ids(vec![token_id_up.clone(), token_id_down.clone()]).await;
        }
        update_interval_bids(&mut state, &token_id_up, &token_id_down, &top);
        // market is already the clone from above; do not re-borrow state.market so clear_pending_gtc(&mut state) is allowed later

        // Periodic log: order book scan (real-time visibility) — debug only so terminal shows only buy/sell events
        if tick_count % LOG_BOOK_EVERY_TICKS == 0 {
            let up = top.token_id_up.as_ref();
            let down = top.token_id_down.as_ref();
            debug!(
                "[IntervalSniper] order book Up bid={} ask={} | Down bid={} ask={} | secs_to_close={}",
                fmt_price(up.and_then(|s| s.best_bid.as_ref())),
                fmt_price(up.and_then(|s| s.best_ask.as_ref())),
                fmt_price(down.and_then(|s| s.best_bid.as_ref())),
                fmt_price(down.and_then(|s| s.best_ask.as_ref())),
                fmt_secs(secs_to_close)
            );
            // When position open, log TP/SL monitoring so user sees we're checking for fills (debug only)
            if let Some(ref tp) = state.pending_auto_sell {
                if !state.auto_sell_placed {
                    let is_up = tp.token_id == market.token_id_up;
                    let side_book = if is_up {
                        &top.token_id_up
                    } else {
                        &top.token_id_down
                    };
                    debug!(
                        "[IntervalSniper]  POS   TP   target={}  best_ask={}  (place limit at target when ask <= target; cancel if bid <= entry)",
                        fmt_price(Some(&tp.target_price)),
                        fmt_price(side_book.as_ref().and_then(|s| s.best_ask.as_ref()))
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
                    debug!(
                        "[IntervalSniper]  POS   SL   trigger={}  best_bid={}  (sell when bid <= trigger + margin)",
                        fmt_price(Some(&sl.trigger_price)),
                        fmt_price(side_book.as_ref().and_then(|s| s.best_bid.as_ref()))
                    );
                }
            }
        }

        // Stop loss: limit-order style (like TP). When bid <= trigger + SL_TRIGGER_MARGIN, place GTC limit at best_bid
        // and keep trying every tick while in zone (no resting SL order). If bid drops before fill, cancel and replace.
        // Detect 100% fill via WS user. Use same `top` as main tick for consistent book and minimal latency (no extra WS/REST call).
        if state.config.enable_stop_loss {
            if let Some(ref sl) = state.pending_stop_loss.clone() {
                if !state.stop_loss_placed {
                    let is_up = sl.token_id == market.token_id_up;
                    let side_book = if is_up { &top.token_id_up } else { &top.token_id_down };
                    let best_bid = side_book.as_ref().and_then(|s| s.best_bid).unwrap_or(Decimal::ZERO);

                    // Interval ended: cancel SL order so we don't leave a resting order in wrong interval.
                    let now_check = now_unix();
                    if now_check >= market.close_time_unix
                        || current_5min_slug(config.interval_market) != market.slug
                    {
                        if state.sl_limit_order_id.is_some() {
                            let _ = clob.cancel_orders_for_token(&sl.token_id).await;
                            state.sl_limit_order_id = None;
                            state.sl_limit_order_price = None;
                        }
                    } else if best_bid > sl.trigger_price + SL_TRIGGER_MARGIN && state.sl_limit_order_id.is_some() {
                        // Price recovered above SL zone: cancel resting SL limit so balance is freed and we can place TP again.
                        let cancel_result = clob.cancel_orders_for_token(&sl.token_id).await;
                        if let Some(ws_user) = ws_user_ref {
                            if let Some(ref oid) = state.sl_limit_order_id {
                                if let Some(final_fill) = ws_user.get_order_filled_size_sell(oid).await {
                                    let delta = final_fill - state.sl_last_order_filled;
                                    if delta > Decimal::ZERO {
                                        state.sl_cumulative_filled += delta;
                                    }
                                }
                            }
                        }
                        let sl_filled_via_cancel = cancel_result
                            .as_ref()
                            .ok()
                            .and_then(|res| state.sl_limit_order_id.as_ref().and_then(|oid| res.not_canceled.get(oid.as_str())))
                            .map(|r| {
                                let lower = r.to_lowercase();
                                lower.contains("matched")
                            })
                            .unwrap_or(false);
                        if sl_filled_via_cancel {
                            state.sl_cumulative_filled = sl.size.clone();
                            let order_price = state.sl_limit_order_price.unwrap_or(best_bid);
                            let total_filled = if state.tp_cumulative_filled > Decimal::ZERO {
                                sl.size.clone() - state.tp_cumulative_filled.clone()
                            } else {
                                state.sl_cumulative_filled.clone()
                            };
                            if let Some(ref buy) = state.last_buy_order {
                                let pnl = (order_price - buy.price) * total_filled.clone();
                                let roi_pct = ((order_price / buy.price) - Decimal::ONE) * dec!(100);
                                let held_sec = now_ms_u.saturating_sub(buy.timestamp_ms) / 1000;
                                info!(
                                    "[CLOSED] SL  {} entry={} exit={} size={} pnl={:+.4} ({:+.2}%) held={}s (filled when price recovered)",
                                    match buy.side { EntrySide::Up => "Up", EntrySide::Down => "Down" },
                                    fmt_decimal_2(&buy.price), fmt_decimal_2(&order_price),
                                    fmt_decimal_2(&total_filled), pnl, roi_pct, held_sec
                                );
                            }
                            if let Some(ref mut log) = state.session_log {
                                if let Some(ref buy) = state.last_buy_order {
                                    let _ = log.log_position_close(
                                        &market.slug, market.interval_start_unix, market.close_time_unix,
                                        buy.side, buy.price, order_price, buy.timestamp_ms, now_ms_u,
                                        ExitType::StopLoss, total_filled.clone(), None,
                                        buy.order_id.as_deref(), state.sl_limit_order_id.as_deref(),
                                        state.interval_min_bid_up, state.interval_max_bid_up,
                                        state.interval_min_bid_down, state.interval_max_bid_down,
                                        None, None, None, false,
                                    );
                                }
                            }
                            info!("[IntervalSniper] ✓ SL limit filled on recovery (order matched on cancel), position closed");
                            state.stop_loss_placed = true;
                            state.auto_sell_placed = true;
                            state.re_entry_allowed_after_sl = true;
                            state.tp_limit_order_id = None;
                            state.tp_placed_size = None;
                            state.tp_cumulative_filled = Decimal::ZERO;
                            state.tp_last_order_filled = Decimal::ZERO;
                            state.tp_limit_balance_retries = 0;
                            state.sl_limit_order_id = None;
                            state.sl_limit_order_price = None;
                            state.sl_cumulative_filled = Decimal::ZERO;
                            state.sl_last_order_filled = Decimal::ZERO;
                            state.sl_limit_last_rest_check_ms = None;
                            state.pending_auto_sell = None;
                            state.pending_stop_loss = None;
                            state.allowance_cache = None;
                            state.last_buy_order = None;
                            clear_pending_gtc(&mut state);
                            state.balance_reflected_at_ms = None;
                            state.balance_delay_clob_logged = false;
                            state.last_logged_balance_up = None;
                            state.last_logged_balance_down = None;
                            state.total_shares_this_interval = Decimal::ZERO;
                            if let Some(ws) = ws_user_ref {
                                ws.clear_token_state(&sl.token_id).await;
                            }
                        } else {
                            state.sl_limit_order_id = None;
                            state.sl_limit_order_price = None;
                            state.sl_last_order_filled = Decimal::ZERO;
                            state.allowance_cache = None;
                            info!(
                                "[IntervalSniper] SL limit canceled (price recovered bid {} > trigger {}), balance freed for TP",
                                fmt_price(Some(&best_bid)), fmt_price(Some(&sl.trigger_price))
                            );
                        }
                    } else if best_bid > Decimal::ZERO && best_bid <= sl.trigger_price + SL_TRIGGER_MARGIN {
                        // In SL zone (bid <= trigger + margin): try to place SL limit every tick while we have position and no resting SL order.
                        // First time in zone: cancel TP so balance is free for SL limit order.
                        if state.sl_limit_order_id.is_none() && state.sl_cumulative_filled.is_zero() {
                            let cancel_result = clob.cancel_orders_for_token(&sl.token_id).await;
                            match cancel_result {
                                Err(e) => warn!("[IntervalSniper] cancel orders before SL limit failed: {} (continuing)", e),
                                Ok(ref res) if !res.not_canceled.is_empty() => {
                                    warn!("[IntervalSniper] cancel before SL: {} order(s) not canceled", res.not_canceled.len());
                                }
                                _ => {}
                            }
                            state.tp_limit_order_id = None;
                            state.tp_placed_size = None;
                            state.tp_limit_balance_retries = 0;
                            state.allowance_cache = None;
                            // Wait for REST balance to reflect fill (same as TP): retry every 200ms until interval ends or balance >= fill size.
                            loop {
                                if now_unix() >= market.close_time_unix {
                                    break;
                                }
                                state.allowance_cache = None;
                                let freed = clob.get_available_balance(&sl.token_id).await.ok().flatten();
                                if freed.map_or(false, |a| a >= sl.size * dec!(0.90)) {
                                    break;
                                }
                                tokio::time::sleep(Duration::from_millis(TP_SL_BALANCE_RETRY_MS)).await;
                            }
                            state.allowance_cache = None;
                            info!(
                                "[IntervalSniper] SL TRIGGERED: bid {} in SL zone (trigger {} + margin) — placing limit at best_bid (cancel+replace if bid drops)",
                                fmt_price(Some(&best_bid)), fmt_price(Some(&sl.trigger_price))
                            );
                        }

                        let remaining = sl.size - state.sl_cumulative_filled.clone();

                        // 1) If we have an SL limit order: check WS fill, or cancel+replace if bid dropped.
                        if let Some(ref oid) = state.sl_limit_order_id {
                            let order_price = state.sl_limit_order_price.unwrap_or(Decimal::ZERO);

                            // Detect fill via WS user (partial or full for this order).
                            if let Some(ws_user) = ws_user_ref {
                                if let Some((filled_this_order, fill_event_type)) = ws_user.get_order_filled_size_sell_with_type(oid).await {
                                    let delta = filled_this_order - state.sl_last_order_filled;
                                    if delta > Decimal::ZERO {
                                        state.sl_cumulative_filled += delta;
                                        state.sl_last_order_filled = filled_this_order;
                                        info!(
                                            "[IntervalSniper] SL fill via WS ({}): +{} (total {}/{}), order_id={}",
                                            fill_event_type, fmt_decimal_2(&delta), fmt_decimal_2(&state.sl_cumulative_filled), fmt_decimal_2(&sl.size), oid
                                        );
                                    }
                                }
                            }

                            // REST fallback for SL fill (when WS unavailable or missed event); throttle 5s.
                            let should_check_sl_rest = ws_user_ref.is_none()
                                || state.sl_limit_last_rest_check_ms
                                    .map(|last| now_ms_u.saturating_sub(last) >= 5000)
                                    .unwrap_or(true);
                            if should_check_sl_rest {
                                state.sl_limit_last_rest_check_ms = Some(now_ms_u);
                                if let Ok(order_info) = clob.get_order(oid).await {
                                    let _status = order_info.get("status").and_then(|v| v.as_str()).unwrap_or("");
                                    let size_matched = order_info
                                        .get("size_matched")
                                        .and_then(|v| v.as_str())
                                        .and_then(|s| Decimal::from_str(s).ok())
                                        .unwrap_or(Decimal::ZERO);
                                    if size_matched > state.sl_last_order_filled {
                                        let delta = size_matched - state.sl_last_order_filled;
                                        state.sl_cumulative_filled += delta;
                                        state.sl_last_order_filled = size_matched;
                                        debug!(
                                            "[IntervalSniper] SL limit fill +{} (REST, total {}/{}), order_id={}",
                                            fmt_decimal_2(&delta), fmt_decimal_2(&state.sl_cumulative_filled), fmt_decimal_2(&sl.size), oid
                                        );
                                    }
                                }
                            }

                            // Balance-dust fallback: if available balance is dust, order may be filled and WS/REST missed it.
                            // IMPORTANT: a resting SL limit order locks the allowance, so available can be ~0 even when NOT filled.
                            // Always verify via REST get_order status before concluding position is closed.
                            let sl_available = get_available_for_sell(
                                clob.as_ref().as_ref(),
                                ws_user_ref,
                                &sl.token_id,
                                &mut state.allowance_cache,
                                true,
                            ).await;
                            if sl_available.map_or(false, |a| a <= SL_BALANCE_DUST_CLOSE) {
                                let implied_filled = sl.size - sl_available.unwrap_or(Decimal::ZERO);
                                if implied_filled >= sl.size * dec!(0.99) {
                                    // Verify via REST that the order is actually MATCHED, not just locking allowance.
                                    let order_confirmed_filled = match clob.get_order(oid).await {
                                        Ok(info) => {
                                            let status = info.get("status").and_then(|v| v.as_str()).unwrap_or("");
                                            status.contains("MATCHED") || status.eq_ignore_ascii_case("FILLED")
                                        }
                                        Err(_) => false,
                                    };
                                    if order_confirmed_filled {
                                        state.sl_cumulative_filled = sl.size;
                                        info!(
                                            "[IntervalSniper] SL position closed (balance dust {}, REST confirmed MATCHED), allowing re-entry",
                                            fmt_decimal_2(&sl_available.unwrap_or(Decimal::ZERO))
                                        );
                                    } else {
                                        debug!(
                                            "[IntervalSniper] SL balance dust {} but order not yet MATCHED (resting) — skipping dust close",
                                            fmt_decimal_2(&sl_available.unwrap_or(Decimal::ZERO))
                                        );
                                    }
                                } else if state.tp_cumulative_filled > Decimal::ZERO {
                                    // Partial TP: remaining sold at SL. Verify REST before closing.
                                    let order_confirmed_filled = match clob.get_order(oid).await {
                                        Ok(info) => {
                                            let status = info.get("status").and_then(|v| v.as_str()).unwrap_or("");
                                            status.contains("MATCHED") || status.eq_ignore_ascii_case("FILLED")
                                        }
                                        Err(_) => false,
                                    };
                                    if order_confirmed_filled {
                                        state.sl_cumulative_filled = sl.size;
                                        info!(
                                            "[IntervalSniper] SL position closed (balance dust {}, had partial TP, REST confirmed), allowing re-entry",
                                            fmt_decimal_2(&sl_available.unwrap_or(Decimal::ZERO))
                                        );
                                    }
                                }
                            }

                            // Position closed when cumulative >= 99% of size.
                            if state.sl_cumulative_filled >= sl.size * dec!(0.99) {
                                let exit_price = order_price;
                                let total_filled = if state.tp_cumulative_filled > Decimal::ZERO {
                                    sl.size.clone() - state.tp_cumulative_filled.clone()
                                } else {
                                    state.sl_cumulative_filled.clone()
                                };
                                if let Some(ref buy) = state.last_buy_order {
                                    let pnl = (exit_price - buy.price) * total_filled.clone();
                                    let roi_pct = ((exit_price / buy.price) - Decimal::ONE) * dec!(100);
                                    let held_sec = now_ms_u.saturating_sub(buy.timestamp_ms) / 1000;
                                    info!(
                                        "[CLOSED] SL  {} entry={} exit={} size={} pnl={:+.4} ({:+.2}%) held={}s (limit @ best_bid)",
                                        match buy.side { EntrySide::Up => "Up", EntrySide::Down => "Down" },
                                        fmt_decimal_2(&buy.price), fmt_decimal_2(&exit_price),
                                        fmt_decimal_2(&total_filled), pnl, roi_pct, held_sec
                                    );
                                }
                                // Don't block on real_exit_usd: session_log uses computed value (size*exit_price); Telegram gets message immediately.
                                if let Some(ref mut log) = state.session_log {
                                    if let Some(ref buy) = state.last_buy_order {
                                        let _ = log.log_position_close(
                                            &market.slug, market.interval_start_unix, market.close_time_unix,
                                            buy.side, buy.price, exit_price, buy.timestamp_ms, now_ms_u,
                                            ExitType::StopLoss, total_filled.clone(), None,
                                            buy.order_id.as_deref(), state.sl_limit_order_id.as_deref(),
                                            state.interval_min_bid_up, state.interval_max_bid_up,
                                            state.interval_min_bid_down, state.interval_max_bid_down,
                                            None,
                                            None,
                                            None,
                                            false,
                                        );
                                    }
                                }
                                info!("[IntervalSniper] ✓ SL limit filled @ {} — position closed (re-entry allowed)", fmt_price(Some(&exit_price)));
                                state.stop_loss_placed = true;
                                state.auto_sell_placed = true;
                                state.re_entry_allowed_after_sl = true;
                                state.tp_limit_order_id = None;
                                state.tp_placed_size = None;
                                state.tp_cumulative_filled = Decimal::ZERO;
                                state.tp_last_order_filled = Decimal::ZERO;
                                state.tp_limit_balance_retries = 0;
                                state.sl_limit_order_id = None;
                                state.sl_limit_order_price = None;
                                state.sl_cumulative_filled = Decimal::ZERO;
                                state.sl_last_order_filled = Decimal::ZERO;
                                state.sl_limit_last_rest_check_ms = None;
                                state.pending_auto_sell = None;
                                state.pending_stop_loss = None;
                                state.allowance_cache = None;
                                state.last_buy_order = None;
                                clear_pending_gtc(&mut state);
                                state.balance_reflected_at_ms = None;
                                state.balance_delay_clob_logged = false;
                                state.last_logged_balance_up = None;
                                state.last_logged_balance_down = None;
                                state.total_shares_this_interval = Decimal::ZERO;
                                // Clear WS token state so next balance log doesn't show stale aggregated fills (e.g. 72.13).
                                if let Some(ws) = ws_user_ref {
                                    ws.clear_token_state(&sl.token_id).await;
                                }
                            } else if best_bid < order_price {
                                // Bid dropped: cancel and replace at new best_bid next iteration.
                                let cancel_result = clob.cancel_orders_for_token(&sl.token_id).await;
                                if let Some(ws_user) = ws_user_ref {
                                    if let Some(final_fill) = ws_user.get_order_filled_size_sell(oid).await {
                                        let delta = final_fill - state.sl_last_order_filled;
                                        if delta > Decimal::ZERO {
                                            state.sl_cumulative_filled += delta;
                                        }
                                    }
                                }
                                // If cancel failed because order was already matched, SL filled on exchange — mark as filled so next iteration runs position-closed + re-entry.
                                let sl_filled_via_cancel = cancel_result
                                    .as_ref()
                                    .ok()
                                    .and_then(|res| res.not_canceled.get(oid.as_str()))
                                    .map(|r| {
                                        let lower = r.to_lowercase();
                                        lower.contains("matched")
                                    })
                                    .unwrap_or(false);
                                if sl_filled_via_cancel {
                                    state.sl_cumulative_filled = sl.size.clone();
                                    info!(
                                        "[IntervalSniper] SL position closed (order already matched on cancel), allowing re-entry"
                                    );
                                    // Run full cleanup immediately: next tick price may be above trigger so we'd never enter this block again.
                                    let exit_price = order_price;
                                    let total_filled = if state.tp_cumulative_filled > Decimal::ZERO {
                                        sl.size.clone() - state.tp_cumulative_filled.clone()
                                    } else {
                                        state.sl_cumulative_filled.clone()
                                    };
                                    if let Some(ref buy) = state.last_buy_order {
                                        let pnl = (exit_price - buy.price) * total_filled.clone();
                                        let roi_pct = ((exit_price / buy.price) - Decimal::ONE) * dec!(100);
                                        let held_sec = now_ms_u.saturating_sub(buy.timestamp_ms) / 1000;
                                        info!(
                                            "[CLOSED] SL  {} entry={} exit={} size={} pnl={:+.4} ({:+.2}%) held={}s (limit @ best_bid)",
                                            match buy.side { EntrySide::Up => "Up", EntrySide::Down => "Down" },
                                            fmt_decimal_2(&buy.price), fmt_decimal_2(&exit_price),
                                            fmt_decimal_2(&total_filled), pnl, roi_pct, held_sec
                                        );
                                    }
                                    if let Some(ref mut log) = state.session_log {
                                        if let Some(ref buy) = state.last_buy_order {
                                            let _ = log.log_position_close(
                                                &market.slug, market.interval_start_unix, market.close_time_unix,
                                                buy.side, buy.price, exit_price, buy.timestamp_ms, now_ms_u,
                                                ExitType::StopLoss, total_filled.clone(), None,
                                                buy.order_id.as_deref(), state.sl_limit_order_id.as_deref(),
                                                state.interval_min_bid_up, state.interval_max_bid_up,
                                                state.interval_min_bid_down, state.interval_max_bid_down,
                                                None,
                                                None,
                                                None,
                                                false,
                                            );
                                        }
                                    }
                                    info!("[IntervalSniper] ✓ SL limit filled @ {} — position closed (re-entry allowed)", fmt_price(Some(&exit_price)));
                                    state.stop_loss_placed = true;
                                    state.auto_sell_placed = true;
                                    state.re_entry_allowed_after_sl = true;
                                    state.tp_limit_order_id = None;
                                    state.tp_placed_size = None;
                                    state.tp_cumulative_filled = Decimal::ZERO;
                                    state.tp_last_order_filled = Decimal::ZERO;
                                    state.tp_limit_balance_retries = 0;
                                    state.sl_limit_order_id = None;
                                    state.sl_limit_order_price = None;
                                    state.sl_cumulative_filled = Decimal::ZERO;
                                    state.sl_last_order_filled = Decimal::ZERO;
                                    state.sl_limit_last_rest_check_ms = None;
                                    state.pending_auto_sell = None;
                                    state.pending_stop_loss = None;
                                    state.allowance_cache = None;
                                    state.last_buy_order = None;
                                    clear_pending_gtc(&mut state);
                                    state.balance_reflected_at_ms = None;
                                    state.balance_delay_clob_logged = false;
                                    state.last_logged_balance_up = None;
                                    state.last_logged_balance_down = None;
                                    state.total_shares_this_interval = Decimal::ZERO;
                                    if let Some(ws) = ws_user_ref {
                                        ws.clear_token_state(&sl.token_id).await;
                                    }
                                } else {
                                    state.sl_limit_order_id = None;
                                    state.sl_limit_order_price = None;
                                    state.sl_last_order_filled = Decimal::ZERO;
                                    state.allowance_cache = None;
                                    trace!(
                                        "[IntervalSniper] SL limit canceled (bid {} < order {}), will replace at new best_bid",
                                        fmt_price(Some(&best_bid)), fmt_price(Some(&order_price))
                                    );
                                }
                            }
                        }

                        // 2) Place GTC limit at best_bid when we have no resting SL order and remaining to sell.
                        // Require last_buy_order.is_some() so we skip placing after "already matched" cleanup in same tick.
                        if state.sl_limit_order_id.is_none() && state.last_buy_order.is_some() && remaining >= DUST_THRESHOLD {
                            if state.sl_cumulative_filled >= sl.size * dec!(0.99) {
                                // Already closed above (WS detected 100%)
                            } else {
                                // With WS: only use MATCHED flow when we have matched buy for this token; else legacy retry loop.
                                let confirmed_ok = match ws_user_ref {
                                    Some(ws) => ws
                                        .get_confirmed_buy_size(&sl.token_id)
                                        .await
                                        .map(|c| c >= remaining.clone() * dec!(0.99))
                                        .unwrap_or(false),
                                    None => false,
                                };
                                if confirmed_ok {
                                    // SL: retry every 200ms until place_sell_order succeeds or interval ends or price exits SL zone.
                                    let expected_size = floor_to_decimals(remaining.clone().min(sl.size.clone()), SELL_SIZE_DECIMALS);
                                    let params = BalanceAllowanceParams {
                                        asset_type: AssetType::Conditional,
                                        token_id: Some(sl.token_id.clone()),
                                    };
                                    let mut sl_placed = false;
                                    loop {
                                        if now_unix() >= market.close_time_unix
                                            || current_5min_slug(config.interval_market) != market.slug
                                        {
                                            trace!("[IntervalSniper] SL place deferred: interval ended, will retry next tick");
                                            break;
                                        }
                                        let top_recheck = if let Some(ref ws) = state.ws_book {
                                            ws.get_top_of_book().await
                                        } else {
                                            match fetch_top_of_book(&http, &clob_host, &market.token_id_up, &market.token_id_down).await {
                                                Ok(t) => t,
                                                Err(_) => top.clone(),
                                            }
                                        };
                                        let side_recheck = if is_up { &top_recheck.token_id_up } else { &top_recheck.token_id_down };
                                        let recheck_bid = side_recheck.as_ref().and_then(|s| s.best_bid).unwrap_or(Decimal::ZERO);
                                        if recheck_bid > sl.trigger_price + SL_TRIGGER_MARGIN {
                                            trace!(
                                                "[IntervalSniper] SL place deferred: bid {} above target ({}), will retry next tick",
                                                fmt_price(Some(&recheck_bid)),
                                                fmt_price(Some(&sl.trigger_price))
                                            );
                                            break;
                                        }
                                        state.allowance_cache = None;
                                        let _ = clob.as_ref().update_balance_allowance(&params).await;
                                        let bal = clob.get_available_balance(&sl.token_id).await.ok().flatten();
                                        if bal.as_ref().map(|b| *b >= expected_size.clone() * dec!(0.99)).unwrap_or(false) {
                                            let size_to_place = bal
                                                .map(|b| floor_to_decimals(expected_size.clone().min(b), SELL_SIZE_DECIMALS))
                                                .unwrap_or_else(|| expected_size.clone());
                                            if size_to_place >= MIN_SELL_SIZE && size_to_place >= DUST_THRESHOLD {
                                                let price = round_to_tick(recheck_bid);
                                                let result = clob
                                                    .place_sell_order(
                                                        &sl.token_id,
                                                        price,
                                                        size_to_place.clone(),
                                                        crate::types::SellOrderTimeInForce::Gtc,
                                                    )
                                                    .await?;
                                                if result.success {
                                                    state.sl_limit_order_id = result.order_id.clone();
                                                    state.sl_limit_order_price = Some(price);
                                                    state.sl_last_order_filled = Decimal::ZERO;
                                                    info!(
                                                        "[IntervalSniper] SL limit placed @ {} size={} order_id={:?} (cancel+replace if bid drops)",
                                                        fmt_price(Some(&price)),
                                                        fmt_decimal_2(&size_to_place),
                                                        result.order_id
                                                    );
                                                    sl_placed = true;
                                                    break;
                                                }
                                            }
                                        }
                                        tokio::time::sleep(Duration::from_millis(TP_SL_BALANCE_RETRY_MS)).await;
                                    }
                                    if sl_placed {
                                        // Fast follow-down: recheck bid every SL_FOLLOW_DOWN_MS and cancel+replace if it dropped again.
                                        for _ in 0..SL_FOLLOW_DOWN_MAX_RETRIES {
                                            tokio::time::sleep(Duration::from_millis(SL_FOLLOW_DOWN_MS)).await;
                                            if now_unix() >= market.close_time_unix
                                                || current_5min_slug(config.interval_market) != market.slug
                                            {
                                                break;
                                            }
                                            let oid = match &state.sl_limit_order_id {
                                                Some(id) => id.clone(),
                                                None => break,
                                            };
                                            if state.sl_cumulative_filled >= sl.size * dec!(0.99) {
                                                break;
                                            }
                                            let order_price = state.sl_limit_order_price.unwrap_or(Decimal::ZERO);
                                            let top_fd = if let Some(ref ws) = state.ws_book {
                                                ws.get_top_of_book().await
                                            } else {
                                                match fetch_top_of_book(&http, &clob_host, &market.token_id_up, &market.token_id_down).await {
                                                    Ok(t) => t,
                                                    Err(_) => break,
                                                }
                                            };
                                            let side_fd = if is_up { &top_fd.token_id_up } else { &top_fd.token_id_down };
                                            let bid_fd = side_fd.as_ref().and_then(|s| s.best_bid).unwrap_or(Decimal::ZERO);
                                            if bid_fd >= order_price || bid_fd <= Decimal::ZERO {
                                                break;
                                            }
                                            let cancel_result = clob.cancel_orders_for_token(&sl.token_id).await;
                                            if let Some(ws_user) = ws_user_ref {
                                                if let Some(final_fill) = ws_user.get_order_filled_size_sell(&oid).await {
                                                    let delta = final_fill - state.sl_last_order_filled;
                                                    if delta > Decimal::ZERO {
                                                        state.sl_cumulative_filled += delta;
                                                    }
                                                }
                                            }
                                            let sl_filled_via_cancel = cancel_result
                                                .as_ref()
                                                .ok()
                                                .and_then(|res| res.not_canceled.get(oid.as_str()))
                                                .map(|r| r.to_lowercase().contains("matched"))
                                                .unwrap_or(false);
                                            if sl_filled_via_cancel {
                                                state.sl_cumulative_filled = sl.size.clone();
                                                state.sl_limit_order_id = None;
                                                break;
                                            }
                                            state.sl_limit_order_id = None;
                                            state.sl_limit_order_price = None;
                                            state.sl_last_order_filled = Decimal::ZERO;
                                            state.allowance_cache = None;
                                            let remaining_fd = sl.size.clone() - state.sl_cumulative_filled.clone();
                                            if remaining_fd < DUST_THRESHOLD {
                                                break;
                                            }
                                            let available_fd = get_available_for_sell(
                                                clob.as_ref().as_ref(),
                                                ws_user_ref,
                                                &sl.token_id,
                                                &mut state.allowance_cache,
                                                true,
                                            )
                                            .await;
                                            let size_fd = if available_fd.map_or(false, |a| a > SL_BALANCE_DUST_CLOSE) {
                                                effective_sell_size(
                                                    remaining_fd.clone(),
                                                    available_fd,
                                                    CLOB_DEFAULT_MIN_ORDER_SIZE,
                                                )
                                            } else {
                                                floor_to_decimals(remaining_fd.clone(), SELL_SIZE_DECIMALS)
                                            };
                                            if size_fd < MIN_SELL_SIZE || size_fd < DUST_THRESHOLD {
                                                break;
                                            }
                                            let price_fd = round_to_tick(bid_fd);
                                            let replace_result = clob
                                                .place_sell_order(
                                                    &sl.token_id,
                                                    price_fd,
                                                    size_fd.clone(),
                                                    crate::types::SellOrderTimeInForce::Gtc,
                                                )
                                                .await?;
                                            if replace_result.success {
                                                state.sl_limit_order_id = replace_result.order_id.clone();
                                                state.sl_limit_order_price = Some(price_fd);
                                                state.sl_last_order_filled = Decimal::ZERO;
                                                info!(
                                                    "[IntervalSniper] SL limit replaced @ {} size={} (follow-down, bid dropped)",
                                                    fmt_price(Some(&price_fd)),
                                                    fmt_decimal_2(&size_fd)
                                                );
                                            } else {
                                                state.sl_limit_order_id = None;
                                                break;
                                            }
                                        }
                                    }
                                } else {
                                    // Legacy: no MATCHED or no WS — retry every 200ms until place or interval/price exit.
                                    let available = get_available_for_sell(
                                        clob.as_ref().as_ref(),
                                        ws_user_ref,
                                        &sl.token_id,
                                        &mut state.allowance_cache,
                                        true,
                                    ).await;
                                    if available.map_or(true, |a| a <= SL_BALANCE_DUST_CLOSE) && state.sl_cumulative_filled > Decimal::ZERO {
                                        state.sl_cumulative_filled = sl.size.clone();
                                        trace!(
                                            "[IntervalSniper] SL balance dust/zero (WS filled) — skipping place, will close next tick"
                                        );
                                    } else {
                                        let size = if available.map_or(false, |a| a > SL_BALANCE_DUST_CLOSE) {
                                            effective_sell_size(
                                                remaining.clone(),
                                                available,
                                                CLOB_DEFAULT_MIN_ORDER_SIZE,
                                            )
                                        } else {
                                            floor_to_decimals(remaining.clone(), SELL_SIZE_DECIMALS)
                                        };
                                        if size >= MIN_SELL_SIZE && size >= DUST_THRESHOLD {
                                            let price = round_to_tick(best_bid);
                                            let mut result = clob
                                                .place_sell_order(
                                                    &sl.token_id,
                                                    price,
                                                    size.clone(),
                                                    crate::types::SellOrderTimeInForce::Gtc,
                                                )
                                                .await?;
                                            loop {
                                                if result.success {
                                                    state.sl_limit_order_id = result.order_id.clone();
                                                    state.sl_limit_order_price = Some(price);
                                                    state.sl_last_order_filled = Decimal::ZERO;
                                                    info!(
                                                        "[IntervalSniper] SL limit placed @ {} size={} order_id={:?} (cancel+replace if bid drops)",
                                                        fmt_price(Some(&price)), fmt_decimal_2(&size), result.order_id
                                                    );
                                                    break;
                                                }
                                                if now_unix() >= market.close_time_unix
                                                    || current_5min_slug(config.interval_market) != market.slug
                                                {
                                                    trace!("[IntervalSniper] SL place deferred: interval ended, will retry next tick");
                                                    break;
                                                }
                                                let top_recheck = if let Some(ref ws) = state.ws_book {
                                                    ws.get_top_of_book().await
                                                } else {
                                                    match fetch_top_of_book(&http, &clob_host, &market.token_id_up, &market.token_id_down).await {
                                                        Ok(t) => t,
                                                        Err(_) => top.clone(),
                                                    }
                                                };
                                                let side_recheck = if is_up { &top_recheck.token_id_up } else { &top_recheck.token_id_down };
                                                let recheck_bid = side_recheck.as_ref().and_then(|s| s.best_bid).unwrap_or(Decimal::ZERO);
                                                if recheck_bid > sl.trigger_price + SL_TRIGGER_MARGIN {
                                                    trace!(
                                                        "[IntervalSniper] SL place deferred: bid {} above target ({}), will retry next tick",
                                                        fmt_price(Some(&recheck_bid)), fmt_price(Some(&sl.trigger_price))
                                                    );
                                                    break;
                                                }
                                                if is_position_closed_error(result.error_msg.as_deref()) {
                                                    let ws_bal = if let Some(ws) = ws_user_ref {
                                                        ws.get_balance_for_token(&sl.token_id).await
                                                    } else {
                                                        None
                                                    };
                                                    if ws_bal.map_or(false, |b| b > DUST_THRESHOLD) {
                                                        debug!(
                                                            "[IntervalSniper] SL 'not enough balance' but WS balance={} — allowance lag, retrying",
                                                            fmt_decimal_2(&ws_bal.unwrap_or(Decimal::ZERO))
                                                        );
                                                    } else {
                                                        state.allowance_cache = None;
                                                        let balance_check = clob.get_available_balance(&sl.token_id).await.ok().flatten();
                                                        if balance_check.map_or(true, |a| a <= DUST_THRESHOLD) {
                                                            state.sl_cumulative_filled = sl.size.clone();
                                                            break;
                                                        }
                                                        debug!(
                                                            "[IntervalSniper] SL 'not enough balance' but balance={} — allowance lag, retrying every {}ms",
                                                            fmt_decimal_2(&balance_check.unwrap_or(Decimal::ZERO)),
                                                            TP_SL_BALANCE_RETRY_MS
                                                        );
                                                    }
                                                }
                                                if now_unix() >= market.close_time_unix {
                                                    trace!("[IntervalSniper] SL place deferred: interval ended");
                                                    break;
                                                }
                                                tokio::time::sleep(Duration::from_millis(TP_SL_BALANCE_RETRY_MS)).await;
                                                state.allowance_cache = None;
                                                result = clob
                                                    .place_sell_order(
                                                        &sl.token_id,
                                                        round_to_tick(recheck_bid),
                                                        size.clone(),
                                                        crate::types::SellOrderTimeInForce::Gtc,
                                                    )
                                                    .await?;
                                            }
                                            for _ in 0..SL_FOLLOW_DOWN_MAX_RETRIES {
                                                tokio::time::sleep(Duration::from_millis(SL_FOLLOW_DOWN_MS)).await;
                                                if now_unix() >= market.close_time_unix
                                                    || current_5min_slug(config.interval_market) != market.slug
                                                {
                                                    break;
                                                }
                                                let oid = match &state.sl_limit_order_id {
                                                    Some(id) => id.clone(),
                                                    None => break,
                                                };
                                                if state.sl_cumulative_filled >= sl.size * dec!(0.99) {
                                                    break;
                                                }
                                                let order_price = state.sl_limit_order_price.unwrap_or(Decimal::ZERO);
                                                let top_fd = if let Some(ref ws) = state.ws_book {
                                                    ws.get_top_of_book().await
                                                } else {
                                                    match fetch_top_of_book(&http, &clob_host, &market.token_id_up, &market.token_id_down).await {
                                                        Ok(t) => t,
                                                        Err(_) => break,
                                                    }
                                                };
                                                let side_fd = if is_up { &top_fd.token_id_up } else { &top_fd.token_id_down };
                                                let bid_fd = side_fd.as_ref().and_then(|s| s.best_bid).unwrap_or(Decimal::ZERO);
                                                if bid_fd >= order_price || bid_fd <= Decimal::ZERO {
                                                    break;
                                                }
                                                let cancel_result = clob.cancel_orders_for_token(&sl.token_id).await;
                                                if let Some(ws_user) = ws_user_ref {
                                                    if let Some(final_fill) = ws_user.get_order_filled_size_sell(&oid).await {
                                                        let delta = final_fill - state.sl_last_order_filled;
                                                        if delta > Decimal::ZERO {
                                                            state.sl_cumulative_filled += delta;
                                                        }
                                                    }
                                                }
                                                let sl_filled_via_cancel = cancel_result
                                                    .as_ref()
                                                    .ok()
                                                    .and_then(|res| res.not_canceled.get(oid.as_str()))
                                                    .map(|r| r.to_lowercase().contains("matched"))
                                                    .unwrap_or(false);
                                                if sl_filled_via_cancel {
                                                    state.sl_cumulative_filled = sl.size.clone();
                                                    state.sl_limit_order_id = None;
                                                    break;
                                                }
                                                state.sl_limit_order_id = None;
                                                state.sl_limit_order_price = None;
                                                state.sl_last_order_filled = Decimal::ZERO;
                                                state.allowance_cache = None;
                                                let remaining_fd = sl.size.clone() - state.sl_cumulative_filled.clone();
                                                if remaining_fd < DUST_THRESHOLD {
                                                    break;
                                                }
                                                let available_fd = get_available_for_sell(
                                                    clob.as_ref().as_ref(),
                                                    ws_user_ref,
                                                    &sl.token_id,
                                                    &mut state.allowance_cache,
                                                    true,
                                                )
                                                .await;
                                                let size_fd = if available_fd.map_or(false, |a| a > SL_BALANCE_DUST_CLOSE) {
                                                    effective_sell_size(
                                                        remaining_fd.clone(),
                                                        available_fd,
                                                        CLOB_DEFAULT_MIN_ORDER_SIZE,
                                                    )
                                                } else {
                                                    floor_to_decimals(remaining_fd.clone(), SELL_SIZE_DECIMALS)
                                                };
                                                if size_fd < MIN_SELL_SIZE || size_fd < DUST_THRESHOLD {
                                                    break;
                                                }
                                                let price_fd = round_to_tick(bid_fd);
                                                let replace_result = clob
                                                    .place_sell_order(
                                                        &sl.token_id,
                                                        price_fd,
                                                        size_fd.clone(),
                                                        crate::types::SellOrderTimeInForce::Gtc,
                                                    )
                                                    .await?;
                                                if replace_result.success {
                                                    state.sl_limit_order_id = replace_result.order_id.clone();
                                                    state.sl_limit_order_price = Some(price_fd);
                                                    state.sl_last_order_filled = Decimal::ZERO;
                                                    info!(
                                                        "[IntervalSniper] SL limit replaced @ {} size={} (follow-down, bid dropped)",
                                                        fmt_price(Some(&price_fd)),
                                                        fmt_decimal_2(&size_fd)
                                                    );
                                                } else {
                                                    state.sl_limit_order_id = None;
                                                    break;
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }

                        // When SL closed without a resting order (e.g. WS filled + dust, or place failed with "not enough balance"), run full cleanup.
                        if state.sl_limit_order_id.is_none() && state.sl_cumulative_filled >= sl.size * dec!(0.99) {
                            let exit_price = state.sl_limit_order_price.unwrap_or(best_bid);
                            let total_filled = if state.tp_cumulative_filled > Decimal::ZERO {
                                sl.size.clone() - state.tp_cumulative_filled.clone()
                            } else {
                                state.sl_cumulative_filled.clone()
                            };
                            if let Some(ref buy) = state.last_buy_order {
                                let pnl = (exit_price - buy.price) * total_filled.clone();
                                let roi_pct = ((exit_price / buy.price) - Decimal::ONE) * dec!(100);
                                let held_sec = now_ms_u.saturating_sub(buy.timestamp_ms) / 1000;
                                info!(
                                    "[CLOSED] SL  {} entry={} exit={} size={} pnl={:+.4} ({:+.2}%) held={}s (limit @ best_bid)",
                                    match buy.side { EntrySide::Up => "Up", EntrySide::Down => "Down" },
                                    fmt_decimal_2(&buy.price), fmt_decimal_2(&exit_price),
                                    fmt_decimal_2(&total_filled), pnl, roi_pct, held_sec
                                );
                            }
                            if let Some(ref mut log) = state.session_log {
                                if let Some(ref buy) = state.last_buy_order {
                                    let _ = log.log_position_close(
                                        &market.slug, market.interval_start_unix, market.close_time_unix,
                                        buy.side, buy.price, exit_price, buy.timestamp_ms, now_ms_u,
                                        ExitType::StopLoss, total_filled.clone(), None,
                                        buy.order_id.as_deref(), state.sl_limit_order_id.as_deref(),
                                        state.interval_min_bid_up, state.interval_max_bid_up,
                                        state.interval_min_bid_down, state.interval_max_bid_down,
                                        None,
                                        None,
                                        None,
                                        false,
                                    );
                                }
                            }
                            info!("[IntervalSniper] ✓ SL limit filled @ {} — position closed (re-entry allowed)", fmt_price(Some(&exit_price)));
                            state.stop_loss_placed = true;
                            state.auto_sell_placed = true;
                            state.re_entry_allowed_after_sl = true;
                            state.tp_limit_order_id = None;
                            state.tp_placed_size = None;
                            state.tp_cumulative_filled = Decimal::ZERO;
                            state.tp_last_order_filled = Decimal::ZERO;
                            state.tp_limit_balance_retries = 0;
                            state.sl_limit_order_id = None;
                            state.sl_limit_order_price = None;
                            state.sl_cumulative_filled = Decimal::ZERO;
                            state.sl_last_order_filled = Decimal::ZERO;
                            state.sl_limit_last_rest_check_ms = None;
                            state.pending_auto_sell = None;
                            state.pending_stop_loss = None;
                            state.allowance_cache = None;
                            state.last_buy_order = None;
                            clear_pending_gtc(&mut state);
                            state.balance_reflected_at_ms = None;
                            state.balance_delay_clob_logged = false;
                            state.last_logged_balance_up = None;
                            state.last_logged_balance_down = None;
                            state.total_shares_this_interval = Decimal::ZERO;
                            if let Some(ws) = ws_user_ref {
                                ws.clear_token_state(&sl.token_id).await;
                            }
                        }
                    }
                }
            }
        }

        // Take profit: when target is reached (best_ask at target), place GTC limit at target.
        // If price drops back to entry, cancel the TP limit and wait for target again.
        if state.config.enable_auto_sell || state.config.auto_sell_at_max_price {
            if let Some(tp) = state.pending_auto_sell.clone() {
                if !state.auto_sell_placed {
                    let entry_price = state
                        .pending_stop_loss
                        .as_ref()
                        .map(|s| s.entry_price)
                        .unwrap_or(Decimal::ZERO);
                    let elapsed_sec = (now_ms_u - tp.placed_at_ms) / 1000;
                    if elapsed_sec >= state.config.min_seconds_after_buy_before_auto_sell as u64 {
                        // Use same `top` as main tick (already WS when available) — one book snapshot per tick, no extra latency.
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
                        let _best_ask = side_book
                            .as_ref()
                            .and_then(|s| s.best_ask)
                            .unwrap_or(Decimal::ZERO);
                        let target = round_to_tick(tp.target_price);
                        let tp_activation_price = target - TICK_SIZE; // Only activate TP when price touches TP - 0.01

                        // Reconcile FAK fill from WS: when MATCHED trade events arrive, pending_auto_sell/size may be larger than actual filled;
                        // downsize so TP/SL placement does not fail with "not enough balance".
                        if state.pending_auto_sell.is_some() && state.pending_stop_loss.is_some() {
                            if let (Some(buy), Some(ws_user)) = (
                                state.last_buy_order.as_ref(),
                                state.ws_user.as_ref().map(|a| a.as_ref()),
                            ) {
                                if let Some(ref oid) = buy.order_id {
                                    if let Some((filled, _)) = ws_user.get_order_filled_size_with_type(oid).await {
                                        let cap = filled.min(buy.size.clone());
                                        let tp_size = state.pending_auto_sell.as_ref().unwrap().size.clone();
                                        let sl_size = state.pending_stop_loss.as_ref().unwrap().size.clone();
                                        if cap < tp_size || cap < sl_size {
                                            let new_tp = tp_size.min(cap);
                                            let new_sl = sl_size.min(cap);
                                            state.pending_auto_sell.as_mut().unwrap().size = new_tp.clone();
                                            state.pending_stop_loss.as_mut().unwrap().size = new_sl.clone();
                                            info!(
                                                "[IntervalSniper] FAK fill reconciled from WS: order_id={} actual_filled={} → TP size {}→{}  SL size {}→{}",
                                                oid.chars().take(20).collect::<String>(),
                                                fmt_decimal_2(&cap),
                                                fmt_decimal_2(&tp_size),
                                                fmt_decimal_2(&new_tp),
                                                fmt_decimal_2(&sl_size),
                                                fmt_decimal_2(&new_sl),
                                            );
                                        }
                                    }
                                }
                            }
                        }

                        let mut tp_filled_this_iteration = false;
                        // 1) If we have a TP limit order resting: cancel when price drops below entry OR when in SL zone (best_bid <= SL trigger).
                        //    Same `top` as main tick is used for SL and TP so one consistent book view per tick (HFT).
                        let in_sl_zone = state
                            .pending_stop_loss
                            .as_ref()
                            .map(|sl| best_bid <= sl.trigger_price + SL_TRIGGER_MARGIN)
                            .unwrap_or(false);
                        let should_cancel_tp_below_level = best_bid < entry_price || in_sl_zone;
                        let tp_order_id = state.tp_limit_order_id.clone();
                        if let Some(ref oid) = tp_order_id {
                            if best_bid > Decimal::ZERO && should_cancel_tp_below_level {
                                // HFT: WS first — get partial fill before cancel to track tp_cumulative_filled.
                                if let Some(ws_user) = ws_user_ref {
                                    if let Some((filled, fill_event_type)) = ws_user.get_order_filled_size_sell_with_type(oid).await {
                                        if filled > state.tp_last_order_filled {
                                            let delta = filled.clone() - state.tp_last_order_filled.clone();
                                            state.tp_cumulative_filled += delta.clone();
                                            state.tp_last_order_filled = filled.clone();
                                            if delta >= MIN_SELL_SIZE {
                                                if let Some(ref buy) = state.last_buy_order {
                                                    let pnl = (target - buy.price.clone()) * delta.clone();
                                                    let roi_pct = ((target / buy.price) - Decimal::ONE) * dec!(100);
                                                    let held_sec = now_ms_u.saturating_sub(buy.timestamp_ms) / 1000;
                                                    info!(
                                                        "[IntervalSniper] TP partial fill via WS ({}) (price at entry): +{} @ {} (total TP filled {}/{}), pnl={:+.4} ({:+.2}%) held={}s",
                                                        fill_event_type, fmt_decimal_2(&delta), fmt_price(Some(&target)),
                                                        fmt_decimal_2(&state.tp_cumulative_filled), fmt_decimal_2(&tp.size), pnl, roi_pct, held_sec
                                                    );
                                                    if let Some(ref mut log) = state.session_log {
                                                        let _ = log.log_position_close(
                                                            &market.slug, market.interval_start_unix, market.close_time_unix,
                                                            buy.side, buy.price.clone(), target, buy.timestamp_ms, now_ms_u,
                                                            ExitType::TakeProfit, delta.clone(), Some(delta),
                                                            buy.order_id.as_deref(), Some(oid.as_str()),
                                                            state.interval_min_bid_up, state.interval_max_bid_up,
                                                            state.interval_min_bid_down, state.interval_max_bid_down,
                                                            None,
                                                            None,
                                                            None,
                                                            false,
                                                        );
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                                let cancel_result = clob.cancel_orders_for_token(&tp.token_id).await;
                                state.tp_limit_order_id = None;
                                state.tp_placed_size = None;
                                state.tp_last_order_filled = Decimal::ZERO;
                                state.tp_limit_balance_retries = 0;
                                // If cancel failed because order was already matched, TP filled on exchange.
                                if let Ok(ref res) = cancel_result {
                                    if let Some(reason) = res.not_canceled.get(oid.as_str()) {
                                        let r = reason.to_lowercase();
                                        if r.contains("matched") || r.contains("already canceled") {
                                            tp_filled_this_iteration = true;
                                            if let Some(ref buy) = state.last_buy_order {
                                                let pnl = (target - buy.price) * tp.size.clone();
                                                let roi_pct = ((target / buy.price) - Decimal::ONE) * dec!(100);
                                                let held_sec = now_ms_u.saturating_sub(buy.timestamp_ms) / 1000;
                                                info!(
                                                    "[CLOSED] TP  {} entry={} exit={} size={} pnl={:+.4} ({:+.2}%) held={}s (fill detected via cancel)",
                                                    match buy.side { EntrySide::Up => "Up", EntrySide::Down => "Down" },
                                                    fmt_decimal_2(&buy.price), fmt_decimal_2(&target),
                                                    fmt_decimal_2(&tp.size), pnl, roi_pct, held_sec
                                                );
                                            }
                                            info!(
                                                "[IntervalSniper] ✓ TP limit filled @ {} — position closed (cancel returned already matched)",
                                                fmt_price(Some(&target))
                                            );
                                            if let Some(ref mut log) = state.session_log {
                                                if let Some(ref buy) = state.last_buy_order {
                                                    let _ = log.log_position_close(
                                                        &market.slug,
                                                        market.interval_start_unix,
                                                        market.close_time_unix,
                                                        buy.side,
                                                        buy.price,
                                                        target,
                                                        buy.timestamp_ms,
                                                        now_ms_u,
                                                        ExitType::TakeProfit,
                                                        tp.size.clone(),
                                                        Some(tp.size.clone()),
                                                        buy.order_id.as_deref(),
                                                        Some(oid.as_str()),
                                                        state.interval_min_bid_up,
                                                        state.interval_max_bid_up,
                                                        state.interval_min_bid_down,
                                                        state.interval_max_bid_down,
                                                        None,
                                                        None,
                                                        None,
                                                        false,
                                                    );
                                                }
                                            }
                                            state.auto_sell_placed = true;
                                            state.stop_loss_placed = true;
                                            state.re_entry_allowed_after_sl = false;
                                            state.tp_limit_order_id = None;
                                            state.tp_placed_size = None;
                                            state.tp_cumulative_filled = Decimal::ZERO;
                                            state.tp_last_order_filled = Decimal::ZERO;
                                            state.tp_limit_balance_retries = 0;
                                            state.sl_limit_order_id = None;
                                            state.sl_limit_order_price = None;
                                            state.sl_cumulative_filled = Decimal::ZERO;
                                            state.sl_last_order_filled = Decimal::ZERO;
                                            state.sl_limit_last_rest_check_ms = None;
                                            state.pending_auto_sell = None;
                                            state.pending_stop_loss = None;
                                            state.allowance_cache = None;
                                            state.last_buy_order = None;
                                            clear_pending_gtc(&mut state);
                                            state.balance_reflected_at_ms = None;
                                            state.balance_delay_clob_logged = false;
                                            state.last_logged_balance_up = None;
                                            state.last_logged_balance_down = None;
                                            state.total_shares_this_interval = Decimal::ZERO;
                                        }
                                    }
                                }
                                if !tp_filled_this_iteration {
                                    if in_sl_zone {
                                        trace!(
                                            "[IntervalSniper] TP limit canceled (bid {} in SL zone), waiting for target or SL",
                                            fmt_price(Some(&best_bid))
                                        );
                                    } else {
                                        trace!(
                                            "[IntervalSniper] TP limit canceled (price below entry {}), waiting for target or SL",
                                            fmt_price(Some(&entry_price))
                                        );
                                    }
                                }
                            }
                        }

                        // PRIORIDAD 1: WS User (instant, 0ms latency). Verificar con REST antes de cerrar para evitar falsos positivos.
                        // Log each TP partial fill via WS; when cumulative reaches 99%+, same close logic as single fill.
                        let mut tp_detected_by_ws = false;

                        if let Some(ws_user) = ws_user_ref {
                            if let Some(ref oid) = state.tp_limit_order_id {
                                let tp_size_for_check = state.tp_placed_size.unwrap_or(tp.size.clone());
                                match ws_user.get_order_filled_size_sell_with_type(oid).await {
                                    Some((filled, fill_event_type)) => {
                                        if filled >= tp_size_for_check * dec!(0.99) {
                                            // Full fill or cumulative partials reached 100% — same close logic as single fill.
                                            tp_detected_by_ws = true;
                                            let close_size = filled.clone();

                                            info!("[IntervalSniper] ✓ TP fill via WS ({}): {}/{} filled",
                                                fill_event_type, fmt_decimal_2(&close_size), fmt_decimal_2(&tp_size_for_check));

                                            if let Some(ref buy) = state.last_buy_order {
                                                let exit_price = target;
                                                let pnl = (exit_price - buy.price) * close_size.clone();
                                                let roi_pct = ((exit_price / buy.price) - Decimal::ONE) * dec!(100);
                                                let held_sec = now_ms_u.saturating_sub(buy.timestamp_ms) / 1000;

                                                info!(
                                                    "[CLOSED] TP  {} entry={} exit={} size={} pnl={:+.4} ({:+.2}%) held={}s",
                                                    match buy.side { EntrySide::Up => "Up", EntrySide::Down => "Down" },
                                                    fmt_decimal_2(&buy.price), fmt_decimal_2(&exit_price),
                                                    fmt_decimal_2(&close_size), pnl, roi_pct, held_sec
                                                );
                                            }

                                            if let Some(ref mut log) = state.session_log {
                                                if let Some(ref buy) = state.last_buy_order {
                                                    let _ = log.log_position_close(
                                                        &market.slug, market.interval_start_unix, market.close_time_unix,
                                                        buy.side, buy.price.clone(), target, buy.timestamp_ms, now_ms_u,
                                                        ExitType::TakeProfit, close_size.clone(), state.tp_placed_size.clone(),
                                                        buy.order_id.as_deref(), state.tp_limit_order_id.as_deref(),
                                                        state.interval_min_bid_up, state.interval_max_bid_up,
                                                        state.interval_min_bid_down, state.interval_max_bid_down,
                                                        None,
                                                        None,
                                                        None,
                                                        false,
                                                    );
                                                }
                                            }

                                            info!("[IntervalSniper] ✓ TP limit filled @ {} — position closed", fmt_price(Some(&target)));

                                            tp_filled_this_iteration = true;
                                            state.auto_sell_placed = true;
                                            state.stop_loss_placed = true;
                                            state.re_entry_allowed_after_sl = false;
                                            state.tp_limit_order_id = None;
                                            state.tp_placed_size = None;
                                            state.tp_cumulative_filled = Decimal::ZERO;
                                            state.tp_last_order_filled = Decimal::ZERO;
                                            state.tp_limit_balance_retries = 0;
                                            state.sl_limit_order_id = None;
                                            state.sl_limit_order_price = None;
                                            state.sl_cumulative_filled = Decimal::ZERO;
                                            state.sl_last_order_filled = Decimal::ZERO;
                                            state.sl_limit_last_rest_check_ms = None;
                                            state.pending_auto_sell = None;
                                            state.pending_stop_loss = None;
                                            state.allowance_cache = None;
                                            state.last_buy_order = None;
                                            clear_pending_gtc(&mut state);
                                            state.balance_reflected_at_ms = None;
                                            state.balance_delay_clob_logged = false;
                                            state.last_logged_balance_up = None;
                                            state.last_logged_balance_down = None;
                                            state.total_shares_this_interval = Decimal::ZERO;
                                        } else if filled > state.tp_last_order_filled {
                                            // Partial fill — update state and log each partial (same as when price drops below entry).
                                            let delta = filled.clone() - state.tp_last_order_filled.clone();
                                            state.tp_cumulative_filled += delta.clone();
                                            state.tp_last_order_filled = filled.clone();
                                            if delta >= MIN_SELL_SIZE {
                                                if let Some(ref buy) = state.last_buy_order {
                                                    let pnl = (target.clone() - buy.price.clone()) * delta.clone();
                                                    let roi_pct = ((target.clone() / buy.price) - Decimal::ONE) * dec!(100);
                                                    let held_sec = now_ms_u.saturating_sub(buy.timestamp_ms) / 1000;
                                                    info!(
                                                        "[IntervalSniper] TP partial fill via WS ({}): +{} @ {} (total TP filled {}/{}), pnl={:+.4} ({:+.2}%) held={}s",
                                                        fill_event_type, fmt_decimal_2(&delta), fmt_price(Some(&target)),
                                                        fmt_decimal_2(&state.tp_cumulative_filled), fmt_decimal_2(&tp_size_for_check), pnl, roi_pct, held_sec
                                                    );
                                                    if let Some(ref mut log) = state.session_log {
                                                        let _ = log.log_position_close(
                                                            &market.slug, market.interval_start_unix, market.close_time_unix,
                                                            buy.side, buy.price.clone(), target.clone(), buy.timestamp_ms, now_ms_u,
                                                            ExitType::TakeProfit, delta.clone(), Some(delta),
                                                            buy.order_id.as_deref(), Some(oid.as_str()),
                                                            state.interval_min_bid_up, state.interval_max_bid_up,
                                                            state.interval_min_bid_down, state.interval_max_bid_down,
                                                            None,
                                                            None,
                                                            None,
                                                            false,
                                                        );
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    _ => {}
                                }
                            }
                        }

                        // PRIORIDAD 2: REST polling (solo si WS no disponible o no detectó fill)
                        if !tp_detected_by_ws && state.tp_limit_order_id.is_some() {
                            let should_check_rest = ws_user_ref.is_none()
                                || state.tp_limit_last_rest_check_ms
                                    .map(|last| now_ms_u.saturating_sub(last) >= 5000)
                                    .unwrap_or(true);

                            if should_check_rest {
                                state.tp_limit_last_rest_check_ms = Some(now_ms_u);

                                if ws_user_ref.is_none() {
                                    debug!("[IntervalSniper] TP: No WS user available, checking REST");
                                }

                                if let Some(ref oid) = state.tp_limit_order_id {
                                    match clob.get_order(oid).await {
                                        Ok(order_info) => {
                                            let status = order_info.get("status")
                                                .and_then(|v| v.as_str())
                                                .unwrap_or("");

                                            if status.contains("MATCHED") || status.eq_ignore_ascii_case("FILLED") {
                                                let rest_filled = order_info
                                                    .get("size_matched")
                                                    .and_then(|v| v.as_str())
                                                    .and_then(|s| Decimal::from_str(s).ok())
                                                    .unwrap_or_else(|| tp.size.clone());
                                                info!("[IntervalSniper] ✓ TP detected via REST fallback (WS unavailable/missed)");

                                                if let Some(ref buy) = state.last_buy_order {
                                                    let exit_price = target;
                                                    let pnl = (exit_price - buy.price) * rest_filled.clone();
                                                    let roi_pct = ((exit_price / buy.price) - Decimal::ONE) * dec!(100);
                                                    let held_sec = now_ms_u.saturating_sub(buy.timestamp_ms) / 1000;

                                                    info!(
                                                        "[CLOSED] TP  {} entry={} exit={} size={} pnl={:+.4} ({:+.2}%) held={}s",
                                                        match buy.side { EntrySide::Up => "Up", EntrySide::Down => "Down" },
                                                        fmt_decimal_2(&buy.price), fmt_decimal_2(&exit_price),
                                                        fmt_decimal_2(&rest_filled), pnl, roi_pct, held_sec
                                                    );
                                                }

                                                if let Some(ref mut log) = state.session_log {
                                                    if let Some(ref buy) = state.last_buy_order {
                                                        let _ = log.log_position_close(
                                                            &market.slug, market.interval_start_unix, market.close_time_unix,
                                                            buy.side, buy.price.clone(), target, buy.timestamp_ms, now_ms_u,
                                                            ExitType::TakeProfit, rest_filled.clone(), state.tp_placed_size.clone(),
                                                            buy.order_id.as_deref(), state.tp_limit_order_id.as_deref(),
                                                            state.interval_min_bid_up, state.interval_max_bid_up,
                                                            state.interval_min_bid_down, state.interval_max_bid_down,
                                                            None,
                                                            None,
                                                            None,
                                                            false,
                                                        );
                                                    }
                                                }

                                                info!("[IntervalSniper] ✓ TP limit filled @ {} (REST) — position closed", fmt_price(Some(&target)));

                                                tp_filled_this_iteration = true;
                                                state.auto_sell_placed = true;
                                                state.stop_loss_placed = true;
                                                state.re_entry_allowed_after_sl = false;
                                                state.tp_limit_order_id = None;
                                                state.tp_placed_size = None;
                                                state.tp_cumulative_filled = Decimal::ZERO;
                                                state.tp_last_order_filled = Decimal::ZERO;
                                                state.tp_limit_balance_retries = 0;
                                                state.sl_limit_order_id = None;
                                                state.sl_limit_order_price = None;
                                                state.sl_cumulative_filled = Decimal::ZERO;
                                                state.sl_last_order_filled = Decimal::ZERO;
                                                state.sl_limit_last_rest_check_ms = None;
                                                state.pending_auto_sell = None;
                                                state.pending_stop_loss = None;
                                                state.allowance_cache = None;
                                                state.last_buy_order = None;
                                                clear_pending_gtc(&mut state);
                                                state.balance_reflected_at_ms = None;
                                                state.balance_delay_clob_logged = false;
                                                state.last_logged_balance_up = None;
                                                state.last_logged_balance_down = None;
                                                state.total_shares_this_interval = Decimal::ZERO;
                                            }
                                        }
                                        Err(_) => {
                                            // Silenciar errores REST, no son críticos
                                        }
                                    }
                                }
                            }
                        }

                        // TP fill is only confirmed by: (1) WS fill, (2) REST get_order filled, or (3) cancel returned "matched".
                        // We do NOT infer TP filled from "available balance is dust" while a TP order is resting: the API can
                        // report available=0 because balance is locked in the open order, causing a false positive "TP filled".

                        // 2) Place GTC limit at target only when price has touched TP - 0.01; cancel if it drops below entry.
                        // Skip if we just detected TP fill this iteration (position already closed; avoid "not enough balance").
                        // Skip if we have a resting SL limit order: the CLOB locks balance for that order, so placing TP
                        // would get "not enough balance" (TP and SL are mutually exclusive for the same position).
                        if !tp_filled_this_iteration && state.tp_limit_order_id.is_none() && state.sl_limit_order_id.is_none() {
                            let target_reached = best_bid >= tp_activation_price;
                            if target_reached {
                                let position_remaining = tp.size.clone() - state.tp_cumulative_filled.clone();
                                let position_size_real = position_remaining.clone();
                                // With WS: only place TP after MATCHED (fill on exchange); update_balance_allowance + backoff handles server cache.
                                let confirmed_ok = match ws_user_ref {
                                    Some(ws) => ws
                                        .get_confirmed_buy_size(&tp.token_id)
                                        .await
                                        .map(|c| c >= position_size_real.clone() * dec!(0.99))
                                        .unwrap_or(false),
                                    None => true,
                                };
                                if !confirmed_ok {
                                    tracing::debug!(
                                        "[IntervalSniper] TP waiting for MATCHED (fill on exchange) for token before placing"
                                    );
                                } else {
                                // Log reactivation when we're retrying after balance/retry was cancelled (e.g. price dropped to entry then came back)
                                if state.tp_limit_balance_retries > 0 {
                                    info!(
                                        "[IntervalSniper] TP reactivation: price back at target (bid {} >= {}), retrying placement",
                                        fmt_price(Some(&best_bid)),
                                        fmt_price(Some(&tp_activation_price))
                                    );
                                }
                                let position_remaining = tp.size.clone() - state.tp_cumulative_filled.clone();
                                let position_size_real = position_remaining;
                                let available =
                                    get_available_for_sell(clob.as_ref().as_ref(), ws_user_ref, &tp.token_id, &mut state.allowance_cache, false).await;
                                let size = effective_sell_size(
                                    position_size_real,
                                    available.clone(),
                                    CLOB_DEFAULT_MIN_ORDER_SIZE,
                                );
                                debug!(
                                    "[IntervalSniper] TP limit calculation: position_size={}, available={:?}, effective_size={}, MIN_SELL_SIZE={}, DUST_THRESHOLD={}",
                                    position_size_real, available, size, MIN_SELL_SIZE, DUST_THRESHOLD
                                );
                                // Remaining is below API minimum order size (e.g. 0.01 < 5): unsellable dust.
                                // Distinguish: dust after partial TP fill vs dust after SL (SL sold most, left unsellable remainder).
                                // On re-entry after SL, we must not treat the new position as "dust after SL": require sl_cumulative_filled > 0
                                // so only the remainder from an SL that already filled counts. Also skip closing when position_remaining >= MIN
                                // but available is stale (e.g. WS balance not yet updated after new fill).
                                // BUG FIX: when size == 0 (e.g. REST balance still 0 after WS fill), we must still use the stale_available
                                // path — otherwise we never place TP until REST catches up, which can be too late or never.
                                let dust_after_sl = state.pending_stop_loss.is_some()
                                    && state.sl_cumulative_filled > Decimal::ZERO
                                    && state.tp_cumulative_filled.is_zero();
                                let stale_available = position_size_real >= CLOB_DEFAULT_MIN_ORDER_SIZE;
                                if (size > Decimal::ZERO && size < CLOB_DEFAULT_MIN_ORDER_SIZE) || (size.is_zero() && stale_available && !dust_after_sl) {
                                    if stale_available && !dust_after_sl {
                                        // With MATCHED (ws_user): invalidate server cache, backoff get balance, place once — no polling loop.
                                        let expected_size = floor_to_decimals(
                                            position_size_real.min(state.config.size_shares),
                                            SELL_SIZE_DECIMALS,
                                        );
                                        if ws_user_ref.is_some() {
                                            // TP: retry every 200ms until place_sell_order succeeds or interval ends or price exits TP zone (e.g. into SL zone).
                                            let params = BalanceAllowanceParams {
                                                asset_type: AssetType::Conditional,
                                                token_id: Some(tp.token_id.clone()),
                                            };
                                            let mut _tp_placed = false;
                                            loop {
                                                if now_unix() >= market.close_time_unix {
                                                    trace!("[IntervalSniper] TP place deferred: interval ended, will retry next tick");
                                                    break;
                                                }
                                                let top_current = if let Some(ref ws) = state.ws_book {
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
                                                        Err(_) => top.clone(),
                                                    }
                                                };
                                                let current_best_bid = if is_up {
                                                    top_current
                                                        .token_id_up
                                                        .as_ref()
                                                        .and_then(|s| s.best_bid)
                                                        .unwrap_or(Decimal::ZERO)
                                                } else {
                                                    top_current
                                                        .token_id_down
                                                        .as_ref()
                                                        .and_then(|s| s.best_bid)
                                                        .unwrap_or(Decimal::ZERO)
                                                };
                                                let sl_zone_upper = state
                                                    .pending_stop_loss
                                                    .as_ref()
                                                    .map(|s| s.trigger_price + SL_TRIGGER_MARGIN)
                                                    .unwrap_or(entry_price);
                                                if current_best_bid <= sl_zone_upper {
                                                    info!(
                                                        "[IntervalSniper] TP limit retries cancelled: price in SL zone (bid {} <= {}), SL will take over — will retry TP when price returns to target (next tick)",
                                                        fmt_price(Some(&current_best_bid)),
                                                        fmt_price(Some(&sl_zone_upper))
                                                    );
                                                    state.allowance_cache = None;
                                                    break;
                                                }
                                                if current_best_bid < tp_activation_price {
                                                    trace!(
                                                        "[IntervalSniper] TP place deferred: bid {} below target ({}), will retry next tick",
                                                        fmt_price(Some(&current_best_bid)),
                                                        fmt_price(Some(&tp_activation_price))
                                                    );
                                                    break;
                                                }
                                                state.allowance_cache = None;
                                                let _ = clob.as_ref().update_balance_allowance(&params).await;
                                                let bal = clob.get_available_balance(&tp.token_id).await.ok().flatten();
                                                if bal.as_ref().map(|b| *b >= expected_size.clone() * dec!(0.99)).unwrap_or(false) {
                                                    let size_to_place = bal
                                                        .map(|b| floor_to_decimals(expected_size.clone().min(b), SELL_SIZE_DECIMALS))
                                                        .unwrap_or_else(|| expected_size.clone());
                                                    if size_to_place >= CLOB_DEFAULT_MIN_ORDER_SIZE {
                                                        let price = target.clone();
                                                        let result = clob
                                                            .place_sell_order(
                                                                &tp.token_id,
                                                                price.clone(),
                                                                size_to_place.clone(),
                                                                crate::types::SellOrderTimeInForce::Gtc,
                                                            )
                                                            .await?;
                                                        if let Some(ref mut log) = state.session_log {
                                                            let _ = log.log_order_submitted(
                                                                &market.slug,
                                                                market.interval_start_unix,
                                                                market.close_time_unix,
                                                                now_ms(),
                                                                &tp.token_id,
                                                                "SELL",
                                                                "GTC",
                                                                price.clone(),
                                                                size_to_place.clone(),
                                                                result.order_id.as_deref(),
                                                                result.http_status,
                                                                result.success,
                                                                result.error_msg.as_deref(),
                                                            );
                                                        }
                                                        if result.success {
                                                            state.tp_limit_order_id = result.order_id.clone();
                                                            state.tp_placed_size = Some(size_to_place);
                                                            state.tp_limit_balance_retries = 0;
                                                            info!(
                                                                "[IntervalSniper] TP limit placed @ {} size={} order_id={:?} (cancel if price drops to entry {})",
                                                                fmt_price(Some(&price)),
                                                                fmt_decimal_2(&size_to_place),
                                                                result.order_id,
                                                                fmt_price(Some(&entry_price))
                                                            );
                                                            _tp_placed = true;
                                                            break;
                                                        }
                                                        if is_position_closed_error(result.error_msg.as_deref()) {
                                                            state.allowance_cache = None;
                                                            state.tp_limit_balance_retries += 1;
                                                        }
                                                    }
                                                }
                                                tokio::time::sleep(Duration::from_millis(TP_SL_BALANCE_RETRY_MS)).await;
                                            }
                                        } else {
                                        // No WS: legacy polling loop until REST balance reflects fill or interval ends.
                                        let mut tp_placed = false;
                                        while !tp_placed {
                                            let interval_ended = now_unix() >= market.close_time_unix;
                                            if interval_ended {
                                                // No "next tick" for this market: next iteration is need_new_market and we skip TP block.
                                                // So do one final placement attempt instead of deferring.
                                                state.allowance_cache = None;
                                                let rest_balance = clob.get_available_balance(&tp.token_id).await.ok().flatten();
                                                let size_to_place = if let Some(av) = rest_balance {
                                                    if av >= expected_size * dec!(0.99) {
                                                        floor_to_decimals(expected_size.min(av), SELL_SIZE_DECIMALS)
                                                    } else {
                                                        expected_size.clone()
                                                    }
                                                } else {
                                                    expected_size.clone()
                                                };
                                                if size_to_place >= CLOB_DEFAULT_MIN_ORDER_SIZE {
                                                    let price = target;
                                                    let result = clob
                                                        .place_sell_order(
                                                            &tp.token_id,
                                                            price,
                                                            size_to_place.clone(),
                                                            crate::types::SellOrderTimeInForce::Gtc,
                                                        )
                                                        .await?;
                                                    if let Some(ref mut log) = state.session_log {
                                                        let _ = log.log_order_submitted(
                                                            &market.slug,
                                                            market.interval_start_unix,
                                                            market.close_time_unix,
                                                            now_ms(),
                                                            &tp.token_id,
                                                            "SELL",
                                                            "GTC",
                                                            price,
                                                            size_to_place.clone(),
                                                            result.order_id.as_deref(),
                                                            result.http_status,
                                                            result.success,
                                                            result.error_msg.as_deref(),
                                                        );
                                                    }
                                                    if result.success {
                                                        state.tp_limit_order_id = result.order_id.clone();
                                                        state.tp_placed_size = Some(size_to_place.clone());
                                                        state.tp_limit_balance_retries = 0;
                                                        info!(
                                                            "[IntervalSniper] TP limit placed @ {} size={} order_id={:?} (final attempt at interval end)",
                                                            fmt_price(Some(&price)),
                                                            fmt_decimal_2(&size_to_place),
                                                            result.order_id
                                                        );
                                                    }
                                                }
                                                break;
                                            }
                                            // Cancel retries only when price has dropped into SL zone (so SL logic takes over).
                                            // Do NOT cancel just because price touched entry — keep retrying so balance can update and we place TP
                                            // when price is at/above target (avoids missing TP when price briefly dips to entry then recovers).
                                            let top_current = if let Some(ref ws) = state.ws_book {
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
                                                    Err(_) => top.clone(),
                                                }
                                            };
                                            let current_best_bid = if is_up {
                                                top_current
                                                    .token_id_up
                                                    .as_ref()
                                                    .and_then(|s| s.best_bid)
                                                    .unwrap_or(Decimal::ZERO)
                                            } else {
                                                top_current
                                                    .token_id_down
                                                    .as_ref()
                                                    .and_then(|s| s.best_bid)
                                                    .unwrap_or(Decimal::ZERO)
                                            };
                                            let sl_zone_upper = state
                                                .pending_stop_loss
                                                .as_ref()
                                                .map(|sl| sl.trigger_price + SL_TRIGGER_MARGIN)
                                                .unwrap_or(entry_price);
                                            if current_best_bid <= sl_zone_upper {
                                                info!(
                                                    "[IntervalSniper] TP limit retries cancelled: price in SL zone (bid {} <= {}), SL will take over — will retry TP when price returns to target (next tick)",
                                                    fmt_price(Some(&current_best_bid)),
                                                    fmt_price(Some(&sl_zone_upper))
                                                );
                                                state.allowance_cache = None;
                                                break;
                                            }
                                            state.allowance_cache = None;
                                            let rest_balance = clob.get_available_balance(&tp.token_id).await.ok().flatten();
                                            let size_to_place = if let Some(av) = rest_balance {
                                                if av >= expected_size * dec!(0.99) {
                                                    floor_to_decimals(expected_size.min(av), SELL_SIZE_DECIMALS)
                                                } else {
                                                    expected_size.clone()
                                                }
                                            } else {
                                                expected_size.clone()
                                            };
                                            if size_to_place < CLOB_DEFAULT_MIN_ORDER_SIZE {
                                                tokio::time::sleep(Duration::from_millis(TP_SL_BALANCE_RETRY_MS)).await;
                                                continue;
                                            }
                                            let price = target;
                                            let result = clob
                                                .place_sell_order(
                                                    &tp.token_id,
                                                    price,
                                                    size_to_place.clone(),
                                                    crate::types::SellOrderTimeInForce::Gtc,
                                                )
                                                .await?;
                                            if let Some(ref mut log) = state.session_log {
                                                let _ = log.log_order_submitted(
                                                    &market.slug,
                                                    market.interval_start_unix,
                                                    market.close_time_unix,
                                                    now_ms(),
                                                    &tp.token_id,
                                                    "SELL",
                                                    "GTC",
                                                    price,
                                                    size_to_place.clone(),
                                                    result.order_id.as_deref(),
                                                    result.http_status,
                                                    result.success,
                                                    result.error_msg.as_deref(),
                                                );
                                            }
                                            if result.success {
                                                state.tp_limit_order_id = result.order_id.clone();
                                                state.tp_placed_size = Some(size_to_place.clone());
                                                state.tp_limit_balance_retries = 0;
                                                info!(
                                                    "[IntervalSniper] TP limit placed @ {} size={} order_id={:?} (cancel if price drops to entry {})",
                                                    fmt_price(Some(&price)),
                                                    fmt_decimal_2(&size_to_place),
                                                    result.order_id,
                                                    fmt_price(Some(&entry_price))
                                                );
                                                tp_placed = true;
                                            } else if is_position_closed_error(result.error_msg.as_deref()) {
                                                state.tp_limit_balance_retries += 1;
                                                if state.tp_limit_balance_retries == 1 {
                                                    warn!(
                                                        "[IntervalSniper] TP limit balance/allowance error — retrying every {}ms until REST balance reflects fill or interval ends",
                                                        TP_SL_BALANCE_RETRY_MS
                                                    );
                                                    state.allowance_cache = None;
                                                }
                                                tokio::time::sleep(Duration::from_millis(TP_SL_BALANCE_RETRY_MS)).await;
                                            } else if is_invalid_amounts_error(result.error_msg.as_deref()) {
                                                state.tp_limit_balance_retries += 1;
                                                state.allowance_cache = None;
                                                tokio::time::sleep(Duration::from_millis(TP_SL_BALANCE_RETRY_MS)).await;
                                            } else {
                                                break;
                                            }
                                        }
                                        } // end legacy polling loop (no WS)
                                    } else if dust_after_sl {
                                        // Dust left after SL filled (e.g. SL size was 5.99, left 0.01). Do not report as TP.
                                        let exit_price = state.sl_limit_order_price.unwrap_or(target);
                                        let total_filled = tp.size.clone() - available.unwrap_or(Decimal::ZERO);
                                        info!(
                                            "[IntervalSniper] TP remaining {} below API minimum {} — dust after SL fill, closing position",
                                            fmt_decimal_2(&size), fmt_decimal_2(&CLOB_DEFAULT_MIN_ORDER_SIZE)
                                        );
                                        if let Some(ref buy) = state.last_buy_order {
                                            let pnl = (exit_price - buy.price) * total_filled.clone();
                                            let roi_pct = ((exit_price / buy.price) - Decimal::ONE) * dec!(100);
                                            let held_sec = now_ms_u.saturating_sub(buy.timestamp_ms) / 1000;
                                            info!(
                                                "[CLOSED] SL  {} entry={} exit={} size={} pnl={:+.4} ({:+.2}%) held={}s (dust remainder)",
                                                match buy.side { EntrySide::Up => "Up", EntrySide::Down => "Down" },
                                                fmt_decimal_2(&buy.price), fmt_decimal_2(&exit_price),
                                                fmt_decimal_2(&total_filled), pnl, roi_pct, held_sec
                                            );
                                        }
                                        if let Some(ref mut log) = state.session_log {
                                            if let Some(ref buy) = state.last_buy_order {
                                                let _ = log.log_position_close(
                                                    &market.slug, market.interval_start_unix, market.close_time_unix,
                                                    buy.side, buy.price, exit_price,
                                                    buy.timestamp_ms, now_ms_u,
                                                    ExitType::StopLoss, total_filled.clone(), None,
                                                    buy.order_id.as_deref(), state.sl_limit_order_id.as_deref(),
                                                    state.interval_min_bid_up, state.interval_max_bid_up,
                                        state.interval_min_bid_down, state.interval_max_bid_down,
                                        None,
                                        None,
                                        None,
                                        false,
                                                );
                                            }
                                        }
                                        state.re_entry_allowed_after_sl = true;
                                    } else {
                                        // Real dust after partial TP fill: report as TP with filled amount.
                                        info!(
                                            "[IntervalSniper] TP remaining {} below API minimum {} — dust after partial fill, closing position",
                                            fmt_decimal_2(&size), fmt_decimal_2(&CLOB_DEFAULT_MIN_ORDER_SIZE)
                                        );
                                        if let Some(ref buy) = state.last_buy_order {
                                            let exit_price = target;
                                            let filled = state.tp_cumulative_filled.clone();
                                            let pnl = (exit_price - buy.price) * filled.clone();
                                            let roi_pct = ((exit_price / buy.price) - Decimal::ONE) * dec!(100);
                                            let held_sec = now_ms_u.saturating_sub(buy.timestamp_ms) / 1000;
                                            info!(
                                                "[CLOSED] TP  {} entry={} exit={} size={} pnl={:+.4} ({:+.2}%) held={}s (partial fill, dust remainder)",
                                                match buy.side { EntrySide::Up => "Up", EntrySide::Down => "Down" },
                                                fmt_decimal_2(&buy.price), fmt_decimal_2(&exit_price),
                                                fmt_decimal_2(&filled), pnl, roi_pct, held_sec
                                            );
                                        }
                                        state.re_entry_allowed_after_sl = false;
                                    }
                                    state.auto_sell_placed = true;
                                    state.stop_loss_placed = true;
                                    state.tp_limit_order_id = None;
                                    state.tp_placed_size = None;
                                    state.tp_cumulative_filled = Decimal::ZERO;
                                    state.tp_last_order_filled = Decimal::ZERO;
                                    state.tp_limit_balance_retries = 0;
                                    state.sl_limit_order_id = None;
                                    state.sl_limit_order_price = None;
                                    state.sl_cumulative_filled = Decimal::ZERO;
                                    state.sl_last_order_filled = Decimal::ZERO;
                                    state.sl_limit_last_rest_check_ms = None;
                                    state.pending_auto_sell = None;
                                    state.pending_stop_loss = None;
                                    state.allowance_cache = None;
                                    state.last_buy_order = None;
                                    clear_pending_gtc(&mut state);
                                    state.balance_reflected_at_ms = None;
                                    state.balance_delay_clob_logged = false;
                                    state.last_logged_balance_up = None;
                                    state.last_logged_balance_down = None;
                                    state.total_shares_this_interval = Decimal::ZERO;
                                } else if size >= MIN_SELL_SIZE && size >= DUST_THRESHOLD {
                                    let price = target;
                                        let result = clob
                                            .place_sell_order(
                                            &tp.token_id,
                                            price,
                                            size.clone(),
                                            crate::types::SellOrderTimeInForce::Gtc,
                                        )
                                        .await?;
                                    if let Some(ref mut log) = state.session_log {
                                        let _ = log.log_order_submitted(
                                            &market.slug,
                                            market.interval_start_unix,
                                            market.close_time_unix,
                                            now_ms_u,
                                            &tp.token_id,
                                            "SELL",
                                            "GTC",
                                            price,
                                            size.clone(),
                                            result.order_id.as_deref(),
                                            result.http_status,
                                            result.success,
                                            result.error_msg.as_deref(),
                                        );
                                    }
                                    if result.success {
                                        state.tp_limit_order_id = result.order_id.clone();
                                        state.tp_placed_size = Some(size.clone());
                                        state.tp_limit_balance_retries = 0;
                                        info!(
                                            "[IntervalSniper] TP limit placed @ {} size={} order_id={:?} (cancel if price drops to entry {})",
                                            fmt_price(Some(&price)),
                                            fmt_decimal_2(&size),
                                            result.order_id,
                                            fmt_price(Some(&entry_price))
                                        );
                                    } else if is_position_closed_error(result.error_msg.as_deref()) {
                                        state.tp_limit_balance_retries += 1;
                                        if state.tp_limit_balance_retries == 1 {
                                            warn!(
                                                "[IntervalSniper] TP limit balance/allowance error (retry {}), will retry with actual balance next tick (do not cancel — would cancel resting GTC entry)",
                                                state.tp_limit_balance_retries
                                            );
                                            state.allowance_cache = None;
                                            // Log raw balance-allowance JSON so we know if it's balance=0 or allowance=0.
                                            match clob.get_balance_allowance(&tp.token_id).await {
                                                Ok(raw) => {
                                                    let hint = format_balance_allowance_hint(&raw);
                                                    warn!(
                                                        "[IntervalSniper] balance-allowance for TP token: {}{}",
                                                        raw.chars().take(300).collect::<String>(),
                                                        hint
                                                    );
                                                }
                                                Err(e) => warn!(
                                                    "[IntervalSniper] could not fetch balance-allowance: {}",
                                                    e
                                                ),
                                            }
                                        } else if state.tp_limit_balance_retries >= 10 {
                                            warn!(
                                                "[IntervalSniper] TP limit failed {} times with balance/allowance error — attempting FOK market sell",
                                                state.tp_limit_balance_retries
                                            );
                                            let fok_price = round_to_tick(best_bid);
                                            let fok_result = clob
                                                .place_sell_order(
                                                    &tp.token_id,
                                                    fok_price,
                                                    size.clone(),
                                                    crate::types::SellOrderTimeInForce::Fok,
                                                )
                                                .await?;
                                            if fok_result.success {
                                                if let Some(ref buy) = state.last_buy_order {
                                                    let pnl = (fok_price - buy.price) * size.clone();
                                                    let roi_pct = ((fok_price / buy.price) - Decimal::ONE) * dec!(100);
                                                    let held_sec = now_ms_u.saturating_sub(buy.timestamp_ms) / 1000;
                                                    info!(
                                                        "[CLOSED] TP  {} entry={} exit={} size={} pnl={:+.4} ({:+.2}%) held={}s",
                                                        match buy.side { EntrySide::Up => "Up", EntrySide::Down => "Down" },
                                                        fmt_decimal_2(&buy.price), fmt_decimal_2(&fok_price),
                                                        fmt_decimal_2(&size), pnl, roi_pct, held_sec
                                                    );
                                                }
                                                info!(
                                                    "[IntervalSniper] ✓ TP emergency FOK sell filled @ {} — position closed",
                                                    fmt_price(Some(&fok_price))
                                                );
                                                if let Some(ref mut log) = state.session_log {
                                                    if let Some(ref buy) = state.last_buy_order {
                                                        let _ = log.log_position_close(
                                                            &market.slug,
                                                            market.interval_start_unix,
                                                            market.close_time_unix,
                                                            buy.side,
                                                            buy.price,
                                                            fok_price,
                                                            buy.timestamp_ms,
                                                            now_ms_u,
                                                            ExitType::TakeProfit,
                                                            size.clone(),
                                                            Some(tp.size.clone()),
                                                            buy.order_id.as_deref(),
                                                            fok_result.order_id.as_deref(),
                                                            state.interval_min_bid_up,
                                                            state.interval_max_bid_up,
                                                            state.interval_min_bid_down,
                                                            state.interval_max_bid_down,
                                                            None,
                                                            None,
                                                            None,
                                                            false,
                                                        );
                                                    }
                                                }
                                                state.auto_sell_placed = true;
                                                state.stop_loss_placed = true;
                                                state.re_entry_allowed_after_sl = false;
                                                state.tp_limit_order_id = None;
                                                state.tp_placed_size = None;
                                                state.tp_cumulative_filled = Decimal::ZERO;
                                                state.tp_last_order_filled = Decimal::ZERO;
                                                state.tp_limit_balance_retries = 0;
                                                state.sl_limit_order_id = None;
                                                state.sl_limit_order_price = None;
                                                state.sl_cumulative_filled = Decimal::ZERO;
                                                state.sl_last_order_filled = Decimal::ZERO;
                                                state.sl_limit_last_rest_check_ms = None;
                                                state.pending_auto_sell = None;
                                                state.pending_stop_loss = None;
                                                state.allowance_cache = None;
                                                state.last_buy_order = None;
                                                clear_pending_gtc(&mut state);
                                                state.balance_reflected_at_ms = None;
                                                state.balance_delay_clob_logged = false;
                                                state.last_logged_balance_up = None;
                                                state.last_logged_balance_down = None;
                                                state.total_shares_this_interval = Decimal::ZERO;
                                            } else {
                                                warn!(
                                                    "[IntervalSniper] TP emergency FOK also failed: {:?} — treating position as closed to prevent spam",
                                                    fok_result.error_msg
                                                );
                                                state.auto_sell_placed = true;
                                                state.stop_loss_placed = true;
                                                state.tp_limit_order_id = None;
                                                state.tp_placed_size = None;
                                                state.tp_cumulative_filled = Decimal::ZERO;
                                                state.tp_last_order_filled = Decimal::ZERO;
                                                state.tp_limit_balance_retries = 0;
                                                state.sl_limit_order_id = None;
                                                state.sl_limit_order_price = None;
                                                state.pending_auto_sell = None;
                                                state.pending_stop_loss = None;
                                                state.allowance_cache = None;
                                                state.last_buy_order = None;
                                                clear_pending_gtc(&mut state);
                                            }
                                        }
                                    } else if is_invalid_amounts_error(result.error_msg.as_deref()) {
                                        // Position may already be closed (TP or SL filled); remaining balance is dust below API minimum.
                                        let available_is_dust = available.map_or(false, |a| a < TP_SL_DUST_SIZE);
                                        if available_is_dust || size < TP_SL_DUST_SIZE {
                                            info!(
                                                "[IntervalSniper] TP 'invalid amounts' with dust (available={:?}, size={}) — position already closed",
                                                available, size
                                            );
                                            state.auto_sell_placed = true;
                                            state.stop_loss_placed = true;
                                            // If we had SL active, position was likely closed by SL (e.g. filled before we detected); allow re-entry.
                                            state.re_entry_allowed_after_sl = state.pending_stop_loss.is_some();
                                            state.tp_limit_order_id = None;
                                            state.tp_placed_size = None;
                                            state.tp_cumulative_filled = Decimal::ZERO;
                                            state.tp_last_order_filled = Decimal::ZERO;
                                            state.tp_limit_balance_retries = 0;
                                            state.sl_limit_order_id = None;
                                            state.sl_limit_order_price = None;
                                            state.sl_cumulative_filled = Decimal::ZERO;
                                            state.sl_last_order_filled = Decimal::ZERO;
                                            state.sl_limit_last_rest_check_ms = None;
                                            state.pending_auto_sell = None;
                                            state.pending_stop_loss = None;
                                            state.allowance_cache = None;
                                            state.last_buy_order = None;
                                            clear_pending_gtc(&mut state);
                                            state.balance_reflected_at_ms = None;
                                            state.balance_delay_clob_logged = false;
                                            state.last_logged_balance_up = None;
                                            state.last_logged_balance_down = None;
                                            state.total_shares_this_interval = Decimal::ZERO;
                                            // Clear WS token state so re-entry balance check is fresh.
                                            if let Some(ws) = ws_user_ref {
                                                ws.clear_token_state(&market.token_id_up).await;
                                                ws.clear_token_state(&market.token_id_down).await;
                                            }
                                        } else {
                                            state.tp_limit_balance_retries += 1;
                                            warn!(
                                                "[IntervalSniper] TP limit 'invalid amounts' error (retry {}): size={}, available={:?}, position={}",
                                                state.tp_limit_balance_retries, size, available, position_size_real
                                            );
                                            if state.tp_limit_balance_retries >= 5 {
                                                warn!("[IntervalSniper] TP limit failed 5+ times with 'invalid amounts' — canceling TP, SL remains active");
                                                state.tp_limit_order_id = None;
                                                state.tp_placed_size = None;
                                                state.tp_limit_balance_retries = 0;
                                                state.pending_auto_sell = None;
                                                state.allowance_cache = None;
                                            } else {
                                                state.allowance_cache = None;
                                                tokio::time::sleep(Duration::from_millis(100)).await;
                                            }
                                        }
                                    } else if let Some(ref msg) = result.error_msg {
                                        warn!("[IntervalSniper] TP limit place failed: {}", msg);
                                    }
                                }
                                } // end else (confirmed_ok)
                            }
                        }
                    }
                }
            }
        }

        // Buy path: up to MAX_TRADES_PER_INTERVAL per interval; re-entry only after SL (not after TP).
        // Require !ordered_this_interval for first slot so we don't double-buy when first order
        // returns success=false but actually filled on the exchange.
        // Never place while a GTC order is resting (waiting for fill) so we don't send a second order.
        let no_open_position = state.pending_auto_sell.is_none() && state.pending_stop_loss.is_none();
        let can_buy = no_open_position
            && state.pending_gtc_order_id.is_none()
            && (state.trades_this_interval == 0 && !state.ordered_this_interval
                || (state.trades_this_interval == 1 && state.re_entry_allowed_after_sl));
        if can_buy {
            let in_window = state.config.no_window_all_intervals
                || secs_to_close <= state.config.seconds_before_close as u64;
            let in_blocked_zone = state.config.block_buy_last_seconds > 0
                && secs_to_close <= state.config.block_buy_last_seconds as u64;
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

            if in_window && can_buy_after_open && !in_blocked_zone {
                let min_order_size = CLOB_DEFAULT_MIN_ORDER_SIZE;
                // GtcResting: trigger when best_bid touches range; place GTC limit at max_buy_price + 1 tick.
                // Gtc: trigger when best_ask in range; place GTC limit at min_buy_price.
                // FokCrossSpread: trigger when best_ask in range; place FOK at exact price if min==max else best_ask + 1 tick (all-or-nothing).
                // Otherwise (FakCrossSpread etc): trigger when best_ask in range; place FAK at best_ask + 1 tick (clamped to range).
                let entry = match state.config.order_strategy {
                    OrderStrategy::GtcResting => choose_side_by_bid(&state.config, &top, min_order_size)
                        .map(|(side, _best_bid, size_available)| {
                            let limit_price =
                                round_to_tick(state.config.max_buy_price + TICK_SIZE);
                            (side, size_available, OrderType::Gtc, limit_price)
                        }),
                    OrderStrategy::Gtc => choose_side(&state.config, &top, min_order_size)
                        .map(|(side, _best_ask, size_available)| {
                            let limit_price = round_to_tick(state.config.min_buy_price);
                            (side, size_available, OrderType::Gtc, limit_price)
                        }),
                    OrderStrategy::FokCrossSpread => {
                        let exact_price = state.config.min_buy_price == state.config.max_buy_price;
                        choose_side(&state.config, &top, min_order_size).map(
                            |(side, best_ask, size_available)| {
                                let limit_price = if exact_price {
                                    round_to_tick(state.config.min_buy_price)
                                } else {
                                    round_to_tick(
                                        (best_ask + TICK_SIZE)
                                            .max(state.config.min_buy_price)
                                            .min(state.config.max_buy_price),
                                    )
                                    .max(best_ask)
                                };
                                (side, size_available, OrderType::Fok, limit_price)
                            },
                        )
                    }
                    _ => choose_side(&state.config, &top, min_order_size).map(
                        |(side, best_ask, size_available)| {
                            let exact_price =
                                state.config.min_buy_price == state.config.max_buy_price;
                            let limit_price = if exact_price {
                                round_to_tick(state.config.min_buy_price)
                            } else {
                                round_to_tick(
                                    (best_ask + TICK_SIZE)
                                        .max(state.config.min_buy_price)
                                        .min(state.config.max_buy_price),
                                )
                                .max(best_ask)
                            };
                            (side, size_available, OrderType::Fak, limit_price)
                        },
                    ),
                };
                if entry.is_none()
                    && state.trades_this_interval == 1
                    && state.re_entry_allowed_after_sl
                {
                    debug!(
                        "[IntervalSniper] Re-entry allowed after SL but price not in range (min_buy={} max_buy={}) — waiting for target",
                        state.config.min_buy_price, state.config.max_buy_price
                    );
                }
                if let Some((side, size_available, order_type, limit_price)) = entry {
                    let token_id = match side {
                        EntrySide::Up => &market.token_id_up,
                        EntrySide::Down => &market.token_id_down,
                    };
                    // Second buy only when first was SL and no pending balance (so we don't add to dust).
                    let is_second_buy = state.trades_this_interval == 1 && state.re_entry_allowed_after_sl;
                    if is_second_buy {
                        // Use min order size (not DUST_THRESHOLD) to decide if there's a meaningful open position.
                        // On this CLOB, balances below the minimum order size are effectively unsellable "dust" and
                        // should not block the SL re-entry forever.
                        let bal_up = get_available_for_sell(
                            clob.as_ref().as_ref(),
                            ws_user_ref,
                            &market.token_id_up,
                            &mut state.allowance_cache,
                            false,
                        )
                        .await;
                        let bal_down = get_available_for_sell(
                            clob.as_ref().as_ref(),
                            ws_user_ref,
                            &market.token_id_down,
                            &mut state.allowance_cache,
                            false,
                        )
                        .await;
                        let has_sellable_balance = bal_up.map_or(false, |b| b >= CLOB_DEFAULT_MIN_ORDER_SIZE)
                            || bal_down.map_or(false, |b| b >= CLOB_DEFAULT_MIN_ORDER_SIZE);
                        if has_sellable_balance {
                            // Pending sellable balance: wait until it's settled before re-entry.
                            tokio::time::sleep(Duration::from_millis(loop_ms)).await;
                            continue;
                        }

                        // Dust cleanup: before re-entry, sell any remaining balance for this token so the new
                        // position starts clean and the subsequent SL does not hit 400s from dirty balance.
                        let reentry_token_balance = match side {
                            EntrySide::Up => bal_up,
                            EntrySide::Down => bal_down,
                        };
                        if reentry_token_balance.map_or(false, |b| b > Decimal::ZERO && b >= CLOB_DEFAULT_MIN_ORDER_SIZE) {
                            let dust_balance = reentry_token_balance.unwrap();
                            let side_book = match side {
                                EntrySide::Up => &top.token_id_up,
                                EntrySide::Down => &top.token_id_down,
                            };
                            if let Some(ref sb) = side_book.as_ref() {
                                if let Some(best_bid) = sb.best_bid.as_ref().filter(|b| **b > Decimal::ZERO) {
                                    let dust_size = effective_sell_size(
                                        dust_balance,
                                        Some(dust_balance),
                                        CLOB_DEFAULT_MIN_ORDER_SIZE,
                                    );
                                    if dust_size >= MIN_SELL_SIZE {
                                        let dust_price = round_to_tick(*best_bid);
                                        info!(
                                            "[IntervalSniper] Re-entry dust cleanup: selling {} @ {} (FAK) before new buy",
                                            fmt_decimal_2(&dust_size), fmt_price(Some(&dust_price))
                                        );
                                        let dust_result = clob
                                            .place_sell_order(
                                                token_id,
                                                dust_price,
                                                dust_size.clone(),
                                                crate::types::SellOrderTimeInForce::Fak,
                                            )
                                            .await;
                                        match &dust_result {
                                            Err(e) => {
                                                warn!("[IntervalSniper] Re-entry dust sell failed: {} (continuing with buy)", e);
                                            }
                                            Ok(res) => {
                                                if res.success {
                                                    debug!("[IntervalSniper] Re-entry dust sell filled, proceeding to buy");
                                                }
                                                if let Some(ref mut log) = state.session_log {
                                                    let _ = log.log_order_submitted(
                                                        &market.slug,
                                                        market.interval_start_unix,
                                                        market.close_time_unix,
                                                        now_ms_u,
                                                        token_id,
                                                        "SELL",
                                                        "FAK",
                                                        dust_price,
                                                        dust_size,
                                                        res.order_id.as_deref(),
                                                        res.http_status,
                                                        res.success,
                                                        res.error_msg.as_deref(),
                                                    );
                                                }
                                            }
                                        }
                                        // Brief wait so balance/allowance reflects the dust sell before placing buy.
                                        tokio::time::sleep(Duration::from_millis(50)).await;
                                    }
                                }
                            }
                        }
                    }
                    let effective_price = limit_price;
                    let shares_left = state.config.size_shares - state.total_shares_this_interval;
                    // Cap at shares_left so we never order more than configured size (e.g. exactly 7 shares).
                    // Round to 2 decimals so we never send 7.24000001 when user wants 7.
                    let size = size_4_decimals(
                        shares_left
                            .min(size_available)
                            .max(min_order_size)
                            .round_dp(2),
                    );
                    let _maker_amount =
                        maker_amount_2_decimals(size.clone(), effective_price.clone());
                    if size >= min_order_size && size > Decimal::ZERO {
                        let params = LimitOrderParams {
                            token_id: token_id.to_string(),
                            side: OrderSide::Buy,
                            price: effective_price.clone(),
                            size: size.clone(),
                            expiration_unix: None,
                            post_only: false,
                            fee_rate_bps: None,
                        };
                        let type_str = match order_type {
                            OrderType::Gtc => "GTC limit",
                            OrderType::Fok => "FOK",
                            OrderType::Fak => "FAK",
                            _ => "limit",
                        };
                        debug!(
                            "[IntervalSniper] Placing {} buy size={} @ {} (range {}-{})",
                            type_str,
                            size,
                            fmt_decimal_2(&effective_price),
                            state.config.min_buy_price,
                            state.config.max_buy_price
                        );
                        let t_order_start = Instant::now();
                        let result = clob.place_limit_order(params, order_type).await?;
                        if let Some(ref mut log) = state.session_log {
                            let order_type_str = match order_type {
                                OrderType::Gtc => "GTC",
                                OrderType::Gtd => "GTD",
                                OrderType::Fok => "FOK",
                                OrderType::Fak => "FAK",
                            };
                            let _ = log.log_order_submitted(
                                &market.slug,
                                market.interval_start_unix,
                                market.close_time_unix,
                                now_ms_u,
                                token_id,
                                "BUY",
                                order_type_str,
                                effective_price.clone(),
                                size.clone(),
                                result.order_id.as_deref(),
                                result.http_status,
                                result.success,
                                result.error_msg.as_deref(),
                            );
                        }
                        // Mark that we attempted a buy this interval (prevents second buy if first
                        // returned success=false but filled on exchange; re-entry only after SL).
                        state.ordered_this_interval = true;
                        if result.success {
                            let is_gtc_resting = order_type == OrderType::Gtc
                                && result.filled_size.as_ref().map_or(true, |s| *s < size.clone() * dec!(0.01))
                                && result.order_id.is_some()
                                && state.ws_user.is_some();
                            if is_gtc_resting {
                                state.pending_gtc_order_id = result.order_id.clone();
                                state.pending_gtc_token_id = Some(token_id.to_string());
                                state.pending_gtc_side = Some(side);
                                state.pending_gtc_price = Some(effective_price.clone());
                                state.pending_gtc_requested_size = Some(size.clone());
                                state.pending_gtc_timestamp_ms = Some(now_ms_u);
                                state.pending_gtc_last_observed_filled = None;
                                state.pending_gtc_fill_deltas.clear();
                                let side_str = match side {
                                    EntrySide::Up => "Up  ",
                                    EntrySide::Down => "Down",
                                };
                                info!(
                                    "[IntervalSniper]  GTC   {}  @ {}   size={}   order_id={:?} — waiting for fill (WS)",
                                    side_str,
                                    fmt_decimal_2(&effective_price),
                                    fmt_decimal_2(&size),
                                    result.order_id.as_ref().map(|s| s.chars().take(20).collect::<String>())
                                );
                            } else {
                                // Position must use actual filled_size from CLOB (FAK can be partial; TP/SL must sell only what we have).
                                // FAK/FOK often return filled_size=None from API; query WS user channel for actual filled size before setting TP/SL.
                                let filled = {
                                    let from_api = result
                                        .filled_size
                                        .as_ref()
                                        .filter(|s| **s > Decimal::ZERO && **s >= size.clone() * dec!(0.01))
                                        .cloned();
                                    let from_ws = if from_api.is_none() {
                                        match (
                                            result.order_id.as_deref(),
                                            state.ws_user.as_ref().map(|a| a.as_ref()),
                                        ) {
                                            (Some(oid), Some(ws)) => {
                                                ws.get_order_filled_size_with_type(oid)
                                                    .await
                                                    .map(|(s, _)| s)
                                            }
                                            _ => None,
                                        }
                                    } else {
                                        None
                                    };
                                    from_api
                                        .or(from_ws)
                                        .unwrap_or_else(|| size.clone())
                                        .min(size.clone())
                                };
                                state.trades_this_interval += 1;
                                state.total_shares_this_interval += filled.clone();
                                let entry_price = effective_price;
                                let entry_side = side;
                                state.last_buy_order = Some(LastBuyOrder {
                                    order_id: result.order_id.clone(),
                                    token_id: token_id.to_string(),
                                    side: entry_side,
                                    size: filled.clone(),
                                    price: entry_price.clone(),
                                    timestamp_ms: now_ms_u,
                                });
                                if let (Some(log), Some(oid)) =
                                    (state.session_log.as_mut(), result.order_id.as_deref())
                                {
                                    let _ = log.log_order_filled(
                                        &market.slug,
                                        market.interval_start_unix,
                                        market.close_time_unix,
                                        now_ms_u,
                                        oid,
                                        filled.clone(),
                                        "api_response",
                                    );
                                }
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
                                state.allowance_cache = None;
                                state.auto_sell_placed = false;
                                state.stop_loss_placed = false;
                                let http_ms = t_order_start.elapsed().as_millis();
                                let side_str = match entry_side {
                                    EntrySide::Up => "Up  ",
                                    EntrySide::Down => "Down",
                                };
                                info!(
                                    "[IntervalSniper]  BUY   {}  @ {}   size={}   http={}ms   TP={} SL={}",
                                    side_str,
                                    fmt_decimal_2(&entry_price),
                                    fmt_decimal_2(&state.last_buy_order.as_ref().unwrap().size),
                                    http_ms,
                                    fmt_decimal_2(&tp_size),
                                    fmt_decimal_2(&sl_size)
                                );
                                let w = state.ws_user.clone();
                                log_balance_after_buy(
                                    clob.as_ref().as_ref(),
                                    &market,
                                    w.as_ref().map(|a| a.as_ref()),
                                    state.last_buy_order.as_ref().map(|b| b.timestamp_ms),
                                    state.last_buy_order.as_ref().map(|b| b.side),
                                    state.last_buy_order.as_ref().map(|b| (b.side, b.size.clone())),
                                )
                                .await;
                            }
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
