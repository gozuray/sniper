use rust_decimal::Decimal;
use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct OrderBook {
    pub best_bid: Option<Decimal>,
    pub best_ask: Option<Decimal>,
    last_update: Instant,
}

impl OrderBook {
    pub fn new() -> Self {
        Self {
            best_bid: None,
            best_ask: None,
            last_update: Instant::now(),
        }
    }

    pub fn update_best(&mut self, best_bid: Option<Decimal>, best_ask: Option<Decimal>) {
        if best_bid.is_some() {
            self.best_bid = best_bid;
        }
        if best_ask.is_some() {
            self.best_ask = best_ask;
        }
        self.last_update = Instant::now();
    }

    pub fn update_from_levels(
        &mut self,
        bids: &[(Decimal, Decimal)],
        asks: &[(Decimal, Decimal)],
    ) {
        self.best_bid = bids.iter().map(|(p, _)| *p).max();
        self.best_ask = asks.iter().map(|(p, _)| *p).min();
        self.last_update = Instant::now();
    }

    pub fn is_stale(&self, threshold: Duration) -> bool {
        self.last_update.elapsed() > threshold
    }
}
