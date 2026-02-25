use rust_decimal::Decimal;
use rust_decimal_macros::dec;

#[derive(Debug)]
pub struct Position {
    pub shares: Decimal,
}

impl Position {
    pub fn new() -> Self {
        Self { shares: dec!(0) }
    }

    pub fn add_fill(&mut self, filled: Decimal) {
        self.shares += filled;
        tracing::info!(shares = %self.shares, filled = %filled, "position increased (buy fill)");
    }

    pub fn subtract_fill(&mut self, filled: Decimal) {
        self.shares -= filled;
        if self.shares < dec!(0) {
            tracing::warn!(shares = %self.shares, "position went negative, clamping to 0");
            self.shares = dec!(0);
        }
        tracing::info!(shares = %self.shares, filled = %filled, "position decreased (sell fill)");
    }

    pub fn has_position(&self) -> bool {
        self.shares > dec!(0)
    }

    pub fn set(&mut self, shares: Decimal) {
        self.shares = shares;
        tracing::info!(shares = %self.shares, "position set via REST refresh");
    }
}
