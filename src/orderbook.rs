//! Order book via CLOB REST (GET /book?token_id=...). Builds TopOfBook for both tokens.

use crate::types::{OrderBookRaw, TopOfBook, TopOfBookSide};
use anyhow::Result;
use reqwest::Client;
use rust_decimal::Decimal;
use std::str::FromStr;

/// Fetch order book for one token (no auth required).
pub async fn fetch_order_book(
    client: &Client,
    clob_host: &str,
    token_id: &str,
) -> Result<OrderBookRaw> {
    let base = clob_host.trim_end_matches('/');
    let url = format!("{}/book?token_id={}", base, urlencoding::encode(token_id));
    let res = client
        .get(&url)
        .header("Accept", "application/json")
        .send()
        .await?;
    if !res.status().is_success() {
        let status = res.status();
        let text = res.text().await.unwrap_or_default();
        anyhow::bail!(
            "CLOB {}: {}",
            status,
            text.chars().take(200).collect::<String>()
        );
    }
    let raw: OrderBookRaw = res.json().await?;
    Ok(raw)
}

/// Parse one level price/size to Decimal.
fn parse_level_price_size(price: &str, size: &str) -> Option<(Decimal, Decimal)> {
    let p = Decimal::from_str(price).ok()?;
    let s = Decimal::from_str(size).ok()?;
    if p.is_zero() || s.is_zero() {
        return None;
    }
    Some((p, s))
}

/// Build TopOfBookSide from raw order book.
/// Best bid = highest bid price; best ask = lowest ask price (robust to API sort order).
fn raw_to_side(raw: &OrderBookRaw) -> TopOfBookSide {
    let mut side = TopOfBookSide::default();

    if let Some(ref bids) = raw.bids {
        let mut best_bid_price: Option<Decimal> = None;
        let mut best_bid_size: Option<Decimal> = None;
        for b in bids.iter() {
            if let Some((p, s)) = parse_level_price_size(&b.price, &b.size) {
                if best_bid_price.map(|bp| p > bp).unwrap_or(true) {
                    best_bid_price = Some(p);
                    best_bid_size = Some(s);
                }
            }
        }
        side.best_bid = best_bid_price;
        side.best_bid_size = best_bid_size;
    }
    if let Some(ref asks) = raw.asks {
        let mut best_ask_price: Option<Decimal> = None;
        let mut best_ask_size: Option<Decimal> = None;
        for a in asks.iter() {
            if let Some((p, s)) = parse_level_price_size(&a.price, &a.size) {
                if best_ask_price.map(|ap| p < ap).unwrap_or(true) {
                    best_ask_price = Some(p);
                    best_ask_size = Some(s);
                }
            }
        }
        side.best_ask = best_ask_price;
        side.best_ask_size = best_ask_size;
    }
    side
}

/// Fetch order books for both tokens and return TopOfBook.
pub async fn fetch_top_of_book(
    client: &Client,
    clob_host: &str,
    token_id_up: &str,
    token_id_down: &str,
) -> Result<TopOfBook> {
    let (up_raw, down_raw) = tokio::join!(
        fetch_order_book(client, clob_host, token_id_up),
        fetch_order_book(client, clob_host, token_id_down),
    );
    let up_raw = up_raw?;
    let down_raw = down_raw?;

    Ok(TopOfBook {
        token_id_up: Some(raw_to_side(&up_raw)),
        token_id_down: Some(raw_to_side(&down_raw)),
    })
}

/// Min order size from raw book (default 5 if missing).
pub fn min_order_size_from_raw(raw: &OrderBookRaw) -> Decimal {
    raw.min_order_size
        .as_ref()
        .and_then(|s| Decimal::from_str(s.as_str()).ok())
        .unwrap_or(Decimal::from(5))
}
