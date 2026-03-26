use crate::config::MomentumConfig;
use crate::types::{MomentumSnapshot, Outcome, Price, ResolvedMarket, Signal, SpotAssetState};
use crate::utils::clamp_prob_dec;
use crate::utils::fair_prob_up_from_pct_change;
use rust_decimal::Decimal;
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};

/// Returns `(aligned, net_norm)`.
/// `net_norm` ∈ [-1, 1]: positive = buy pressure, negative = sell pressure.
#[inline]
fn taker_imbalance(
    buy_q: Decimal,
    sell_q: Decimal,
    direction: Outcome,
    min_imbalance: f64,
) -> (bool, f64) {
    if min_imbalance <= 0.0 {
        return (true, 0.0);
    }
    let total = buy_q + sell_q;
    if total.is_zero() {
        // No signed breakdown (incomplete feed): skip flow filter.
        return (true, 0.0);
    }
    let buy_f = buy_q.to_f64().unwrap_or(0.0);
    let sell_f = sell_q.to_f64().unwrap_or(0.0);
    let den = buy_f + sell_f;
    if den <= 0.0 {
        return (true, 0.0);
    }
    let net = (buy_f - sell_f) / den;
    let ok = match direction {
        Outcome::Up => net >= min_imbalance,
        Outcome::Down => net <= -min_imbalance,
    };
    (ok, net)
}

/// Convert a dollar delta threshold to pct using the anchor price.
/// Falls back to the pct config field if usd is None or anchor is zero.
#[inline]
fn effective_delta_pct(pct_cfg: &Decimal, usd_cfg: Option<&Decimal>, anchor_price: Decimal) -> f64 {
    if let Some(usd) = usd_cfg {
        if !anchor_price.is_zero() {
            return (*usd / anchor_price).to_f64().unwrap_or_else(|| pct_cfg.to_f64().unwrap_or(0.0));
        }
    }
    pct_cfg.to_f64().unwrap_or(0.0)
}

/// Noise gate: only emit `✗ delta` when pct is at least 10% of the threshold.
/// Suppresses flood of near-zero entries when market is flat (tick = 1 ms).
#[inline]
fn delta_log_worthy(pct_abs: f64, up_th: f64, down_th: f64) -> bool {
    let min_th = up_th.min(down_th);
    pct_abs >= min_th * 0.10
}

/// Fusiona dos snapshots con la **misma** `direction` (p. ej. Binance + Coinbase en `consensus`).
#[must_use]
pub fn merge_consensus_momentum(a: MomentumSnapshot, b: MomentumSnapshot) -> MomentumSnapshot {
    debug_assert_eq!(a.direction, b.direction);
    let anchor_interval_start_unix = if a.anchor_interval_start_unix != 0 {
        a.anchor_interval_start_unix
    } else {
        b.anchor_interval_start_unix
    };
    MomentumSnapshot {
        fair_prob_up: (a.fair_prob_up + b.fair_prob_up) / 2.0,
        pct_change: (a.pct_change + b.pct_change) / 2.0,
        quote_volume: a.quote_volume + b.quote_volume,
        strong: a.strong && b.strong,
        direction: a.direction,
        anchor_interval_start_unix,
    }
}

/// Momentum from the Binance aggTrade ring-buffer over the last `window_sec` seconds.
///
/// - **Price comparison**: oldest sample in the rolling window → `last_price` (WS).
///   Falls back to the REST anchor (`binance_5m_open`) if no samples exist in the window.
/// - **Volume / imbalance**: accumulated over the same rolling window.
/// - **Delta threshold**: uses `delta_up_usd / anchor_price` when USD is configured,
///   otherwise falls back to `delta_up_pct`. The anchor price is always the REST open.
/// - **Interval guard**: `anchor_interval_start_unix` is set so `evaluate_market_signal`
///   can reject stale-interval signals.
///
/// WS feed freshness is checked by the caller (`bin_ok`) before invoking this.
pub fn compute_momentum_from_binance_5m_anchor(
    state: &SpotAssetState,
    now_ms: u64,
    cfg: &MomentumConfig,
) -> Option<MomentumSnapshot> {
    if state.binance_5m_open.is_zero() || state.binance_5m_kline_event_ms == 0 {
        return None;
    }

    let window_cutoff = now_ms.saturating_sub(cfg.window_sec.saturating_mul(1000));

    let mut first_price_win: Option<Decimal> = None;
    let mut last_price_win: Option<Decimal> = None;
    let mut quote_sum = Decimal::ZERO;
    let mut buy_q = Decimal::ZERO;
    let mut sell_q = Decimal::ZERO;

    state.for_each_recent(window_cutoff, |s| {
        quote_sum += s.quote_volume;
        buy_q += s.taker_buy_quote;
        sell_q += s.taker_sell_quote;
        if first_price_win.is_none() {
            first_price_win = Some(s.price);
        }
        last_price_win = Some(s.price);
    });

    if quote_sum < cfg.min_quote_volume_window {
        // Only log when there is some (but insufficient) volume — avoids noise before first trade.
        if !quote_sum.is_zero() {
            tracing::trace!(
                target: "sniper",
                "mom · ✗ vol       vol={:.2}  min={:.2}",
                quote_sum, cfg.min_quote_volume_window
            );
        }
        return None;
    }

    // Solo usar fallback REST como first_price si hay volumen suficiente en ventana;
    // evita mezclar ancla 5m (0–300s) con último precio WS en los primeros segundos.
    let min_vol_bootstrap = cfg.min_quote_volume_window * Decimal::from(2u64);
    if first_price_win.is_none() && quote_sum < min_vol_bootstrap {
        return None;
    }

    // Use REST anchor as first_price when the ring-buffer window has no samples yet
    // (e.g. right after interval start before the first aggTrade arrives).
    let first_price = first_price_win.unwrap_or(state.binance_5m_open);
    let last_price = last_price_win.unwrap_or(state.last_price);

    if first_price.is_zero() || last_price.is_zero() {
        return None;
    }

    let pct_change = (last_price / first_price - Decimal::ONE)
        .to_f64()
        .unwrap_or(0.0);

    // Delta thresholds: USD-based (converted at anchor price) or pct fallback.
    let anchor_price = state.binance_5m_open;
    let up_th = effective_delta_pct(&cfg.delta_up_pct, cfg.delta_up_usd.as_ref(), anchor_price);
    let down_th = effective_delta_pct(&cfg.delta_down_pct, cfg.delta_down_usd.as_ref(), anchor_price);

    let direction = if pct_change >= up_th {
        Outcome::Up
    } else if pct_change <= -down_th {
        Outcome::Down
    } else {
        if delta_log_worthy(pct_change.abs(), up_th, down_th) {
            tracing::trace!(
                target: "sniper",
                "mom · ✗ delta     pct={:+.4}%  up={:.4}%  dn={:.4}%",
                pct_change * 100.0, up_th * 100.0, down_th * 100.0
            );
        }
        return None;
    };

    let (imb_ok, net) = taker_imbalance(buy_q, sell_q, direction, cfg.min_taker_imbalance);
    if !imb_ok {
        tracing::trace!(
            target: "sniper",
            "mom · ✗ imbalance net={:+.3}  min={:.3}  dir={:?}",
            net, cfg.min_taker_imbalance, direction
        );
        return None;
    }

    let p_strong = fair_prob_up_from_pct_change(pct_change.abs(), cfg.prob_scale);
    if p_strong < cfg.strong_prob_threshold {
        tracing::trace!(
            target: "sniper",
            "mom · ✗ prob      p={:.4}  need={:.4}",
            p_strong, cfg.strong_prob_threshold
        );
        return None;
    }

    let fair_prob_up = match direction {
        Outcome::Up => p_strong,
        Outcome::Down => 1.0 - p_strong,
    };

    let anchor_interval_start_unix = state.binance_5m_open_ms / 1000;

    tracing::info!(
        target: "sniper",
        "mom · detect · binance · window_sec={} · Δpct={:+.4}% · th_up={:.4}% th_dn={:.4}% · dir={:?} · vol={:.2} · taker_net={:+.3} · p_strong={:.4} · fair_up={:.4} · anchor_open={}",
        cfg.window_sec,
        pct_change * 100.0,
        up_th * 100.0,
        down_th * 100.0,
        direction,
        quote_sum,
        net,
        p_strong,
        fair_prob_up,
        anchor_price
    );

    Some(MomentumSnapshot {
        fair_prob_up,
        pct_change,
        quote_volume: quote_sum,
        strong: true,
        direction,
        anchor_interval_start_unix,
    })
}

/// Compute a momentum snapshot from ring-buffer samples (Coinbase fallback).
///
/// `anchor_unix`: when non-zero, attached to the snapshot so the interval guard in
/// `evaluate_market_signal` is enforced. Pass the current Polymarket interval start unix;
/// pass `0` to skip the guard.
pub fn compute_momentum_snapshot(
    state: &SpotAssetState,
    now_ms: u64,
    cfg: &MomentumConfig,
    anchor_unix: u64,
) -> Option<MomentumSnapshot> {
    let cutoff = now_ms.saturating_sub(cfg.window_sec.saturating_mul(1000));

    let mut first_price: Option<Decimal> = None;
    let mut last_price: Option<Decimal> = None;
    let mut quote_sum = Decimal::ZERO;
    let mut buy_q = Decimal::ZERO;
    let mut sell_q = Decimal::ZERO;

    state.for_each_recent(cutoff, |s| {
        quote_sum += s.quote_volume;
        buy_q += s.taker_buy_quote;
        sell_q += s.taker_sell_quote;
        if first_price.is_none() {
            first_price = Some(s.price);
        }
        last_price = Some(s.price);
    });

    let (first_price, last_price) = match (first_price, last_price) {
        (Some(f), Some(l)) => (f, l),
        _ => return None,
    };

    if quote_sum < cfg.min_quote_volume_window {
        if !quote_sum.is_zero() {
            tracing::trace!(
                target: "sniper",
                "mom(cb) · ✗ vol       vol={:.2}  min={:.2}",
                quote_sum, cfg.min_quote_volume_window
            );
        }
        return None;
    }
    if first_price.is_zero() {
        return None;
    }

    let pct_change = (last_price / first_price - Decimal::ONE)
        .to_f64()
        .unwrap_or(0.0);

    // Delta thresholds: USD-based (converted at first_price as proxy anchor) or pct fallback.
    let up_th = effective_delta_pct(&cfg.delta_up_pct, cfg.delta_up_usd.as_ref(), first_price);
    let down_th = effective_delta_pct(&cfg.delta_down_pct, cfg.delta_down_usd.as_ref(), first_price);

    let direction = if pct_change >= up_th {
        Outcome::Up
    } else if pct_change <= -down_th {
        Outcome::Down
    } else {
        if delta_log_worthy(pct_change.abs(), up_th, down_th) {
            tracing::trace!(
                target: "sniper",
                "mom(cb) · ✗ delta     pct={:+.4}%  up={:.4}%  dn={:.4}%",
                pct_change * 100.0, up_th * 100.0, down_th * 100.0
            );
        }
        return None;
    };

    let (imb_ok, net) = taker_imbalance(buy_q, sell_q, direction, cfg.min_taker_imbalance);
    if !imb_ok {
        tracing::trace!(
            target: "sniper",
            "mom(cb) · ✗ imbalance net={:+.3}  min={:.3}  dir={:?}",
            net, cfg.min_taker_imbalance, direction
        );
        return None;
    }

    let p_strong = fair_prob_up_from_pct_change(pct_change.abs(), cfg.prob_scale);
    if p_strong < cfg.strong_prob_threshold {
        tracing::trace!(
            target: "sniper",
            "mom(cb) · ✗ prob      p={:.4}  need={:.4}",
            p_strong, cfg.strong_prob_threshold
        );
        return None;
    }

    let fair_prob_up = match direction {
        Outcome::Up => p_strong,
        Outcome::Down => 1.0 - p_strong,
    };

    tracing::info!(
        target: "sniper",
        "mom · detect · coinbase · window_sec={} · Δpct={:+.4}% · th_up={:.4}% th_dn={:.4}% · dir={:?} · vol={:.2} · taker_net={:+.3} · p_strong={:.4} · fair_up={:.4} · first_px={} last_px={}",
        cfg.window_sec,
        pct_change * 100.0,
        up_th * 100.0,
        down_th * 100.0,
        direction,
        quote_sum,
        net,
        p_strong,
        fair_prob_up,
        first_price,
        last_price
    );

    Some(MomentumSnapshot {
        fair_prob_up,
        pct_change,
        quote_volume: quote_sum,
        strong: true,
        direction,
        anchor_interval_start_unix: anchor_unix,
    })
}

/// Evaluate momentum and compute edge vs Polymarket implied probabilities from best ask prices.
///
/// `market_bid_up` / `market_bid_down`: best bid for each outcome (for spread guard). `None` = no bid.
pub fn evaluate_market_signal(
    resolved: &ResolvedMarket,
    market_prob_up: Price,
    market_prob_down: Price,
    market_bid_up: Option<Price>,
    market_bid_down: Option<Price>,
    momentum: MomentumSnapshot,
    market_edge_min: &Decimal,
    max_spread_ticks: Option<Decimal>,
) -> Option<Signal> {
    if momentum.anchor_interval_start_unix != 0
        && momentum.anchor_interval_start_unix != resolved.interval_start_unix
    {
        tracing::trace!(
            target: "sniper",
            "mom · ✗ intervalo snap={}  mkt={}",
            momentum.anchor_interval_start_unix, resolved.interval_start_unix
        );
        return None;
    }

    let market_prob_up = clamp_prob_dec(market_prob_up);
    let market_prob_down = clamp_prob_dec(market_prob_down);

    let fair_prob_up_dec =
        Decimal::from_f64(momentum.fair_prob_up).unwrap_or(Decimal::ZERO);
    let fair_prob_down_dec = Decimal::ONE - fair_prob_up_dec;

    // Momentum edge: (fair − market) for the outcome we would buy.
    let (side_prob, edge) = match momentum.direction {
        Outcome::Up => (market_prob_up, fair_prob_up_dec - market_prob_up),
        Outcome::Down => (market_prob_down, fair_prob_down_dec - market_prob_down),
    };

    if edge < *market_edge_min {
        tracing::trace!(
            target: "sniper",
            "mom · ✗ edge      edge={:+.4}  need={:.4}  dir={:?}",
            edge, market_edge_min, momentum.direction
        );
        return None;
    }

    // Spread guard: reject if bid–ask spread on the entry side exceeds limit.
    if let Some(max_spread) = max_spread_ticks {
        let (ask_side, bid_side) = match momentum.direction {
            Outcome::Up => (market_prob_up, market_bid_up),
            Outcome::Down => (market_prob_down, market_bid_down),
        };
        if let Some(bid) = bid_side {
            let spread = ask_side - bid;
            if spread > max_spread {
                tracing::trace!(
                    target: "sniper",
                    "mom · ✗ spread    spread={:.4}  max={:.4}  dir={:?}",
                    spread, max_spread, momentum.direction
                );
                return None;
            }
        }
    }

    let signal = Signal::Momentum {
        market: resolved.key,
        interval_start_unix: resolved.interval_start_unix,
        close_time_unix: resolved.close_time_unix,
        token_id_up: resolved.token_id_up,
        token_id_down: resolved.token_id_down,
        outcome: momentum.direction,
        fair_prob_up: momentum.fair_prob_up,
        market_prob_side: side_prob,
        edge,
    };

    Some(signal)
}

/// Edge (fair − best ask) for the momentum direction; same definition as `evaluate_market_signal`.
pub fn momentum_edge_vs_asks(
    momentum: &MomentumSnapshot,
    market_prob_up: Price,
    market_prob_down: Price,
) -> Decimal {
    let market_prob_up = clamp_prob_dec(market_prob_up);
    let market_prob_down = clamp_prob_dec(market_prob_down);
    let fair_prob_up_dec =
        Decimal::from_f64(momentum.fair_prob_up).unwrap_or(Decimal::ZERO);
    let fair_prob_down_dec = Decimal::ONE - fair_prob_up_dec;
    match momentum.direction {
        Outcome::Up => fair_prob_up_dec - market_prob_up,
        Outcome::Down => fair_prob_down_dec - market_prob_down,
    }
}

/// Decide both-sides arbitrage based purely on Polymarket implied probabilities.
pub fn evaluate_arb_both(
    resolved: &ResolvedMarket,
    market_prob_up: Price,
    market_prob_down: Price,
    arb_yes_no_sum_max: &Decimal,
    edge_min: &Decimal,
) -> Option<Signal> {
    let p_up = clamp_prob_dec(market_prob_up);
    let p_down = clamp_prob_dec(market_prob_down);
    let p_sum = p_up + p_down;

    if p_sum > Decimal::ONE {
        return None;
    }

    if p_sum > *arb_yes_no_sum_max {
        return None;
    }

    let edge = Decimal::ONE - p_sum;
    if edge < *edge_min {
        return None;
    }

    Some(Signal::ArbBoth {
        market: resolved.key,
        interval_start_unix: resolved.interval_start_unix,
        close_time_unix: resolved.close_time_unix,
        token_id_up: resolved.token_id_up,
        token_id_down: resolved.token_id_down,
        fair_prob_up: 0.5,
        market_prob_up: p_up,
        market_prob_down: p_down,
        edge,
    })
}
