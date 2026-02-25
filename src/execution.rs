use anyhow::Result;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use polymarket_client_sdk::auth::state::Authenticated;
use polymarket_client_sdk::auth::{Normal, Signer as SignerTrait};
use polymarket_client_sdk::clob::types::request::OrderBookSummaryRequest;
use polymarket_client_sdk::clob::types::response::PostOrderResponse;
use polymarket_client_sdk::clob::types::{OrderStatusType, OrderType, Side};
use ruint::Uint;

type U256 = Uint<256, 4>;

// ── Result types ───────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
pub enum FillStatus {
    FullyFilled,
    PartiallyFilled,
    NotFilled,
    Placed,
}

#[derive(Debug, Clone)]
pub struct OrderResult {
    pub order_id: String,
    pub filled_size: Decimal,
    pub status: FillStatus,
}

#[derive(Debug)]
pub struct BookSnapshot {
    pub best_bid: Option<Decimal>,
    pub best_ask: Option<Decimal>,
}

// ── Executor (generic over signer) ────────────────────────────────

pub struct Executor<S> {
    client: polymarket_client_sdk::clob::Client<Authenticated<Normal>>,
    signer: S,
    token_id: U256,
}

impl<S: SignerTrait + Send + Sync> Executor<S> {
    pub fn new(
        client: polymarket_client_sdk::clob::Client<Authenticated<Normal>>,
        signer: S,
        token_id: U256,
    ) -> Self {
        Self {
            client,
            signer,
            token_id,
        }
    }

    // ── Sell FAK (for SL) ──────────────────────────────────────────

    pub async fn sell_fak(
        &self,
        size: Decimal,
        limit_price: Decimal,
    ) -> Result<OrderResult> {
        tracing::info!(size = %size, limit = %limit_price, "sending SL sell FAK");
        let resp = self
            .place_limit_sell(size, limit_price, OrderType::FAK)
            .await?;
        Ok(classify_sell_response(resp, size))
    }

    // ── Sell FOK (for TP — immediate or cancel) ────────────────────

    pub async fn sell_limit(
        &self,
        size: Decimal,
        limit_price: Decimal,
    ) -> Result<OrderResult> {
        tracing::info!(size = %size, limit = %limit_price, "sending TP sell FOK");
        let resp = self
            .place_limit_sell(size, limit_price, OrderType::FOK)
            .await?;
        Ok(classify_sell_response(resp, size))
    }

    // ── Buy limit (GTC, rests on book) ─────────────────────────────

    pub async fn buy_limit(
        &self,
        size: Decimal,
        price: Decimal,
    ) -> Result<OrderResult> {
        tracing::info!(size = %size, price = %price, "placing buy limit GTC");

        let order = self
            .client
            .limit_order()
            .token_id(self.token_id)
            .size(size)
            .price(price)
            .side(Side::Buy)
            .build()
            .await?;

        let signed = self.client.sign(&self.signer, order).await?;
        let resp = self.client.post_order(signed).await?;
        Ok(classify_buy_response(resp, size))
    }

    // ── Cancel ─────────────────────────────────────────────────────

    pub async fn cancel_order(&self, order_id: &str) -> Result<()> {
        tracing::info!(order_id, "cancelling order");
        self.client.cancel_order(order_id).await?;
        Ok(())
    }

    // ── GET /book (REST fallback for stale book) ───────────────────

    pub async fn get_book(&self) -> Result<BookSnapshot> {
        tracing::debug!("REST: fetching order book");

        let request = OrderBookSummaryRequest::builder()
            .token_id(self.token_id)
            .build();
        let book = self.client.order_book(&request).await?;

        // No asumir orden del API: best bid = mayor precio en bids, best ask = menor precio en asks
        let best_bid = book.bids.iter().map(|l| l.price).max();
        let best_ask = book.asks.iter().map(|l| l.price).min();

        if book.bids.is_empty() || book.asks.is_empty() {
            tracing::debug!(
                bids_len = book.bids.len(),
                asks_len = book.asks.len(),
                "REST book tiene bids o asks vacíos"
            );
        } else {
            let top_bids: Vec<_> = book.bids.iter().take(3).map(|l| (l.price, l.size)).collect();
            let top_asks: Vec<_> = book.asks.iter().take(3).map(|l| (l.price, l.size)).collect();
            tracing::debug!(
                best_bid = ?best_bid,
                best_ask = ?best_ask,
                top_bids = ?top_bids,
                top_asks = ?top_asks,
                "REST book snapshot"
            );
        }

        Ok(BookSnapshot { best_bid, best_ask })
    }

    // ── Internal ───────────────────────────────────────────────────

    async fn place_limit_sell(
        &self,
        size: Decimal,
        limit_price: Decimal,
        order_type: OrderType,
    ) -> Result<PostOrderResponse> {
        let order = self
            .client
            .limit_order()
            .token_id(self.token_id)
            .size(size)
            .price(limit_price)
            .side(Side::Sell)
            .order_type(order_type)
            .build()
            .await?;

        let signed = self.client.sign(&self.signer, order).await?;
        Ok(self.client.post_order(signed).await?)
    }
}

// ── Response classification ────────────────────────────────────────

/// For SELL orders, `making_amount` = shares sold.
fn classify_sell_response(resp: PostOrderResponse, requested_size: Decimal) -> OrderResult {
    let filled = resp.making_amount;

    let status = match resp.status {
        OrderStatusType::Matched if filled >= requested_size => FillStatus::FullyFilled,
        OrderStatusType::Matched if filled > dec!(0) => FillStatus::PartiallyFilled,
        OrderStatusType::Matched => FillStatus::FullyFilled,
        OrderStatusType::Unmatched => FillStatus::NotFilled,
        OrderStatusType::Live => FillStatus::Placed,
        _ => {
            if filled > dec!(0) {
                FillStatus::PartiallyFilled
            } else {
                FillStatus::NotFilled
            }
        }
    };

    tracing::info!(
        order_id = %resp.order_id,
        sdk_status = ?resp.status,
        making = %resp.making_amount,
        taking = %resp.taking_amount,
        success = resp.success,
        fill_status = ?status,
        "sell order response"
    );

    OrderResult {
        order_id: resp.order_id,
        filled_size: filled,
        status,
    }
}

/// For BUY orders, `taking_amount` = shares received.
fn classify_buy_response(resp: PostOrderResponse, requested_size: Decimal) -> OrderResult {
    let filled = resp.taking_amount;

    let status = match resp.status {
        OrderStatusType::Matched if filled >= requested_size => FillStatus::FullyFilled,
        OrderStatusType::Matched if filled > dec!(0) => FillStatus::PartiallyFilled,
        OrderStatusType::Matched => FillStatus::FullyFilled,
        OrderStatusType::Live => FillStatus::Placed,
        OrderStatusType::Unmatched => FillStatus::NotFilled,
        _ => {
            if filled > dec!(0) {
                FillStatus::PartiallyFilled
            } else {
                FillStatus::Placed
            }
        }
    };

    tracing::info!(
        order_id = %resp.order_id,
        sdk_status = ?resp.status,
        making = %resp.making_amount,
        taking = %resp.taking_amount,
        success = resp.success,
        fill_status = ?status,
        "buy order response"
    );

    OrderResult {
        order_id: resp.order_id,
        filled_size: filled,
        status,
    }
}
