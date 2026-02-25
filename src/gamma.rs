//! Resolve BTC 5-min market token_id from Polymarket Gamma API.
//! Slug pattern: btc-updown-5m-{window_start_unix}, window every 300s.

use anyhow::{Context, Result};
use serde::Deserialize;

const GAMMA_API_BASE: &str = "https://gamma-api.polymarket.com";

/// Gamma API market response (subset we need).
#[derive(Debug, Deserialize)]
struct GammaMarket {
    #[serde(rename = "clobTokenIds")]
    clob_token_ids: String, // JSON array as string: "[\"id1\", \"id2\"]"
}

/// Compute current 5-minute window start (Unix seconds).
/// Windows are aligned to 0, 300, 600, ...
#[inline]
pub fn current_window_start_unix() -> u64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time before Unix epoch");
    (now.as_secs() / 300) * 300
}

/// Seconds from now until the current window ends (window_start + 300).
pub fn secs_until_window_end() -> u64 {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system time before Unix epoch");
    let window_start = (now.as_secs() / 300) * 300;
    let window_end = window_start + 300;
    window_end.saturating_sub(now.as_secs())
}

/// Slug for the BTC 5-min market for the given window start (Unix seconds).
pub fn btc5m_slug(window_start: u64) -> String {
    format!("btc-updown-5m-{window_start}")
}

/// Fetch token_id for the given slug. Outcome: true = Up (first token), false = Down (second).
pub async fn fetch_token_id(slug: &str, outcome_up: bool) -> Result<String> {
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
    ids.get(index)
        .cloned()
        .with_context(|| format!("clobTokenIds missing index {index} for slug {slug}"))
}
