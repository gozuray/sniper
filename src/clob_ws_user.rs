//! CLOB WebSocket client for user channel: real-time order and trade updates.
//!
//! Connects to `wss://ws-subscriptions-clob.polymarket.com/ws/user`, authenticates with
//! API key/secret/passphrase, subscribes by condition_id (market), and keeps per-order
//! fill state (size_matched) so the runner can know fills without waiting for balance.

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time::interval;
use tokio_tungstenite::{connect_async, tungstenite::Message};

/// Default CLOB WebSocket user endpoint (authenticated).
pub const DEFAULT_WS_USER_URL: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/user";

const PING_INTERVAL_SECS: u64 = 10;

/// Per-order state from user channel (order events).
#[derive(Debug, Clone)]
pub struct UserOrderState {
    pub order_id: String,
    pub asset_id: String,
    pub side: String, // "BUY" | "SELL"
    pub original_size: Decimal,
    pub size_matched: Decimal,
    pub order_type: String, // PLACEMENT | UPDATE | CANCELLATION
}

fn parse_decimal_value(v: &serde_json::Value) -> Option<Decimal> {
    let s = v
        .as_str()
        .map(|s| s.trim().to_string())
        .or_else(|| v.as_i64().map(|n| n.to_string()))
        .or_else(|| v.as_u64().map(|n| n.to_string()))
        .or_else(|| v.as_f64().map(|f| f.to_string()))?;
    Decimal::from_str(&s).ok().filter(|d| *d >= Decimal::ZERO)
}

/// Canonical order_id for map key: "0x" + lowercase hex (REST and WS may differ by case).
fn normalize_order_id(id: &str) -> String {
    let s = id.trim().trim_start_matches("0x").to_lowercase();
    if s.is_empty() {
        id.to_string()
    } else {
        format!("0x{}", s)
    }
}

/// Client for CLOB WebSocket user channel. Holds order fill state in a background task.
pub struct ClobWsUser {
    state: Arc<RwLock<HashMap<String, UserOrderState>>>,
    _join: tokio::task::JoinHandle<()>,
}

impl ClobWsUser {
    /// Connect to the user WebSocket, authenticate, and start the receive + ping loop.
    /// Pass empty `condition_ids` to receive events for all markets (recommended for persistent connection).
    /// Uses API_KEY, SECRET, PASSPHRASE from env.
    pub async fn connect(ws_url: &str, condition_ids: &[String]) -> Result<Self> {
        let api_key = std::env::var("API_KEY").context("API_KEY required for user WebSocket")?;
        let secret = std::env::var("SECRET")
            .or_else(|_| std::env::var("API_SECRET"))
            .context("SECRET or API_SECRET required")?;
        let passphrase = std::env::var("PASSPHRASE")
            .or_else(|_| std::env::var("API_PASSPHRASE"))
            .context("PASSPHRASE required")?;

        let url = if ws_url.is_empty() {
            DEFAULT_WS_USER_URL
        } else {
            ws_url
        };
        let (ws_stream, _) = connect_async(url)
            .await
            .context("CLOB user WebSocket connect")?;

        let (mut write, mut read) = ws_stream.split();
        let state: Arc<RwLock<HashMap<String, UserOrderState>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let state_recv = Arc::clone(&state);

        let sub = serde_json::json!({
            "auth": {
                "apiKey": api_key,
                "secret": secret,
                "passphrase": passphrase
            },
            "markets": condition_ids,
            "type": "user"
        });
        write
            .send(Message::Text(sub.to_string()))
            .await
            .context("send user subscribe")?;

        let join = tokio::spawn(async move {
            let mut ping_interval = interval(Duration::from_secs(PING_INTERVAL_SECS));
            ping_interval.tick().await;

            loop {
                tokio::select! {
                    _ = ping_interval.tick() => {
                        if write.send(Message::Ping(vec![])).await.is_err() {
                            break;
                        }
                    }
                    msg = read.next() => {
                        let Some(Ok(msg)) = msg else { break };
                        if let Message::Text(text) = msg {
                            if let Err(e) = Self::apply_message(&state_recv, &text).await {
                                tracing::debug!("ClobWsUser parse: {} | payload: {}", e, text.chars().take(300).collect::<String>());
                            }
                        }
                    }
                }
            }
        });

        Ok(Self { state, _join: join })
    }

    /// Build WebSocket user URL from REST host (same pattern as market WS).
    pub fn ws_url_from_rest_host(rest_host: &str) -> String {
        let rest = rest_host.trim_end_matches('/');
        if rest.contains("clob.polymarket.com") {
            DEFAULT_WS_USER_URL.to_string()
        } else {
            DEFAULT_WS_USER_URL.to_string()
        }
    }

    async fn apply_message(state: &RwLock<HashMap<String, UserOrderState>>, text: &str) -> Result<()> {
        let value: serde_json::Value = serde_json::from_str(text).context("parse JSON")?;
        let event_type = value.get("event_type").and_then(|v| v.as_str()).unwrap_or("");

        if event_type == "order" {
            let id = value
                .get("id")
                .and_then(|v| v.as_str())
                .map(String::from);
            let asset_id = value
                .get("asset_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let original_size = value
                .get("original_size")
                .and_then(parse_decimal_value)
                .unwrap_or(Decimal::ZERO);
            let size_matched = value
                .get("size_matched")
                .and_then(parse_decimal_value)
                .unwrap_or(Decimal::ZERO);
            let order_type = value
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let side = value
                .get("side")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_uppercase();

            if let Some(order_id) = id {
                let key = normalize_order_id(&order_id);
                let entry = UserOrderState {
                    order_id: key.clone(),
                    asset_id,
                    side,
                    original_size,
                    size_matched,
                    order_type: order_type.clone(),
                };
                let mut map = state.write().await;
                map.insert(key, entry);
            }
        } else if event_type == "trade" {
            // Trade events often arrive before the order UPDATE with size_matched.
            // Process them immediately so get_balance_for_token reflects fills faster.
            let trade_size = value.get("size").and_then(parse_decimal_value).unwrap_or(Decimal::ZERO);
            if trade_size > Decimal::ZERO {
                let asset_id = value.get("asset_id").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let side = value.get("side")
                    .or_else(|| value.get("trader_side"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_uppercase();

                // Try taker_order_id first (our FAK/FOK orders), then maker_order_id (our GTC orders).
                let order_id = value.get("taker_order_id")
                    .or_else(|| value.get("maker_order_id"))
                    .and_then(|v| v.as_str())
                    .map(String::from);

                if let Some(oid) = order_id {
                    let key = normalize_order_id(&oid);
                    let mut map = state.write().await;
                    if let Some(existing) = map.get_mut(&key) {
                        let new_matched = existing.size_matched + trade_size;
                        existing.size_matched = new_matched;
                    } else {
                        map.insert(key.clone(), UserOrderState {
                            order_id: key,
                            asset_id,
                            side,
                            original_size: trade_size,
                            size_matched: trade_size,
                            order_type: "TRADE".to_string(),
                        });
                    }
                }
            }
        }

        Ok(())
    }

    /// Return the current filled size (size_matched) for an order, if known.
    pub async fn get_order_filled_size(&self, order_id: &str) -> Option<Decimal> {
        let key = normalize_order_id(order_id);
        let map = self.state.read().await;
        map.get(&key)
            .filter(|s| s.size_matched > Decimal::ZERO)
            .map(|s| s.size_matched)
    }

    /// Return balance for a token derived from WS order fills: sum of size_matched for BUY orders
    /// minus sum for SELL orders for this asset_id. Returns None if we have no orders for this token.
    pub async fn get_balance_for_token(&self, asset_id: &str) -> Option<Decimal> {
        let map = self.state.read().await;
        let mut buy_sum = Decimal::ZERO;
        let mut sell_sum = Decimal::ZERO;
        for s in map.values() {
            if s.asset_id != asset_id {
                continue;
            }
            if s.side == "BUY" {
                buy_sum += s.size_matched;
            } else if s.side == "SELL" {
                sell_sum += s.size_matched;
            }
        }
        let net = buy_sum - sell_sum;
        let has_any = buy_sum > Decimal::ZERO || sell_sum > Decimal::ZERO;
        if has_any {
            Some(net.max(Decimal::ZERO))
        } else {
            None
        }
    }

    /// Return full order state for an order_id.
    pub async fn get_order_state(&self, order_id: &str) -> Option<UserOrderState> {
        let key = normalize_order_id(order_id);
        let map = self.state.read().await;
        map.get(&key).cloned()
    }
}
