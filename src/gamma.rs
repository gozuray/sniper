//! Resolve BTC 5-min market token_id from Polymarket Gamma API.
//! Slug pattern: btc-updown-5m-{window_start_unix}. Interval = 300s.
//! Bot operates on the *active* interval (current 5-min window) so we trade the live market.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Deserialize;

const GAMMA_API_BASE: &str = "https://gamma-api.polymarket.com";

/// 5-minute interval in seconds (used for close-time math).
pub const BTC_5MIN_INTERVAL_SEC: u64 = 300;

/// Current 5-min interval *close* time (Unix seconds): ceil(now_sec / 300) * 300.
#[inline]
pub fn get_current_btc_5min_close_time_unix() -> u64 {
    let now_sec = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time before Unix epoch")
        .as_secs();
    ((now_sec + BTC_5MIN_INTERVAL_SEC - 1) / BTC_5MIN_INTERVAL_SEC) * BTC_5MIN_INTERVAL_SEC
}

/// Previous 5-min interval close time: current - 300.
#[inline]
pub fn get_previous_btc_5min_close_time_unix() -> u64 {
    get_current_btc_5min_close_time_unix()
        .saturating_sub(BTC_5MIN_INTERVAL_SEC)
}

/// Current time as Unix seconds (for interval checks).
#[inline]
pub fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time before Unix epoch")
        .as_secs()
}

/// Start of the current 5-min window (floor(now/300)*300). This is the number used in Polymarket slugs.
#[inline]
pub fn current_window_start_unix() -> u64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time before Unix epoch");
    (now.as_secs() / BTC_5MIN_INTERVAL_SEC) * BTC_5MIN_INTERVAL_SEC
}

/// Slug for the *active* 5-min market (the window we're currently in). Uses window start so we subscribe to the correct live market.
/// Polymarket URLs: 12:25-12:30 ET = btc-updown-5m-1771997100, 12:30-12:35 ET = btc-updown-5m-1771997400 (cada +300s).
pub fn get_active_5min_slug() -> String {
    format!("btc-updown-5m-{}", current_window_start_unix())
}

/// Slug for the market we trade: the *previous* interval (legacy; prefer get_active_5min_slug for active interval).
pub fn get_previous_5min_slug() -> String {
    format!("btc-updown-5m-{}", get_previous_btc_5min_close_time_unix())
}

/// Slug for the *current* interval (for reference / next-slug when interval just closed).
pub fn get_current_5min_slug() -> String {
    format!("btc-updown-5m-{}", get_current_btc_5min_close_time_unix())
}

/// Seconds from now until the current 5-min window ends.
pub fn secs_until_window_end() -> u64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time before Unix epoch");
    let close = get_current_btc_5min_close_time_unix();
    close.saturating_sub(now.as_secs())
}

/// Legacy: slug for a given window start. Prefer get_active_5min_slug() for dynamic operation.
pub fn btc5m_slug(window_start: u64) -> String {
    format!("btc-updown-5m-{window_start}")
}

/// Gamma API market response (subset we need).
#[derive(Debug, Deserialize)]
struct GammaMarket {
    #[serde(rename = "clobTokenIds")]
    clob_token_ids: String, // JSON array as string: "[\"id1\", \"id2\"]"
    #[serde(rename = "endDate")]
    end_date: Option<String>, // ISO 8601 date-time
}

/// Info returned when fetching a market by slug (token_id + close time for interval switch).
#[derive(Debug, Clone)]
pub struct MarketInfo {
    pub token_id: String,
    pub slug: String,
    /// Unix seconds when this market closes (from Gamma endDate). None if not available.
    pub close_time_unix: Option<u64>,
}

/// Fetch market info for the given slug. Outcome: true = Up (first token), false = Down (second).
pub async fn fetch_market_info(slug: &str, outcome_up: bool) -> Result<MarketInfo> {
    let url = format!("{GAMMA_API_BASE}/markets/slug/{slug}");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .context("build HTTP client")?;

    let market: GammaMarket = client
        .get(&url)
        .send()
        .await
        .context("Gamma API request")?
        .error_for_status()
        .context("Gamma API error status")?
        .json()
        .await
        .context("Gamma API JSON")?;

    let ids: Vec<String> = serde_json::from_str(&market.clob_token_ids)
        .context("parse clobTokenIds JSON")?;

    let index = if outcome_up { 0 } else { 1 };
    let token_id = ids
        .get(index)
        .cloned()
        .with_context(|| format!("clobTokenIds missing index {index} for slug {slug}"))?;

    let close_time_unix = market
        .end_date
        .as_deref()
        .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.with_timezone(&Utc).timestamp() as u64);

    Ok(MarketInfo {
        token_id,
        slug: slug.to_string(),
        close_time_unix,
    })
}

/// Fetch token_id for the given slug. Outcome: true = Up (first token), false = Down (second).
/// Prefer fetch_market_info when you need close_time_unix for interval switching.
pub async fn fetch_token_id(slug: &str, outcome_up: bool) -> Result<String> {
    let info = fetch_market_info(slug, outcome_up).await?;
    Ok(info.token_id)
}

/// Fetch market info for both outcomes (Up and Down) in one request.
/// Returns (up_info, down_info) for the same slug so both sides can be scanned and traded.
pub async fn fetch_both_market_infos(slug: &str) -> Result<(MarketInfo, MarketInfo)> {
    let up = fetch_market_info(slug, true).await?;
    let down = fetch_market_info(slug, false).await?;
    Ok((up, down))
}
