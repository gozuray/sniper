use crate::config::{Config, SignatureType as LocalSignatureType};
use crate::types::{Price, TokenId};
use anyhow::{Context, Result};
use futures_util::StreamExt;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;

use polymarket_client_sdk::auth::{LocalSigner, Signer};
use polymarket_client_sdk::auth::state::Authenticated;
use polymarket_client_sdk::auth::Normal;
use polymarket_client_sdk::clob::ws::types::response::TradeMessage;
use polymarket_client_sdk::clob::ws::{Client as WsClient, WsMessage};
use polymarket_client_sdk::clob::Config as ClobConfig;
use polymarket_client_sdk::clob::types::{OrderType, SignatureType as SdkSignatureType};
use polymarket_client_sdk::clob::Client as ClobRestClient;
use polymarket_client_sdk::{POLYGON};
use polymarket_client_sdk::types::{Address, B256};

/// Best bid/ask snapshot for a fixed token set.
#[derive(Debug)]
pub struct OrderbookTop {
    index: HashMap<TokenId, usize>,
    best_bid_price: Vec<Price>,
    best_bid_size: Vec<Price>,
    best_ask_price: Vec<Price>,
    best_ask_size: Vec<Price>,
    has_bid: Vec<bool>,
    has_ask: Vec<bool>,
    last_timestamp: i64,
}

impl OrderbookTop {
    pub fn new(token_ids: &[TokenId]) -> Self {
        let index = token_ids
            .iter()
            .cloned()
            .enumerate()
            .map(|(i, id)| (id, i))
            .collect::<HashMap<_, _>>();
        let n = token_ids.len();
        Self {
            index,
            best_bid_price: vec![Price::ZERO; n],
            best_bid_size: vec![Price::ZERO; n],
            best_ask_price: vec![Price::ZERO; n],
            best_ask_size: vec![Price::ZERO; n],
            has_bid: vec![false; n],
            has_ask: vec![false; n],
            last_timestamp: 0,
        }
    }

    pub fn best_ask(&self, token: TokenId) -> Option<(Price, Price)> {
        let &i = self.index.get(&token)?;
        if !self.has_ask[i] {
            None
        } else {
            Some((self.best_ask_price[i], self.best_ask_size[i]))
        }
    }

    pub fn best_bid(&self, token: TokenId) -> Option<(Price, Price)> {
        let &i = self.index.get(&token)?;
        if !self.has_bid[i] {
            None
        } else {
            Some((self.best_bid_price[i], self.best_bid_size[i]))
        }
    }

    #[inline]
    pub fn tracks_token(&self, token: TokenId) -> bool {
        self.index.contains_key(&token)
    }

    /// Last book WS `timestamp` (any subscribed asset). Kept for diagnostics.
    #[inline]
    #[allow(dead_code)]
    pub fn last_book_event_ts_ms(&self) -> i64 {
        self.last_timestamp
    }

    fn update_from_book(&mut self, book: &polymarket_client_sdk::clob::ws::types::response::BookUpdate) {
        let asset_id = book.asset_id;
        let Some(&i) = self.index.get(&asset_id) else { return };

        self.last_timestamp = book.timestamp;
        // No usar solo `.first()`: el orden real de bids/asks en el wire no siempre coincide con la
        // descripción del SDK (y hay deltas). Mejor bid = mayor precio con tamaño > 0; mejor ask = menor.
        if let Some(b) = book
            .bids
            .iter()
            .filter(|l| !l.size.is_zero())
            .max_by_key(|l| l.price)
        {
            self.best_bid_price[i] = b.price;
            self.best_bid_size[i] = b.size;
            self.has_bid[i] = true;
        } else {
            self.has_bid[i] = false;
        }
        if let Some(a) = book
            .asks
            .iter()
            .filter(|l| !l.size.is_zero())
            .min_by_key(|l| l.price)
        {
            self.best_ask_price[i] = a.price;
            self.best_ask_size[i] = a.size;
            self.has_ask[i] = true;
        } else {
            self.has_ask[i] = false;
        }
    }
}

/// Map local SignatureType to SDK SignatureType.
fn map_signature_type(t: &LocalSignatureType) -> SdkSignatureType {
    match t {
        LocalSignatureType::Eoa => SdkSignatureType::Eoa,
        LocalSignatureType::Proxy => SdkSignatureType::Proxy,
        LocalSignatureType::GnosisSafe => SdkSignatureType::GnosisSafe,
    }
}

/// Live REST + auth context.
pub struct LivePolymarket {
    rest: ClobRestClient<Authenticated<Normal>>,
    private_key: String,
    credentials: polymarket_client_sdk::auth::Credentials,
    address: Address,
}

impl LivePolymarket {
    pub async fn connect(cfg: &Config) -> Result<Self> {
        let pk = cfg.private_key_polygon.as_deref().context("private_key_polygon missing")?;
        let signer = LocalSigner::from_str(pk)?.with_chain_id(Some(POLYGON));

        let rest = ClobRestClient::new(cfg.clob_base_url(), ClobConfig::default())?
            .authentication_builder(&signer)
            .signature_type(map_signature_type(&cfg.signature_type))
            .authenticate()
            .await?;

        let credentials = rest.credentials().clone();
        let address = rest.address();

        Ok(Self {
            rest,
            private_key: pk.to_string(),
            credentials,
            address,
        })
    }

    pub fn credentials(&self) -> &polymarket_client_sdk::auth::Credentials {
        &self.credentials
    }

    pub fn address(&self) -> Address {
        self.address
    }

    pub async fn place_limit_buy(
        &self,
        token_id: TokenId,
        price: Price,
        size: Price,
        order_type: OrderType,
        post_only: bool,
    ) -> Result<String> {
        let order = self
            .rest
            .limit_order()
            .token_id(token_id)
            .order_type(order_type)
            .price(price)
            .size(size)
            .side(polymarket_client_sdk::clob::types::Side::Buy)
            .post_only(post_only)
            .build()
            .await?;

        let signer = LocalSigner::from_str(&self.private_key)?.with_chain_id(Some(POLYGON));
        let signed = self.rest.sign(&signer, order).await?;
        let resp = self.rest.post_order(signed).await?;
        Ok(resp.order_id)
    }

    pub async fn place_limit_sell(
        &self,
        token_id: TokenId,
        price: Price,
        size: Price,
        order_type: OrderType,
        post_only: bool,
    ) -> Result<String> {
        let order = self
            .rest
            .limit_order()
            .token_id(token_id)
            .order_type(order_type)
            .price(price)
            .size(size)
            .side(polymarket_client_sdk::clob::types::Side::Sell)
            .post_only(post_only)
            .build()
            .await?;

        let signer = LocalSigner::from_str(&self.private_key)?.with_chain_id(Some(POLYGON));
        let signed = self.rest.sign(&signer, order).await?;
        let resp = self.rest.post_order(signed).await?;
        Ok(resp.order_id)
    }

    pub async fn cancel_order(&self, order_id: &str) -> Result<()> {
        let _ = self.rest.cancel_order(order_id).await?;
        Ok(())
    }
}

/// Spawn orderbook WS (public) and continuously update `OrderbookTop`.
pub fn spawn_orderbook_ws(
    shutdown: CancellationToken,
    token_ids: Vec<TokenId>,
    state: Arc<RwLock<OrderbookTop>>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let client = WsClient::default();
        let mut attempt: u32 = 0;

        loop {
            if shutdown.is_cancelled() {
                return;
            }

            match client.subscribe_orderbook(token_ids.clone()) {
                Ok(stream) => {
                    attempt = 0;
                    let mut stream = Box::pin(stream);
                    while let Some(msg) = stream.next().await {
                        if shutdown.is_cancelled() {
                            return;
                        }
                        match msg {
                            Ok(book) => {
                                let mut guard = state.write().await;
                                guard.update_from_book(&book);
                            }
                            Err(e) => {
                                tracing::warn!(error = %e, "orderbook ws error; reconnecting");
                                break;
                            }
                        }
                    }
                }
                Err(e) => {
                    attempt = attempt.saturating_add(1);
                    tracing::warn!(error = %e, attempt = attempt, "orderbook ws subscribe failed");
                }
            }

            let delay_ms = 250u64.saturating_mul(2u64.pow(attempt.min(6)));
            sleep(Duration::from_millis(delay_ms.min(30_000))).await;
        }
    })
}

/// Spawn authenticated user trade WS and forward trade messages to `out`.
///
/// Note: we subscribe to all markets by passing an empty `Vec<B256>`.
pub fn spawn_user_trade_ws(
    shutdown: CancellationToken,
    credentials: polymarket_client_sdk::auth::Credentials,
    address: Address,
    out: tokio::sync::mpsc::UnboundedSender<TradeMessage>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut attempt: u32 = 0;
        loop {
            if shutdown.is_cancelled() {
                return;
            }

            let client = WsClient::default();
            let ws = match client.authenticate(credentials.clone(), address) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error = %e, attempt = attempt, "ws auth failed; retrying");
                    attempt = attempt.saturating_add(1);
                    let delay_ms = 250u64.saturating_mul(2u64.pow(attempt.min(6)));
                    sleep(Duration::from_millis(delay_ms.min(30_000))).await;
                    continue;
                }
            };

            match ws.subscribe_user_events(Vec::<B256>::new()) {
                Ok(stream) => {
                    attempt = 0;
                    let mut stream = Box::pin(stream);
                    while let Some(msg) = stream.next().await {
                        if shutdown.is_cancelled() {
                            return;
                        }
                        match msg {
                            Ok(WsMessage::Trade(trade)) => {
                                let _ = out.send(trade);
                            }
                            Ok(WsMessage::Order(_)) => {
                                // Orders are not needed if we track fills using trades.
                            }
                            Ok(_) => {}
                            Err(e) => {
                                tracing::warn!(error = %e, "user ws error; reconnecting");
                                break;
                            }
                        }
                    }
                }
                Err(e) => {
                    attempt = attempt.saturating_add(1);
                    tracing::warn!(error = %e, attempt = attempt, "ws subscribe_user_events failed");
                }
            }

            let delay_ms = 250u64.saturating_mul(2u64.pow(attempt.min(6)));
            sleep(Duration::from_millis(delay_ms.min(30_000))).await;
        }
    })
}

