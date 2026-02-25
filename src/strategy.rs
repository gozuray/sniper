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
pub fn evaluate(
    config: &Config,
    book: &OrderBook,
    position: &Position,
    dedupe: &Dedupe,
    live_buy: Option<&LiveBuyOrder>,
    book_is_stale: bool,
    traded_this_interval: bool,
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
    // One trade per interval: if we already traded this 5-min window, do not place/replace buys.
    if traded_this_interval {
        if let Some(existing) = live_buy {
            let target_price = best_bid.max(config.buy_min).min(config.buy_max);
            if target_price < config.buy_min || target_price > config.buy_max {
                return Action::CancelBuy {
                    order_id: existing.order_id.clone(),
                };
            }
        }
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

    // Target buy price: join best_bid, clamped to [buy_min, buy_max].
    // Bot never buys outside this range.
    let target_price = best_bid.max(config.buy_min).min(config.buy_max);

    // Only place if the target is actually within our range
    if target_price < config.buy_min || target_price > config.buy_max {
        return match live_buy {
            Some(existing) => Action::CancelBuy {
                order_id: existing.order_id.clone(),
            },
            None => Action::Nothing,
        };
    }

    if !dedupe.can_send(IntentKind::Buy, None) {
        return Action::Nothing;
    }

    match live_buy {
        Some(existing) => {
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
        None => Action::PlaceBuy {
            size,
            price: target_price,
        },
    }
}
