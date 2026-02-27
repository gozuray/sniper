//! CLOB WebSocket client for real-time order book (Polymarket).
//!
//! Connects to `wss://ws-subscriptions-clob.polymarket.com/ws/market`, subscribes to
//! asset IDs (token_id_up, token_id_down), and keeps a shared [TopOfBook] updated from
//! `book`, `best_bid_ask`, and `price_change` events. Send PING every 10s per docs.

use crate::types::{TopOfBook, TopOfBookSide};
use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use rust_decimal::Decimal;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::time::interval;
use tokio_tungstenite::{connect_async, tungstenite::Message};

/// Default CLOB WebSocket market endpoint (no auth).
pub const DEFAULT_WS_MARKET_URL: &str = "wss://ws-subscriptions-clob.polymarket.com/ws/market";

/// Heartbeat interval per Polymarket docs.
const PING_INTERVAL_SECS: u64 = 10;

/// WebSocket message: full book snapshot.
#[derive(Debug, serde::Deserialize)]
pub struct WsBookMessage {
    #[serde(rename = "event_type")]
    pub event_type: String,
    #[serde(rename = "asset_id")]
    pub asset_id: String,
    pub bids: Option<Vec<WsBookLevel>>,
    pub asks: Option<Vec<WsBookLevel>>,
}

#[derive(Debug, serde::Deserialize)]
pub struct WsBookLevel {
    pub price: String,
    pub size: String,
}

/// WebSocket message: best bid/ask only (custom_feature_enabled).
#[derive(Debug, serde::Deserialize)]
pub struct WsBestBidAskMessage {
    #[serde(rename = "event_type")]
    pub event_type: String,
    #[serde(rename = "asset_id")]
    pub asset_id: String,
    #[serde(rename = "best_bid")]
    pub best_bid: Option<String>,
    #[serde(rename = "best_ask")]
    pub best_ask: Option<String>,
}

/// WebSocket message: price change (best_bid/best_ask per asset).
#[derive(Debug, serde::Deserialize)]
pub struct WsPriceChangeMessage {
    #[serde(rename = "event_type")]
    pub event_type: String,
    #[serde(rename = "price_changes")]
    pub price_changes: Option<Vec<WsPriceChangeItem>>,
}

#[derive(Debug, serde::Deserialize)]
pub struct WsPriceChangeItem {
    #[serde(rename = "asset_id")]
    pub asset_id: String,
    #[serde(rename = "best_bid")]
    pub best_bid: Option<String>,
    #[serde(rename = "best_ask")]
    pub best_ask: Option<String>,
}

fn parse_decimal(s: &str) -> Option<Decimal> {
    Decimal::from_str(s.trim()).ok().filter(|d| !d.is_zero())
}

/// Build [TopOfBookSide] from WS book snapshot (bids/asks arrays).
fn book_to_side(bids: &[WsBookLevel], asks: &[WsBookLevel]) -> TopOfBookSide {
    let mut side = TopOfBookSide::default();
    let mut best_bid_price: Option<Decimal> = None;
    let mut best_bid_size: Option<Decimal> = None;
    for b in bids.iter() {
        if let (Some(p), Some(s)) = (parse_decimal(&b.price), parse_decimal(&b.size)) {
            if best_bid_price.map(|bp| p > bp).unwrap_or(true) {
                best_bid_price = Some(p);
                best_bid_size = Some(s);
            }
        }
    }
    side.best_bid = best_bid_price;
    side.best_bid_size = best_bid_size;

    let mut best_ask_price: Option<Decimal> = None;
    let mut best_ask_size: Option<Decimal> = None;
    for a in asks.iter() {
        if let (Some(p), Some(s)) = (parse_decimal(&a.price), parse_decimal(&a.size)) {
            if best_ask_price.map(|ap| p < ap).unwrap_or(true) {
                best_ask_price = Some(p);
                best_ask_size = Some(s);
            }
        }
    }
    side.best_ask = best_ask_price;
    side.best_ask_size = best_ask_size;
    side
}

/// Client for CLOB WebSocket order book. Holds shared [TopOfBook] updated in a background task.
pub struct ClobWsBook {
    /// Current top of book for both tokens; updated by the WS receive loop.
    state: Arc<RwLock<TopOfBook>>,
    _join: tokio::task::JoinHandle<()>,
}

impl ClobWsBook {
    /// Connect to the CLOB WebSocket, subscribe to the two token IDs, and start the receive + ping loop.
    /// Uses [DEFAULT_WS_MARKET_URL] if `ws_url` is empty.
    pub async fn connect(ws_url: &str, token_id_up: &str, token_id_down: &str) -> Result<Self> {
        let url = if ws_url.is_empty() {
            DEFAULT_WS_MARKET_URL
        } else {
            ws_url
        };
        let (ws_stream, _) = connect_async(url).await.context("CLOB WebSocket connect")?;

        let (mut write, mut read) = ws_stream.split();
        let state: Arc<RwLock<TopOfBook>> = Arc::new(RwLock::new(TopOfBook::default()));
        let state_recv = Arc::clone(&state);
        let token_id_up = token_id_up.to_string();
        let token_id_down = token_id_down.to_string();

        // Subscribe immediately (server may close if we don't).
        let sub = serde_json::json!({
            "assets_ids": [token_id_up.as_str(), token_id_down.as_str()],
            "type": "market",
            "custom_feature_enabled": true
        });
        write
            .send(Message::Text(sub.to_string()))
            .await
            .context("send subscribe")?;

        let join = tokio::spawn(async move {
            let mut ping_interval = interval(Duration::from_secs(PING_INTERVAL_SECS));
            ping_interval.tick().await; // first tick fires immediately, skip

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
                            if let Err(e) = Self::apply_message(&state_recv, &text, &token_id_up, &token_id_down).await {
                                tracing::debug!("ClobWsBook parse/apply: {} | payload: {}", e, text.chars().take(200).collect::<String>());
                            }
                        }
                    }
                }
            }
        });

        Ok(Self { state, _join: join })
    }

    /// Build WebSocket URL from REST CLOB host (e.g. https://clob.polymarket.com -> wss://ws-subscriptions-clob.polymarket.com/ws/market).
    pub fn ws_url_from_rest_host(rest_host: &str) -> String {
        let rest = rest_host.trim_end_matches('/');
        if rest.starts_with("https://clob.polymarket.com")
            || rest.starts_with("http://clob.polymarket.com")
        {
            DEFAULT_WS_MARKET_URL.to_string()
        } else if rest.contains("clob.polymarket.com") {
            DEFAULT_WS_MARKET_URL.to_string()
        } else {
            DEFAULT_WS_MARKET_URL.to_string()
        }
    }

    async fn apply_message(
        state: &RwLock<TopOfBook>,
        text: &str,
        token_id_up: &str,
        token_id_down: &str,
    ) -> Result<()> {
        let value: serde_json::Value = serde_json::from_str(text).context("parse JSON")?;
        let event_type = value
            .get("event_type")
            .and_then(|v| v.as_str())
            .unwrap_or("");

        match event_type {
            "book" => {
                let msg: WsBookMessage = serde_json::from_str(text).context("parse book")?;
                let bids = msg.bids.as_deref().unwrap_or(&[]);
                let asks = msg.asks.as_deref().unwrap_or(&[]);
                let side = book_to_side(bids, asks);
                let mut book = state.write().await;
                if msg.asset_id == *token_id_up {
                    book.token_id_up = Some(side);
                } else if msg.asset_id == *token_id_down {
                    book.token_id_down = Some(side);
                }
            }
            "best_bid_ask" => {
                let msg: WsBestBidAskMessage =
                    serde_json::from_str(text).context("parse best_bid_ask")?;
                let best_bid = msg.best_bid.as_deref().and_then(parse_decimal);
                let best_ask = msg.best_ask.as_deref().and_then(parse_decimal);
                let mut book = state.write().await;
                if msg.asset_id == *token_id_up {
                    let up = book.token_id_up.get_or_insert_with(TopOfBookSide::default);
                    if best_bid.is_some() {
                        up.best_bid = best_bid;
                    }
                    if best_ask.is_some() {
                        up.best_ask = best_ask;
                    }
                } else if msg.asset_id == *token_id_down {
                    let down = book
                        .token_id_down
                        .get_or_insert_with(TopOfBookSide::default);
                    if best_bid.is_some() {
                        down.best_bid = best_bid;
                    }
                    if best_ask.is_some() {
                        down.best_ask = best_ask;
                    }
                }
            }
            "price_change" => {
                let msg: WsPriceChangeMessage =
                    serde_json::from_str(text).context("parse price_change")?;
                let Some(ref changes) = msg.price_changes else {
                    return Ok(());
                };
                let mut book = state.write().await;
                for c in changes.iter() {
                    let best_bid = c.best_bid.as_deref().and_then(parse_decimal);
                    let best_ask = c.best_ask.as_deref().and_then(parse_decimal);
                    if c.asset_id == *token_id_up {
                        let up = book.token_id_up.get_or_insert_with(TopOfBookSide::default);
                        if best_bid.is_some() {
                            up.best_bid = best_bid;
                        }
                        if best_ask.is_some() {
                            up.best_ask = best_ask;
                        }
                    } else if c.asset_id == *token_id_down {
                        let down = book
                            .token_id_down
                            .get_or_insert_with(TopOfBookSide::default);
                        if best_bid.is_some() {
                            down.best_bid = best_bid;
                        }
                        if best_ask.is_some() {
                            down.best_ask = best_ask;
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Return a copy of the current top of book (both tokens).
    pub async fn get_top_of_book(&self) -> TopOfBook {
        self.state.read().await.clone()
    }
}
