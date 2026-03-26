use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;

/// Clamp an f64 into `[min, max]`.
#[inline]
pub fn clamp_f64(x: f64, min: f64, max: f64) -> f64 {
    if x < min {
        min
    } else if x > max {
        max
    } else {
        x
    }
}

/// Clamp a Decimal into `[0, 1]`.
#[inline]
pub fn clamp_prob_dec(p: Decimal) -> Decimal {
    if p < Decimal::ZERO {
        Decimal::ZERO
    } else if p > Decimal::ONE {
        Decimal::ONE
    } else {
        p
    }
}

/// Convert CEX pct_change (as f64, e.g. 0.012 => +1.2%) into an implied fair probability for `Up`.
///
/// Strategy mapping:
/// - Returns a probability in [0.5, 0.95] for finite signals.
/// - Positive returns increase p_up, negative returns decrease p_up symmetrically.
///
/// We use a logistic curve:
///     p_up = 1 / (1 + exp(-scale * pct_change))
/// then clamp to [0.5, 0.95].
#[inline]
pub fn fair_prob_up_from_pct_change(pct_change: f64, scale: f64) -> f64 {
    let x = scale * pct_change;
    // Using f64 exp is fine here: it is not in a tight loop with millions of iterations.
    let p = 1.0 / (1.0 + (-x).exp());
    clamp_f64(p, 0.5, 0.95)
}

/// Apply a slippage in basis points to a probability/price (0..1 domain).
#[inline]
pub fn apply_slippage_bps(price: Decimal, slippage_bps: Decimal) -> Decimal {
    // slippage_bps in bps => multiplier = 1 + bps/10_000
    let multiplier = Decimal::ONE + slippage_bps / Decimal::from(10_000u64);
    (price * multiplier).min(Decimal::ONE).max(Decimal::ZERO)
}

/// Fractional P&L in USDC.
#[inline]
#[allow(dead_code)]
pub fn pnl_as_fraction(pnl: Decimal, starting_balance: Decimal) -> Option<f64> {
    if starting_balance.is_zero() {
        None
    } else {
        Some((pnl / starting_balance).to_f64().unwrap_or(0.0))
    }
}

