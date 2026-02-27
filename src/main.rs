//! Interval Sniper (Rust): buy in range [min_buy_price, max_buy_price], sell on take profit and stop loss.
//! Same logic as the TypeScript bot in src/bot/marketMaker/.

mod clob;
mod clob_ws_book;
mod config;
mod market;
mod orderbook;
mod runner;
mod signing;
mod types;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    runner::run().await
}
