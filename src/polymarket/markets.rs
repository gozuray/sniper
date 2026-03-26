use crate::types::{Asset, Interval, MarketKey, ResolvedMarket, TokenId};
use anyhow::{Context, Result};
use chrono::{DateTime, TimeZone, Timelike, Utc};
use chrono_tz::America::New_York;
use reqwest::Client;
use std::str::FromStr;

/// Minimal Gamma market response fields needed for slug -> tokens + conditionId.
#[derive(Debug, Clone, serde::Deserialize)]
struct GammaMarket {
    /// Present in some responses.
    #[serde(rename = "conditionId")]
    condition_id: Option<String>,
    /// Present in some responses (id is conditionId for this endpoint variant).
    id: Option<String>,

    end_date: Option<String>,
    #[serde(rename = "endDate")]
    end_date_alt: Option<String>,
    #[serde(rename = "endDateIso")]
    end_date_iso: Option<String>,

    tokens: Option<Vec<GammaToken>>,
    /// Gamma often omits `tokens` and instead sends a JSON array as a string, e.g. `["id1","id2"]` (Up, Down).
    #[serde(rename = "clobTokenIds")]
    clob_token_ids: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize)]
struct GammaToken {
    #[serde(rename = "tokenId", alias = "token_id")]
    token_id: Option<String>,
    outcome: Option<String>,
}

fn parse_end_date_to_unix(s: &str) -> Result<u64> {
    // Gamma returns inconsistent timestamp formats (unix string, RFC3339, ISO without TZ, ms).
    let s: String = s
        .trim()
        .trim_start_matches('\u{feff}')
        .trim_matches(|c| c == '"' || c == '\'')
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    let s = s.as_str();

    if let Ok(t) = s.parse::<u64>() {
        return Ok(t.max(0));
    }
    if let Ok(ms) = s.parse::<i64>() {
        if ms > 1_000_000_000_000 {
            return Ok((ms / 1000).max(0) as u64);
        }
    }

    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Ok(dt.with_timezone(&chrono::Utc).timestamp().max(0) as u64);
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc2822(s) {
        return Ok(dt.with_timezone(&chrono::Utc).timestamp().max(0) as u64);
    }

    // Naive ISO 8601: `2026-03-25T22:00:00` / `...000` without `Z` or `+00:00`.
    if s.contains('T') && !s.ends_with('Z') {
        let tail = s.get(19..).unwrap_or("");
        let has_offset = tail.starts_with('+')
            || (tail.starts_with('-') && tail.contains(':'));
        if !has_offset {
            let z = format!("{s}Z");
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(&z) {
                return Ok(dt.with_timezone(&chrono::Utc).timestamp().max(0) as u64);
            }
        }
    }

    for fmt in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S",
        "%Y-%m-%d %H:%M:%S",
    ] {
        if let Ok(ndt) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            return Ok(ndt.and_utc().timestamp().max(0) as u64);
        }
    }
    if let Ok(d) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        let ndt = d
            .and_hms_opt(0, 0, 0)
            .context("Gamma endDate date-only")?;
        return Ok(ndt.and_utc().timestamp().max(0) as u64);
    }

    anyhow::bail!("unrecognized Gamma endDate string: {s:?}")
}

fn parse_u256_any(s: &str) -> Result<TokenId> {
    let s = s.trim();
    if s.starts_with("0x") || s.starts_with("0X") {
        TokenId::from_str(s).context("parse U256 hex")
    } else {
        // decimal
        TokenId::from_str(s).context("parse U256 decimal")
    }
}

fn parse_condition_id_to_b256(s: &str) -> Result<polymarket_client_sdk::types::B256> {
    let s = s.trim();
    if s.is_empty() {
        anyhow::bail!("empty condition id");
    }
    if s.starts_with("0x") || s.starts_with("0X") {
        polymarket_client_sdk::types::B256::from_str(s).context("parse B256 hex")
    } else {
        let with_prefix = format!("0x{s}");
        polymarket_client_sdk::types::B256::from_str(&with_prefix).context("parse B256 hex (no 0x)")
    }
}

/// Slug format used by Polymarket for Up/Down markets.
///
/// Examples:
/// - `btc-updown-5m-1772169300`
/// - `sol-updown-15m-1774461600`
pub fn slug_for(asset: Asset, interval: Interval, interval_start_unix: u64) -> String {
    format!(
        "{}-updown-{}-{}",
        asset.as_gamma_slug_prefix(),
        interval.as_slug_suffix(),
        interval_start_unix
    )
}

/// Compute current interval start aligned to interval length.
pub fn interval_start_unix(now_unix_sec: u64, interval_sec: u64) -> u64 {
    (now_unix_sec / interval_sec) * interval_sec
}

/// Inicio unix del mercado **5m** tal como lo organiza Polymarket en la web: franjas en **ET**
/// (`America/New_York`, DST incluido), no el simple `unix/300` en UTC.
///
/// El sufijo numérico del slug `btc-updown-5m-<unix>` coincide con este valor.
pub fn polymarket_5m_interval_start_unix_et(now_unix_sec: u64) -> u64 {
    let Some(now) = DateTime::from_timestamp(now_unix_sec as i64, 0) else {
        return interval_start_unix(now_unix_sec, Interval::M5.sec());
    };
    let et = now.with_timezone(&New_York);
    let m5 = (et.minute() / 5) * 5;
    let Some(naive) = et.date_naive().and_hms_opt(et.hour(), m5, 0) else {
        return interval_start_unix(now_unix_sec, Interval::M5.sec());
    };
    match New_York.from_local_datetime(&naive) {
        chrono::LocalResult::Single(dt) => dt.with_timezone(&Utc).timestamp().max(0) as u64,
        chrono::LocalResult::Ambiguous(earliest, _) => {
            earliest.with_timezone(&Utc).timestamp().max(0) as u64
        }
        chrono::LocalResult::None => interval_start_unix(now_unix_sec, Interval::M5.sec()),
    }
}

/// Resolve a specific market instance from Gamma by slug.
pub async fn resolve_market_by_slug(
    http: &Client,
    gamma_base_url: &str,
    key: MarketKey,
    interval_start_unix: u64,
) -> Result<ResolvedMarket> {
    let slug = slug_for(key.asset, key.interval, interval_start_unix);
    let base = gamma_base_url.trim_end_matches('/');
    let market_url = format!("{base}/markets/slug/{}", urlencoding::encode(&slug));

    let res = http
        .get(&market_url)
        .header("user-agent", "hft-momentum-polymarket-rust")
        .send()
        .await
        .context("Gamma market request")?;

    let market = if res.status() == 404 {
        // Fallback variant.
        let event_url = format!("{base}/events/slug/{}", urlencoding::encode(&slug));
        let event: serde_json::Value = http
            .get(&event_url)
            .header("user-agent", "hft-momentum-polymarket-rust")
            .send()
            .await
            .context("Gamma event request")?
            .json()
            .await
            .context("Gamma event JSON")?;

        // `markets` is typically an array.
        let m = event
            .get("markets")
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .cloned()
            .context("Gamma event has no markets array")?;

        serde_json::from_value::<GammaMarket>(m).context("Gamma event markets[0] parse")?
    } else {
        res.json::<GammaMarket>().await.context("Gamma market JSON parse")?
    };

    let condition_id = market
        .condition_id
        .as_deref()
        .or(market.id.as_deref())
        .unwrap_or("")
        .trim();
    anyhow::ensure!(!condition_id.is_empty(), "Gamma market has no condition_id (slug={slug})");

    let end_date = market
        .end_date_iso
        .as_deref()
        .or(market.end_date.as_deref())
        .or(market.end_date_alt.as_deref())
        .unwrap_or("");
    anyhow::ensure!(!end_date.is_empty(), "Gamma market has no end_date fields (slug={slug})");
    let interval_sec = key.interval.sec();
    // Ventana de trading: la del propio slug (`btc-updown-5m-<unix>`), igual que en polymarket.com/event/...
    // Gamma `endDate` a veces no coincide (TZ / solo fecha / formato); eso rompía `is_market_open` al cambiar vela.
    let interval_start_resolved = interval_start_unix;
    let close_time_resolved = interval_start_resolved.saturating_add(interval_sec);

    let close_gamma = parse_end_date_to_unix(end_date)?;
    let start_gamma = close_gamma.saturating_sub(interval_sec);
    if start_gamma.abs_diff(interval_start_resolved) > 120
        || close_gamma.abs_diff(close_time_resolved) > 120
    {
        // Gamma suele devolver endDate/s que no coinciden con la franja 5m del slug; el slug es la referencia.
        tracing::trace!(
            target: "sniper",
            slug = %slug,
            slug_start = interval_start_resolved,
            slug_close = close_time_resolved,
            gamma_start = start_gamma,
            gamma_close = close_gamma,
            "Gamma endDate difiere de la ventana del slug; se usa la ventana del slug"
        );
    }

    let (token_id_up, token_id_down) = parse_up_down_tokens(&market)
        .context("Gamma tokens must include Up/Down outcomes")?;

    Ok(ResolvedMarket {
        key,
        slug,
        condition_id: parse_condition_id_to_b256(condition_id)?,
        interval_start_unix: interval_start_resolved,
        close_time_unix: close_time_resolved,
        token_id_up,
        token_id_down,
    })
}

fn parse_clob_token_id_pair(clob_field: &str) -> Option<(String, String)> {
    let trimmed = clob_field.trim();
    let parts: Vec<String> = if trimmed.starts_with('[') {
        serde_json::from_str::<Vec<String>>(trimmed).unwrap_or_default()
    } else {
        trimmed
            .split(',')
            .map(|s| s.trim().trim_matches('"').to_string())
            .filter(|s| !s.is_empty())
            .collect()
    };
    if parts.len() >= 2 {
        Some((parts[0].clone(), parts[1].clone()))
    } else {
        None
    }
}

fn parse_up_down_tokens(m: &GammaMarket) -> Result<(TokenId, TokenId)> {
    let mut up: Option<TokenId> = None;
    let mut down: Option<TokenId> = None;

    if let Some(tokens) = m.tokens.as_ref() {
        for t in tokens {
            let id = t.token_id.as_deref().unwrap_or("").trim();
            if id.is_empty() {
                continue;
            }
            let outcome = t.outcome.as_deref().unwrap_or("").to_lowercase();
            if outcome == "up" || outcome == "yes" {
                up = Some(parse_u256_any(id)?);
            } else if outcome == "down" || outcome == "no" {
                down = Some(parse_u256_any(id)?);
            }
        }
    }

    if up.is_none() || down.is_none() {
        if let Some(ref raw) = m.clob_token_ids {
            if let Some((a, b)) = parse_clob_token_id_pair(raw) {
                if up.is_none() {
                    up = Some(parse_u256_any(&a)?);
                }
                if down.is_none() {
                    down = Some(parse_u256_any(&b)?);
                }
            }
        }
    }

    let up = up.context("Missing Up/Yes token (no tokens[] or clobTokenIds)")?;
    let down = down.context("Missing Down/No token (no tokens[] or clobTokenIds)")?;
    Ok((up, down))
}

/// `true` si Gamma tiene el evento por slug y está listado como operable (misma ruta lógica que la URL web).
async fn gamma_btc_5m_slug_event_tradeable(
    http: &Client,
    gamma_base_url: &str,
    interval_start_unix: u64,
    now_unix_sec: u64,
) -> Result<bool> {
    let slug = slug_for(Asset::BTC, Interval::M5, interval_start_unix);
    let base = gamma_base_url.trim_end_matches('/');
    let url = format!("{base}/events/slug/{}", urlencoding::encode(&slug));

    let res = http
        .get(&url)
        .header("user-agent", "hft-momentum-polymarket-rust")
        .send()
        .await
        .with_context(|| format!("Gamma GET events/slug for {slug}"))?;

    if res.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(false);
    }
    if !res.status().is_success() {
        anyhow::bail!("Gamma events/slug returned HTTP {}", res.status());
    }

    let v: serde_json::Value = res.json().await.context("Gamma event slug JSON")?;
    let active = v.get("active").and_then(|x| x.as_bool()).unwrap_or(false);
    let closed = v.get("closed").and_then(|x| x.as_bool()).unwrap_or(true);
    if !active || closed {
        return Ok(false);
    }

    if let Some(markets) = v.get("markets").and_then(|m| m.as_array()) {
        if let Some(m0) = markets.first() {
            let m_active = m0.get("active").and_then(|x| x.as_bool()).unwrap_or(false);
            let m_closed = m0.get("closed").and_then(|x| x.as_bool()).unwrap_or(true);
            if !m_active || m_closed {
                return Ok(false);
            }
        }
    }

    let window = Interval::M5.sec();
    Ok(now_unix_sec >= interval_start_unix && now_unix_sec < interval_start_unix.saturating_add(window))
}

/// Descubre el **inicio unix del mercado BTC 5m que Polymarket marca como activo** y cuyo slug
/// `btc-updown-5m-<unix>` contiene `now_unix_sec` en `[unix, unix+300)`.
///
/// Orden: (1) mismo slug que en `polymarket.com/event/btc-updown-5m-<unix>` vía `events/slug/...`
/// (no depende del top‑N de la lista); (2) lista paginada como respaldo.
/// Ver [documentación Gamma](https://docs.polymarket.com/developers/gamma-markets-api/get-events).
pub async fn gamma_discover_open_btc_5m_interval_start_unix(
    http: &Client,
    gamma_base_url: &str,
    now_unix_sec: u64,
) -> Result<Option<u64>> {
    let et_slot = polymarket_5m_interval_start_unix_et(now_unix_sec);
    if gamma_btc_5m_slug_event_tradeable(http, gamma_base_url, et_slot, now_unix_sec).await? {
        return Ok(Some(et_slot));
    }

    let base = gamma_base_url.trim_end_matches('/');
    let url = format!(
        "{base}/events?slug_contains=btc-updown-5m&active=true&closed=false&limit=100&order=startDate&ascending=false"
    );

    let res = http
        .get(&url)
        .header("user-agent", "hft-momentum-polymarket-rust")
        .send()
        .await
        .context("Gamma GET active btc-updown-5m events")?;

    if !res.status().is_success() {
        anyhow::bail!("Gamma events returned HTTP {}", res.status());
    }

    let v: serde_json::Value = res.json().await.context("Gamma events JSON")?;
    let arr = v.as_array().context("Gamma events response is not an array")?;

    let window = Interval::M5.sec();
    let mut chosen: Option<u64> = None;
    for ev in arr {
        let slug = ev.get("slug").and_then(|s| s.as_str()).unwrap_or("");
        let Some(suffix) = slug.strip_prefix("btc-updown-5m-") else {
            continue;
        };
        let Ok(ts) = suffix.parse::<u64>() else {
            continue;
        };
        if now_unix_sec >= ts && now_unix_sec < ts.saturating_add(window) {
            chosen = Some(chosen.map_or(ts, |c| c.max(ts)));
        }
    }
    Ok(chosen)
}

/// Resolve markets for each `assets` × `intervals` combination, for current time + `horizon` future intervals.
///
/// `now_unix_sec` debe ser el mismo instante que usas para [`polymarket_5m_interval_start_unix_et`] en el bucle
/// principal; si aquí se llama a `SystemTime::now()` dentro mientras el tick lleva segundos de retraso,
/// `i=0` puede quedar en la franja **anterior** y el WS seguiría el mercado equivocado.
///
/// `btc_m5_interval_start_unix`: si está presente, es el **`interval_start_unix` del slug i=0** para BTC+M5
/// (p. ej. devuelto por [`gamma_discover_open_btc_5m_interval_start_unix`]); evita desfase respecto a la web.
pub async fn resolve_active_markets(
    http: &Client,
    gamma_base_url: &str,
    assets: &[Asset],
    intervals: &[Interval],
    horizon_intervals: u32,
    now_unix_sec: u64,
    btc_m5_interval_start_unix: Option<u64>,
) -> Result<Vec<ResolvedMarket>> {
    let now_sec = now_unix_sec;

    let per_combo = (horizon_intervals as usize).saturating_add(1);
    let mut out = Vec::with_capacity(assets.len().saturating_mul(intervals.len()).saturating_mul(per_combo));
    for &asset in assets {
        for &interval in intervals {
            let interval_start = match interval {
                Interval::M5 => {
                    if asset == Asset::BTC {
                        btc_m5_interval_start_unix.unwrap_or_else(|| polymarket_5m_interval_start_unix_et(now_sec))
                    } else {
                        polymarket_5m_interval_start_unix_et(now_sec)
                    }
                }
                Interval::M15 => interval_start_unix(now_sec, interval.sec()),
            };
            for i in 0..=horizon_intervals {
                let start = interval_start.saturating_add(i as u64 * interval.sec());
                let key = MarketKey { asset, interval };
                let market = resolve_market_by_slug(http, gamma_base_url, key, start).await?;
                out.push(market);
            }
        }
    }
    Ok(out)
}

/// Check if a resolved market is currently open.
pub fn is_market_open(market: &ResolvedMarket, now_unix_sec: u64) -> bool {
    now_unix_sec >= market.interval_start_unix && now_unix_sec < market.close_time_unix
}

/// Mercado BTC 5m del **instante** `now_unix_sec`: busca el slug con `interval_start_unix` =
/// `interval_start_hint` si se pasa (ancla Gamma o ya acordada); si no, usa [`polymarket_5m_interval_start_unix_et`].
///
/// Si no está en el horizonte resuelto, hace fallback al primer BTC 5m con [`is_market_open`]
/// y deja un `warn` (evita desalineación silenciosa tras I/O lento en el cambio de franja).
pub fn active_btc_5m_market<'a>(
    resolved: &'a [ResolvedMarket],
    now_unix_sec: u64,
    interval_start_hint: Option<u64>,
) -> Option<&'a ResolvedMarket> {
    let expected_start =
        interval_start_hint.unwrap_or_else(|| polymarket_5m_interval_start_unix_et(now_unix_sec));
    if let Some(m) = resolved.iter().find(|m| {
        m.key.asset == Asset::BTC
            && m.key.interval == Interval::M5
            && m.interval_start_unix == expected_start
    }) {
        if is_market_open(m, now_unix_sec) {
            return Some(m);
        }
        // En el segundo `close_time_unix` (p. ej. tick a las :25.00 UTC) la ventana [start, close)
        // ya cerró: no es desincronización, solo rollover de franja.
        if now_unix_sec >= m.close_time_unix {
            tracing::trace!(
                target: "sniper",
                slug = %m.slug,
                expected_start,
                now_unix_sec,
                close_time_unix = m.close_time_unix,
                "BTC 5m: franja ya cerró (esperado tras cambio de vela 5m)"
            );
        } else {
            tracing::warn!(
                target: "sniper",
                slug = %m.slug,
                expected_start,
                now_unix_sec,
                close_time_unix = m.close_time_unix,
                "BTC 5m: mercado de la franja ET resuelto pero `is_market_open` = false (revisa reloj o datos)"
            );
        }
        return None;
    }

    let fallback = resolved.iter().find(|m| {
        m.key.asset == Asset::BTC && m.key.interval == Interval::M5 && is_market_open(m, now_unix_sec)
    });
    if let Some(m) = fallback {
        tracing::warn!(
            target: "sniper",
            expected_et_slot_start_unix = expected_start,
            fallback_slug = %m.slug,
            fallback_start = m.interval_start_unix,
            "BTC 5m: no hay entrada con interval_start == franja ET; usando otro mercado abierto (amplía subscription_horizon_intervals o hubo demora en Gamma)"
        );
    }
    fallback
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Slugs del sitio: 6:55–7PM ET y 7:00–7:05PM ET (Mar 25, 2026).
    #[test]
    fn polymarket_et_5m_slot_matches_slug_timestamps() {
        let t0 = 1774479300_u64;
        let t1 = 1774479600_u64;
        assert_eq!(t1 - t0, Interval::M5.sec());
        assert_eq!(polymarket_5m_interval_start_unix_et(t0 + 60), t0);
        assert_eq!(polymarket_5m_interval_start_unix_et(t1 + 120), t1);
    }

    /// Ventanas 23:00–23:05 / 23:05–23:10 / 23:10–23:15 UTC (7:00–7:15 PM EDT) = slugs Polymarket del usuario.
    #[test]
    fn polymarket_5m_mar25_2026_utc_windows_match_slugs() {
        let s0 = 1774479600_u64;
        let s1 = 1774479900_u64;
        let s2 = 1774480200_u64;
        assert_eq!(s1 - s0, Interval::M5.sec());
        assert_eq!(s2 - s1, Interval::M5.sec());
        assert_eq!(polymarket_5m_interval_start_unix_et(s0), s0);
        assert_eq!(polymarket_5m_interval_start_unix_et(s0 + 299), s0);
        assert_eq!(polymarket_5m_interval_start_unix_et(s1 + 60), s1);
        assert_eq!(polymarket_5m_interval_start_unix_et(s2), s2);
    }
}

