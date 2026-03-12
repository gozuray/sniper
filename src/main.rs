//! Interval Sniper (Rust): buy in range [min_buy_price, max_buy_price], sell on take profit and stop loss.
//! Same logic as the TypeScript bot in src/bot/marketMaker/.

mod clob;
mod clob_ws_book;
mod clob_ws_user;
mod config;
mod market;
mod orderbook;
mod redeem;
mod runner;
mod session_log;
mod signing;
mod telegram_log;
mod types;

use tracing::Event;
use tracing_subscriber::fmt::format::{FormatEvent, Writer};
use tracing_subscriber::fmt::{FormatFields, FmtContext};

/// Formato de log con target y nivel de ancho fijo para alinear columnas en terminal.
struct SymFormat;

const TARGET_WIDTH: usize = 24;
const LEVEL_WIDTH: usize = 5;

impl<S, N> FormatEvent<S, N> for SymFormat
where
    S: tracing_subscriber::layer::SubscriberExt + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
    N: for<'a> tracing_subscriber::fmt::FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> std::fmt::Result {
        let meta = event.metadata();
        let target = meta.target();
        let level = meta.level().as_str();
        let now = chrono::Utc::now();
        let ts = now.format("%Y-%m-%dT%H:%M:%S%.6fZ");

        write!(
            writer,
            "{} {:>level_width$} {:target_width$} | ",
            ts,
            level,
            target,
            level_width = LEVEL_WIDTH,
            target_width = TARGET_WIDTH
        )?;
        ctx.format_fields(writer.by_ref(), event)?;
        writeln!(writer)
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .event_format(SymFormat)
        .init();

    runner::run().await
}
