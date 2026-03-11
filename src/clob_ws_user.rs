//! CLOB WebSocket client for user channel: real-time order and trade updates.
//!
//! Connects to `wss://ws-subscriptions-clob.polymarket.com/ws/user`, authenticates with
//! API key/secret/passphrase, subscribes by condition_id (market), and keeps per-order
//! fill state (size_matched) so the runner can know fills without waiting for balance.
//!
//! **Reconnection:** On connection close or error, the client automatically reconnects
//! in a loop (no bot restart needed). Fills use REST fallback until the WS is restored.

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time::interval;
use tokio_tungstenite::{connect_async, tungstenite::Message};

/// Default CLOB WebSocket user endpoint (authenticated).
pub const DEFAULT_WS_USER_URL: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/user";

const PING_INTERVAL_SECS: u64 = 10;
/// Max time to wait for TCP+WS handshake; avoids hanging indefinitely on reconnect.
const CONNECT_TIMEOUT_SECS: u64 = 20;

/// Per-order state from user channel (order events).
#[derive(Debug, Clone)]
pub struct UserOrderState {
    #[allow(dead_code)]
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
/// Tracks MATCHED BUY trade sizes per asset so the runner can trigger TP/SL placement (update_balance_allowance + backoff).
pub struct ClobWsUser {
    state: Arc<RwLock<HashMap<String, UserOrderState>>>,
    /// Cumulative size of BUY trades with status MATCHED per asset_id (used as trigger for TP/SL placement).
    confirmed_buy: Arc<RwLock<HashMap<String, Decimal>>>,
    /// Token IDs for the current market; [Blockchain] trade log is emitted only when asset_id is in this set (empty = log all).
    active_token_ids: Arc<RwLock<HashSet<String>>>,
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

        let url: String = if ws_url.is_empty() {
            DEFAULT_WS_USER_URL.to_string()
        } else {
            ws_url.to_string()
        };
        let condition_ids = condition_ids.to_vec();
        let state: Arc<RwLock<HashMap<String, UserOrderState>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let confirmed_buy: Arc<RwLock<HashMap<String, Decimal>>> =
            Arc::new(RwLock::new(HashMap::new()));
        let active_token_ids: Arc<RwLock<HashSet<String>>> = Arc::new(RwLock::new(HashSet::new()));
        let state_recv = Arc::clone(&state);
        let confirmed_buy_recv = Arc::clone(&confirmed_buy);
        let active_token_ids_recv = Arc::clone(&active_token_ids);

        let join = tokio::spawn(async move {
            let mut attempt = 0u32;
            let mut is_reconnecting = false;
            loop {
                let connect_result = async {
                    let connect_fut = connect_async(&url);
                    match tokio::time::timeout(
                        Duration::from_secs(CONNECT_TIMEOUT_SECS),
                        connect_fut,
                    )
                    .await
                    {
                        Ok(Ok((ws_stream, _))) => Ok(ws_stream),
                        Ok(Err(e)) => Err(anyhow::anyhow!("{}", e)),
                        Err(_) => Err(anyhow::anyhow!(
                            "connection timeout after {}s",
                            CONNECT_TIMEOUT_SECS
                        )),
                    }
                }
                .await;

                let ws_stream = match connect_result {
                    Ok(s) => {
                        attempt = 0;
                        if is_reconnecting {
                            tracing::info!(
                                "[ClobWsUser] ✓ reconnected successfully (fills back to real-time WS)"
                            );
                        }
                        is_reconnecting = false;
                        s
                    }
                    Err(e) => {
                        attempt += 1;
                        let delay_ms = (500u64 * 2u64.pow(attempt.min(6))).min(30_000);
                        tracing::warn!(
                            "[ClobWsUser] reconnect attempt {} failed: {} — retrying in {}ms",
                            attempt,
                            e,
                            delay_ms
                        );
                        tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                        continue;
                    }
                };

                let (mut write, mut read) = ws_stream.split();

                let sub = serde_json::json!({
                    "auth": { "apiKey": api_key.clone(), "secret": secret.clone(), "passphrase": passphrase.clone() },
                    "markets": condition_ids.clone(),
                    "type": "user"
                });
                if write.send(Message::Text(sub.to_string())).await.is_err() {
                    tokio::time::sleep(Duration::from_millis(1000)).await;
                    continue;
                }

                let mut last_msg_at = std::time::Instant::now();
                let mut ping_interval = interval(Duration::from_secs(PING_INTERVAL_SECS));
                ping_interval.tick().await;
                let mut heartbeat = interval(Duration::from_secs(30));
                heartbeat.tick().await;

                loop {
                    tokio::select! {
                        _ = ping_interval.tick() => {
                            if write.send(Message::Ping(vec![])).await.is_err() {
                                tracing::warn!("[ClobWsUser] ping failed — reconnecting...");
                                is_reconnecting = true;
                                tracing::info!("[ClobWsUser] attempting to reconnect...");
                                break;
                            }
                        }
                        msg = read.next() => {
                            match msg {
                                Some(Ok(msg)) => {
                                    last_msg_at = std::time::Instant::now();
                                    match msg {
                                        Message::Text(text) => {
                                            if let Err(e) = Self::apply_message(&state_recv, &confirmed_buy_recv, &active_token_ids_recv, &text).await {
                                                let event_type = serde_json::from_str::<serde_json::Value>(&text)
                                                    .ok()
                                                    .and_then(|v| v.get("event_type").and_then(|e| e.as_str()).map(String::from))
                                                    .unwrap_or_default();
                                                if event_type.is_empty() {
                                                    tracing::debug!("ClobWsUser parse: {} | payload: {}", e, text.chars().take(300).collect::<String>());
                                                } else {
                                                    tracing::warn!("ClobWsUser parse error [{}]: {}", event_type, e);
                                                }
                                            }
                                        }
                                        Message::Pong(_) => {}
                                        _ => {}
                                    }
                                }
                                Some(Err(e)) => {
                                    tracing::warn!(
                                        "[ClobWsUser] connection error: {} — reconnecting (fills → REST until restored)",
                                        e
                                    );
                                    is_reconnecting = true;
                                    tracing::info!("[ClobWsUser] attempting to reconnect...");
                                    break;
                                }
                                None => {
                                    tracing::warn!(
                                        "[ClobWsUser] connection closed by server — reconnecting (fills → REST until restored)"
                                    );
                                    is_reconnecting = true;
                                    tracing::info!("[ClobWsUser] attempting to reconnect...");
                                    break;
                                }
                            }
                        }
                        _ = heartbeat.tick() => {
                            let silent_secs = last_msg_at.elapsed().as_secs();
                            if silent_secs > 30 {
                                tracing::warn!("[ClobWsUser] no messages in {}s — possible silent disconnect", silent_secs);
                            }
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        });

        Ok(Self { state, confirmed_buy, active_token_ids, _join: join })
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

    /// Set token IDs for the current market. [Blockchain] trade logs are emitted only for trades whose asset_id is in this set. Empty = log all trades.
    pub async fn set_active_token_ids(&self, ids: impl IntoIterator<Item = String>) {
        *self.active_token_ids.write().await = ids.into_iter().collect();
    }

    async fn apply_message(
        state: &RwLock<HashMap<String, UserOrderState>>,
        confirmed_buy: &RwLock<HashMap<String, Decimal>>,
        active_token_ids: &RwLock<HashSet<String>>,
        text: &str,
    ) -> Result<()> {
        let value: serde_json::Value = serde_json::from_str(text).context("parse JSON")?;
        let event_type = value
            .get("event_type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_lowercase();

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
                let mut map = state.write().await;
                // Never decrease size_matched: TRADE events may have already accumulated partial fills (e.g. 10+2=12);
                // if this ORDER UPDATE has a stale or partial size_matched, keep the higher value.
                let size_matched = if let Some(existing) = map.get(&key) {
                    size_matched.max(existing.size_matched)
                } else {
                    size_matched
                };
                let entry = UserOrderState {
                    order_id: key.clone(),
                    asset_id,
                    side,
                    original_size,
                    size_matched,
                    order_type: order_type.clone(),
                };
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
                let status = value
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_uppercase();

                let trade_id = value.get("id").and_then(|v| v.as_str()).unwrap_or("?");
                let price = value
                    .get("price")
                    .and_then(parse_decimal_value)
                    .unwrap_or(Decimal::ZERO);
                let timestamp = value
                    .get("timestamp")
                    .or_else(|| value.get("last_update"))
                    .and_then(|v| {
                        v.as_str()
                            .map(String::from)
                            .or_else(|| v.as_i64().map(|n| n.to_string()))
                            .or_else(|| v.as_u64().map(|n| n.to_string()))
                    })
                    .unwrap_or_else(|| "?".to_string());
                let should_log = {
                    let ids = active_token_ids.read().await;
                    ids.is_empty() || ids.contains(&asset_id)
                };
                if should_log {
                    tracing::info!(
                        "[Blockchain] trade {} → {} | {} {} @ {} | t={}",
                        trade_id,
                        status,
                        side,
                        trade_size,
                        price,
                        timestamp
                    );
                }

                // MATCHED = fill on exchange; use this as trigger for TP/SL placement (update_balance_allowance + backoff handles server cache).
                if status == "MATCHED" && side == "BUY" && !asset_id.is_empty() {
                    let mut confirmed = confirmed_buy.write().await;
                    let entry = confirmed.entry(asset_id.clone()).or_insert(Decimal::ZERO);
                    *entry += trade_size;
                    tracing::trace!(
                        "[ClobWsUser] MATCHED BUY asset_id={} +{} → confirmed_buy={}",
                        &asset_id[..asset_id.len().min(18)],
                        trade_size,
                        *entry
                    );
                }

                // Try taker_order_id first (our FAK/FOK orders), then maker_order_id (our GTC orders).
                let order_id = value.get("taker_order_id")
                    .or_else(|| value.get("maker_order_id"))
                    .and_then(|v| v.as_str())
                    .map(String::from);

                if let Some(oid) = order_id {
                    let key = normalize_order_id(&oid);
                    let mut map = state.write().await;
                    if let Some(existing) = map.get_mut(&key) {
                        // Accumulate: each trade event is one partial fill; Polymarket sends one event per fill (e.g. 10 then 2 → total 12).
                        // Cap by original_size when known so we never exceed order size and duplicate events don't double-count.
                        let new_matched = existing.size_matched + trade_size;
                        existing.size_matched = if existing.original_size > Decimal::ZERO {
                            new_matched.min(existing.original_size)
                        } else {
                            new_matched
                        };
                        tracing::trace!(
                            "[ClobWsUser] trade applied: order_id={} +{} → size_matched={}",
                            key,
                            trade_size,
                            existing.size_matched
                        );
                    } else {
                        // New order known only from this trade; don't set original_size so we don't cap future partials until we get an ORDER event.
                        tracing::trace!(
                            "[ClobWsUser] trade applied (new entry): order_id={} size={}",
                            key,
                            trade_size
                        );
                        map.insert(key.clone(), UserOrderState {
                            order_id: key,
                            asset_id,
                            side,
                            original_size: Decimal::ZERO,
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
    #[allow(dead_code)]
    pub async fn get_order_filled_size(&self, order_id: &str) -> Option<Decimal> {
        let key = normalize_order_id(order_id);
        let map = self.state.read().await;
        map.get(&key)
            .filter(|s| s.size_matched > Decimal::ZERO)
            .map(|s| s.size_matched)
    }

    /// Same as `get_order_filled_size` but also returns the event type ("TRADE" | "UPDATE" | "PLACEMENT" | ...).
    pub async fn get_order_filled_size_with_type(&self, order_id: &str) -> Option<(Decimal, String)> {
        let key = normalize_order_id(order_id);
        let map = self.state.read().await;
        map.get(&key)
            .filter(|s| s.size_matched > Decimal::ZERO)
            .map(|s| (s.size_matched, s.order_type.clone()))
    }

    /// Same as `get_order_filled_size_sell` but also returns the event type ("TRADE" | "UPDATE" | "CANCELLATION").
    pub async fn get_order_filled_size_sell_with_type(&self, order_id: &str) -> Option<(Decimal, String)> {
        let key = normalize_order_id(order_id);
        let map = self.state.read().await;
        map.get(&key)
            .filter(|s| s.size_matched > Decimal::ZERO)
            .filter(|s| {
                let is_sell = s.side == "SELL";
                let is_executed = s.order_type == "UPDATE"
                    || s.order_type == "TRADE"
                    || s.order_type == "CANCELLATION";
                is_sell && is_executed
            })
            .map(|s| (s.size_matched, s.order_type.clone()))
    }

    /// Return the filled size for an order only if it is a confirmed SELL fill.
    /// Requires side == "SELL" AND order has been executed (UPDATE/TRADE/CANCELLATION).
    /// PLACEMENT events (even with size_matched > 0) are excluded because they can be
    /// stale state from a previous session or an immediate-match that hasn't sent UPDATE yet —
    /// the exchange always follows an immediate match with a separate UPDATE/TRADE event.
    pub async fn get_order_filled_size_sell(&self, order_id: &str) -> Option<Decimal> {
        let key = normalize_order_id(order_id);
        let map = self.state.read().await;
        map.get(&key)
            .filter(|s| s.size_matched > Decimal::ZERO)
            .filter(|s| {
                // Must be a SELL order (not a BUY or unknown side).
                let is_sell = s.side == "SELL";
                // Must have a confirmed execution event — not just a PLACEMENT.
                let is_executed = s.order_type == "UPDATE"
                    || s.order_type == "TRADE"
                    || s.order_type == "CANCELLATION";
                is_sell && is_executed
            })
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
                // Only count actual fills, not resting placements
                buy_sum += s.size_matched;
            } else if s.side == "SELL" {
                // Only count SELL fills that actually executed.
                // Ignore resting GTC SELL orders (PLACEMENT with size_matched=0 or
                // orders where size_matched == original_size but order_type is still PLACEMENT).
                // A SELL is "executed" only if it has a real trade behind it.
                // We detect this by requiring order_type == "TRADE" or order_type == "UPDATE"
                // (Polymarket sends UPDATE when a GTC order partially or fully fills).
                // PLACEMENT events with size_matched > 0 are stale state from previous sessions
                // or phantom fills — exclude them.
                let is_executed = s.order_type == "TRADE"
                    || s.order_type == "UPDATE"
                    || s.order_type == "CANCELLATION"; // cancelled orders: size_matched is real fills before cancel
                if is_executed && s.size_matched > Decimal::ZERO {
                    sell_sum += s.size_matched;
                }
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
    #[allow(dead_code)]
    pub async fn get_order_state(&self, order_id: &str) -> Option<UserOrderState> {
        let key = normalize_order_id(order_id);
        let map = self.state.read().await;
        map.get(&key).cloned()
    }

    /// Remove all order state entries for a given asset_id.
    /// Call this when switching intervals so accumulated fills from previous
    /// intervals do not distort balance calculations for the new interval.
    pub async fn clear_token_state(&self, asset_id: &str) {
        let mut map = self.state.write().await;
        map.retain(|_, v| v.asset_id != asset_id);
        let mut confirmed = self.confirmed_buy.write().await;
        confirmed.remove(asset_id);
    }
}
