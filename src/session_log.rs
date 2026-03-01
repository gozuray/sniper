//! Session log: JSONL file per run with position closes, interval summaries, and session stats.
//! One JSON object per line for easy append and parsing.

use crate::types::EntrySide;
use anyhow::Result;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::Path;

/// Exit type for a closed position.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitType {
    TakeProfit,
    StopLoss,
    MarketClose,
}

fn exit_type_str(t: ExitType) -> &'static str {
    match t {
        ExitType::TakeProfit => "TP",
        ExitType::StopLoss => "SL",
        ExitType::MarketClose => "MARKET_CLOSE",
    }
}

fn side_str(side: EntrySide) -> &'static str {
    match side {
        EntrySide::Up => "Up",
        EntrySide::Down => "Down",
    }
}

fn dec_opt(o: Option<Decimal>) -> Option<String> {
    o.map(|d| d.to_string())
}

/// Session logger: appends JSONL lines to a file. Tracks counts for session summary.
pub struct SessionLog {
    file: File,
    session_start_ms: u64,
    tp_count: u32,
    sl_count: u32,
    market_close_count: u32,
    total_pnl: Decimal,
}

impl SessionLog {
    /// Create a new session log in `dir` with filename `session_YYYY-MM-DDTHH-MM-SS.jsonl`.
    /// Creates `dir` if it does not exist. Returns None if disabled or creation fails.
    pub fn new(session_start_ms: u64, dir: &str) -> Result<Option<Self>> {
        let path = Path::new(dir);
        if !path.exists() {
            fs::create_dir_all(path)?;
        }
        let iso = {
            let secs = (session_start_ms / 1000) as i64;
            let nanos = ((session_start_ms % 1000) * 1_000_000) as u32;
            chrono::DateTime::from_timestamp(secs, nanos)
                .map(|dt| dt.format("%Y-%m-%dT%H-%M-%S").to_string())
                .unwrap_or_else(|| session_start_ms.to_string())
        };
        let filename = path.join(format!("session_{}.jsonl", iso));
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&filename)?;
        tracing::info!("[SessionLog] writing to {}", filename.display());
        Ok(Some(Self {
            file,
            session_start_ms,
            tp_count: 0,
            sl_count: 0,
            market_close_count: 0,
            total_pnl: Decimal::ZERO,
        }))
    }

    fn write_line(&mut self, obj: &serde_json::Value) -> Result<()> {
        let line = serde_json::to_string(obj)?;
        writeln!(self.file, "{}", line)?;
        self.file.flush()?;
        Ok(())
    }

    /// Log a position close (TP, SL, or MARKET_CLOSE). Updates internal counts and PnL.
    #[allow(clippy::too_many_arguments)]
    pub fn log_position_close(
        &mut self,
        slug: &str,
        interval_start_unix: u64,
        close_time_unix: u64,
        side: EntrySide,
        entry_price: Decimal,
        exit_price: Decimal,
        entry_time_ms: u64,
        exit_time_ms: u64,
        exit_type: ExitType,
        size: Decimal,
        min_bid_up: Option<Decimal>,
        max_bid_up: Option<Decimal>,
        min_bid_down: Option<Decimal>,
        max_bid_down: Option<Decimal>,
    ) -> Result<()> {
        let duration_sec = (exit_time_ms.saturating_sub(entry_time_ms)) / 1000;
        let pnl = size * (exit_price - entry_price);

        match exit_type {
            ExitType::TakeProfit => self.tp_count += 1,
            ExitType::StopLoss => self.sl_count += 1,
            ExitType::MarketClose => self.market_close_count += 1,
        }
        self.total_pnl += pnl;

        let ranged_01_99_up = min_bid_up
            .zip(max_bid_up)
            .map(|(min, max)| min <= dec!(0.02) && max >= dec!(0.98))
            .unwrap_or(false);
        let ranged_01_99_down = min_bid_down
            .zip(max_bid_down)
            .map(|(min, max)| min <= dec!(0.02) && max >= dec!(0.98))
            .unwrap_or(false);

        let obj = serde_json::json!({
            "event": "close",
            "slug": slug,
            "interval_start_unix": interval_start_unix,
            "close_time_unix": close_time_unix,
            "side": side_str(side),
            "entry_price": entry_price.to_string(),
            "exit_price": exit_price.to_string(),
            "entry_time_ms": entry_time_ms,
            "exit_time_ms": exit_time_ms,
            "exit_type": exit_type_str(exit_type),
            "size": size.to_string(),
            "pnl_usd": pnl.to_string(),
            "duration_sec": duration_sec,
            "min_bid_up": dec_opt(min_bid_up),
            "max_bid_up": dec_opt(max_bid_up),
            "min_bid_down": dec_opt(min_bid_down),
            "max_bid_down": dec_opt(max_bid_down),
            "ranged_01_99_up": ranged_01_99_up,
            "ranged_01_99_down": ranged_01_99_down,
        });
        self.write_line(&obj)
    }

    /// Log interval summary (price range observed). Call when leaving an interval.
    pub fn log_interval_summary(
        &mut self,
        slug: &str,
        interval_start_unix: u64,
        close_time_unix: u64,
        min_bid_up: Option<Decimal>,
        max_bid_up: Option<Decimal>,
        min_bid_down: Option<Decimal>,
        max_bid_down: Option<Decimal>,
    ) -> Result<()> {
        let ranged_01_99_up = min_bid_up
            .zip(max_bid_up)
            .map(|(min, max)| min <= dec!(0.02) && max >= dec!(0.98))
            .unwrap_or(false);
        let ranged_01_99_down = min_bid_down
            .zip(max_bid_down)
            .map(|(min, max)| min <= dec!(0.02) && max >= dec!(0.98))
            .unwrap_or(false);

        let obj = serde_json::json!({
            "event": "interval_summary",
            "slug": slug,
            "interval_start_unix": interval_start_unix,
            "close_time_unix": close_time_unix,
            "min_bid_up": dec_opt(min_bid_up),
            "max_bid_up": dec_opt(max_bid_up),
            "min_bid_down": dec_opt(min_bid_down),
            "max_bid_down": dec_opt(max_bid_down),
            "ranged_01_99_up": ranged_01_99_up,
            "ranged_01_99_down": ranged_01_99_down,
        });
        self.write_line(&obj)
    }

    /// Write session summary (win rate, total PnL, counts). Call when bot exits.
    pub fn write_session_summary(&mut self) -> Result<()> {
        let end_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64;
        let session_duration_sec = (end_ms.saturating_sub(self.session_start_ms)) / 1000;
        let closed_count = self.tp_count + self.sl_count + self.market_close_count;
        let tp_sl_count = self.tp_count + self.sl_count;
        let win_rate = if tp_sl_count > 0 {
            (self.tp_count as f64) / (tp_sl_count as f64)
        } else {
            f64::NAN
        };

        let obj = serde_json::json!({
            "event": "session_summary",
            "session_start_ms": self.session_start_ms,
            "session_end_ms": end_ms,
            "session_duration_sec": session_duration_sec,
            "tp_count": self.tp_count,
            "sl_count": self.sl_count,
            "market_close_count": self.market_close_count,
            "total_closes": closed_count,
            "win_rate": if win_rate.is_nan() { serde_json::Value::Null } else { serde_json::json!(win_rate) },
            "total_pnl_usd": self.total_pnl.to_string(),
        });
        self.write_line(&obj)
    }
}

impl Drop for SessionLog {
    fn drop(&mut self) {
        let _ = self.write_session_summary();
    }
}
