//! Resolve BTC/SOL 5m market from Gamma API (slug -> token_id_up, token_id_down, close_time_unix).

use crate::types::{GammaEvent, GammaMarket, ResolvedMarket};
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use reqwest::Client;

const FIVE_MIN_SECONDS: u64 = 300;

/// Fetch market by slug: tries /markets/slug/{slug} first, then /events/slug/{slug} on 404.
pub async fn fetch_market_by_slug(
    client: &Client,
    base_url: &str,
    slug: &str,
) -> Result<ResolvedMarket> {
    let base = base_url.trim_end_matches('/');
    let market_url = format!("{}/markets/slug/{}", base, urlencoding::encode(slug));

    let res = client
        .get(&market_url)
        .header("user-agent", "polybot-interval-sniper-rust")
        .send()
        .await
        .context("Gamma API request")?;

    let m = if res.status() == 404 {
        let event_url = format!("{}/events/slug/{}", base, urlencoding::encode(slug));
        let event_res = client
            .get(&event_url)
            .header("user-agent", "polybot-interval-sniper-rust")
            .send()
            .await
            .context("Gamma API event request")?;
        let event: GammaEvent = event_res.json().await.context("Gamma event JSON")?;
        event
            .markets
            .and_then(|v| v.into_iter().next())
            .context("Event has no markets")?
    } else {
        res.json::<GammaMarket>()
            .await
            .context("Gamma market JSON")?
    };

    parse_gamma_market(&m, slug)
}

fn parse_gamma_market(m: &GammaMarket, slug: &str) -> Result<ResolvedMarket> {
    let condition_id = m
        .condition_id
        .as_deref()
        .or(m.id.as_deref())
        .unwrap_or("")
        .trim()
        .to_string();
    if condition_id.is_empty() {
        anyhow::bail!("Market slug \"{}\" has no conditionId", slug);
    }

    let end_date_str = m
        .end_date
        .as_deref()
        .filter(|s| s.contains('T'))
        .or(m.end_date_iso.as_deref())
        .or(m.end_date.as_deref())
        .unwrap_or("");
    let close_time_unix = parse_end_date_to_unix(end_date_str)?;
    let interval_start_unix = close_time_unix.saturating_sub(FIVE_MIN_SECONDS);

    let (token_id_up, token_id_down) = parse_token_ids(m)?;

    Ok(ResolvedMarket {
        slug: slug.to_string(),
        condition_id,
        close_time_unix,
        interval_start_unix,
        token_id_up,
        token_id_down,
    })
}

fn parse_end_date_to_unix(s: &str) -> Result<u64> {
    if s.is_empty() {
        anyhow::bail!("Market has no endDate/endDateIso");
    }
    let s = s.trim();
    if let Ok(t) = s.parse::<u64>() {
        return Ok(t);
    }
    let dt = DateTime::parse_from_rfc3339(s)
        .or_else(|_| DateTime::parse_from_rfc2822(s))
        .context("Invalid endDate")?;
    Ok(dt.with_timezone(&Utc).timestamp().max(0) as u64)
}

fn parse_token_ids(m: &GammaMarket) -> Result<(String, String)> {
    let mut token_id_up = String::new();
    let mut token_id_down = String::new();

    // Prefer tokens with outcome labels (Up/Down, Yes/No) so we don't rely on array order.
    if let Some(ref tokens) = m.tokens {
        if tokens.len() >= 2 {
            for t in tokens {
                let id = t.token_id.as_deref().unwrap_or("").trim().to_string();
                if id.is_empty() {
                    continue;
                }
                let outcome = t.outcome.as_deref().unwrap_or("").to_lowercase();
                if outcome == "up" || outcome == "yes" {
                    token_id_up = id;
                } else if outcome == "down" || outcome == "no" {
                    token_id_down = id;
                }
            }
            if token_id_up.is_empty() || token_id_down.is_empty() {
                token_id_up.clear();
                token_id_down.clear();
            }
        }
    }

    // Fallback: use clob_token_ids order (documented as [Yes, No] = [Up, Down]).
    if token_id_up.is_empty() || token_id_down.is_empty() {
        if let Some(ref clob_ids) = m.clob_token_ids {
            let trimmed = clob_ids.trim();
            let parts: Vec<&str> = if trimmed.starts_with('[') {
                serde_json::from_str(trimmed).unwrap_or_else(|_| vec![])
            } else {
                trimmed
                    .split(',')
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                    .collect()
            };
            if parts.len() >= 2 {
                token_id_up = parts[0].to_string();
                token_id_down = parts[1].to_string();
            }
        }
    }

    if token_id_up.is_empty() || token_id_down.is_empty() {
        anyhow::bail!(
            "Market could not resolve Up/Down token IDs (clobTokenIds={:?}, tokens with outcome?)",
            m.clob_token_ids
        );
    }
    Ok((token_id_up, token_id_down))
}
