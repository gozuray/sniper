use std::time::Instant;

use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use crate::config::Config;
use crate::dedupe::{Dedupe, IntentKind};
use crate::orderbook::OrderBook;
use crate::position::Position;

#[derive(Debug, Clone)]
pub struct LiveBuyOrder {
    pub order_id: String,
    pub price: Decimal,
    pub size: Decimal,
    /// When the order was placed; used to avoid cancelling before it has time to fill.
    pub placed_at: Instant,
    /// Filled amount we have already added to position (from post_order response or previous sync).
    pub filled_so_far: Decimal,
}

#[derive(Debug)]
pub enum Action {
    SendSL {
        size: Decimal,
        limit_price: Decimal,
    },
    SendTP {
        size: Decimal,
        limit_price: Decimal,
    },
    PlaceBuy {
        size: Decimal,
        price: Decimal,
    },
    CancelBuy {
        order_id: String,
    },
    CancelAndReplaceBuy {
        cancel_order_id: String,
        new_size: Decimal,
        new_price: Decimal,
    },
    Nothing,
}

/// Evaluate the current tick. Returns a single Action following the
/// priority chain: SL > TP > Buy (early return).
/// When traded_this_interval is true, no new buy is allowed until the next interval.
/// When interval_data is Some, blocks buy for min_delay_after_interval_start_sec after interval start or after interval switch.
pub fn evaluate(
    config: &Config,
    book: &OrderBook,
    position: &Position,
    dedupe: &Dedupe,
    live_buy: Option<&LiveBuyOrder>,
    book_is_stale: bool,
    traded_this_interval: bool,
    interval_data: Option<(Option<u64>, tokio::time::Instant)>,
    now_unix: u64,
    now: Instant,
) -> Action {
    let best_bid = match book.best_bid {
        Some(b) => b,
        None => return Action::Nothing,
    };

    // ── SL (highest priority) ──────────────────────────────────────
    if best_bid <= config.stop_loss_trigger && position.has_position() {
        let size = position.shares;
        if dedupe.can_send(IntentKind::SellSL, Some(size)) {
            return Action::SendSL {
                size,
                limit_price: best_bid,
            };
        }
        return Action::Nothing; // early return even if deduped
    }

    // ── TP ─────────────────────────────────────────────────────────
    if best_bid >= config.take_profit_trigger && position.has_position() {
        let size = position.shares;
        if dedupe.can_send(IntentKind::SellTP, Some(size)) {
            return Action::SendTP {
                size,
                limit_price: best_bid,
            };
        }
        return Action::Nothing; // early return even if deduped
    }

    // ── Buy (only if no SL/TP this tick, book not stale) ──────────
    // Only buy when the price we pay (best_ask) is in [buy_min, buy_max]. No best_ask = no buy.
    let in_entry_zone = match book.best_ask {
        Some(a) => a >= config.buy_min && a <= config.buy_max,
        None => false,
    };

    if !in_entry_zone {
        return match live_buy {
            Some(existing) => {
                let min_age = std::time::Duration::from_millis(config.buy_order_min_age_ms);
                if now.duration_since(existing.placed_at) < min_age {
                    Action::Nothing
                } else {
                    Action::CancelBuy {
                        order_id: existing.order_id.clone(),
                    }
                }
            }
            None => Action::Nothing,
        };
    }

    // best_ask is Some and in range; use it as limit price (clamped to be safe).
    let target_price = book.best_ask.unwrap().max(config.buy_min).min(config.buy_max);

    // One trade per interval; no buy within min_delay after interval start or switch.
    let min_delay = config.min_delay_after_interval_start_sec;
    let within_delay_after_switch = interval_data
        .as_ref()
        .map(|(_, switch_instant)| switch_instant.elapsed().as_secs() < min_delay)
        .unwrap_or(false);
    let within_delay_after_interval_start = interval_data
        .and_then(|(close_opt, _)| close_opt)
        .map(|close_time_unix| {
            let interval_start = close_time_unix.saturating_sub(300);
            now_unix.saturating_sub(interval_start) < min_delay
        })
        .unwrap_or(false);
    if within_delay_after_switch || within_delay_after_interval_start {
        return Action::Nothing;
    }

    if traded_this_interval {
        return Action::Nothing;
    }

    if book_is_stale {
        return Action::Nothing;
    }

    if position.shares >= config.max_position {
        return Action::Nothing;
    }

    let remaining_capacity = config.max_position - position.shares;
    let size = config.order_size.min(remaining_capacity);
    if size <= Decimal::ZERO {
        return Action::Nothing;
    }

    if !dedupe.can_send(IntentKind::Buy, None) {
        return Action::Nothing;
    }

    match live_buy {
        Some(existing) => {
            let min_age = std::time::Duration::from_millis(config.buy_order_min_age_ms);
            if now.duration_since(existing.placed_at) < min_age {
                Action::Nothing
            } else {
                let tick = dec!(0.01);
                if (existing.price - target_price).abs() > tick {
                    Action::CancelAndReplaceBuy {
                        cancel_order_id: existing.order_id.clone(),
                        new_size: size,
                        new_price: target_price,
                    }
                } else {
                    Action::Nothing
                }
            }
        }
        None => Action::PlaceBuy {
            size,
            price: target_price,
        },
    }
}
