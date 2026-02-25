use rust_decimal::Decimal;
use std::collections::HashMap;
use std::time::{Duration, Instant};

#[derive(Hash, Eq, PartialEq, Clone, Debug)]
pub enum IntentKind {
    Buy,
    SellTP,
    SellSL,
}

/// Dedupe key: (kind, size). For sells, size is included so that
/// "size changed" (e.g. remainder after partial fill) counts as a new intent.
/// For buys, size is None (only one buy intent at a time).
#[derive(Hash, Eq, PartialEq, Clone, Debug)]
struct IntentKey {
    kind: IntentKind,
    size: Option<Decimal>,
}

#[derive(Debug)]
pub struct Dedupe {
    ttl: Duration,
    last_sent: HashMap<IntentKey, Instant>,
}

impl Dedupe {
    pub fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            last_sent: HashMap::new(),
        }
    }

    pub fn can_send(&self, kind: IntentKind, size: Option<Decimal>) -> bool {
        let key = IntentKey {
            kind,
            size,
        };
        match self.last_sent.get(&key) {
            Some(ts) => ts.elapsed() >= self.ttl,
            None => true,
        }
    }

    pub fn record(&mut self, kind: IntentKind, size: Option<Decimal>) {
        let key = IntentKey {
            kind,
            size,
        };
        self.last_sent.insert(key, Instant::now());
    }

    pub fn cleanup(&mut self) {
        let cutoff = self.ttl * 10;
        self.last_sent.retain(|_, ts| ts.elapsed() < cutoff);
    }
}
