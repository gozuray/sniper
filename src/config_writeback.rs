//! Atomic write-back of RL-tuned parameters into `config.toml`.
//!
//! Flow: read existing file → parse with `toml_edit` (preserves comments/formatting)
//! → patch only the keys the RL touched → write to `.tmp` → rename over original.
//! A `.bak` copy is created **once** per process lifetime before the first mutation.

use anyhow::{Context, Result};
use rust_decimal::Decimal;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use toml_edit::{DocumentMut, value};

static BACKUP_DONE: AtomicBool = AtomicBool::new(false);

static LAST_WRITTEN: Mutex<Option<TunedParams>> = Mutex::new(None);

/// Snapshot of the values the RL / adaptive layer may have changed.
#[derive(Debug, Clone)]
pub struct TunedParams {
    pub delta_up_pct: Decimal,
    pub delta_down_pct: Decimal,
    pub edge_min: Decimal,
    pub prob_scale: f64,
    pub min_taker_imbalance: f64,
    pub take_profit_ticks: Option<Decimal>,
    pub stop_loss_ticks: Option<Decimal>,
}

impl TunedParams {
    fn same_as(&self, other: &TunedParams) -> bool {
        self.delta_up_pct == other.delta_up_pct
            && self.delta_down_pct == other.delta_down_pct
            && self.edge_min == other.edge_min
            && (self.prob_scale - other.prob_scale).abs() < 0.01
            && (self.min_taker_imbalance - other.min_taker_imbalance).abs() < 0.001
            && self.take_profit_ticks == other.take_profit_ticks
            && self.stop_loss_ticks == other.stop_loss_ticks
    }
}

pub fn writeback_config(config_path: &Path, params: &TunedParams) -> Result<()> {
    if let Ok(guard) = LAST_WRITTEN.lock() {
        if let Some(ref prev) = *guard {
            if params.same_as(prev) {
                return Ok(());
            }
        }
    }
    ensure_backup(config_path)?;

    let raw = fs::read_to_string(config_path)
        .with_context(|| format!("writeback: read {}", config_path.display()))?;

    let mut doc: DocumentMut = raw
        .parse::<DocumentMut>()
        .with_context(|| "writeback: parse TOML")?;

    patch_momentum(&mut doc, params);
    patch_trading(&mut doc, params);

    let tmp = config_path.with_extension("toml.tmp");
    fs::write(&tmp, doc.to_string())
        .with_context(|| format!("writeback: write {}", tmp.display()))?;
    atomic_replace(&tmp, config_path)
        .with_context(|| format!("writeback: replace {}", config_path.display()))?;

    tracing::info!(
        target: "sniper",
        delta_up = %params.delta_up_pct,
        delta_down = %params.delta_down_pct,
        edge_min = %params.edge_min,
        prob_scale = params.prob_scale,
        min_taker = params.min_taker_imbalance,
        tp_ticks = ?params.take_profit_ticks,
        sl_ticks = ?params.stop_loss_ticks,
        "config_writeback · config.toml actualizado con parámetros RL"
    );

    if let Ok(mut guard) = LAST_WRITTEN.lock() {
        *guard = Some(params.clone());
    }

    Ok(())
}

fn ensure_backup(config_path: &Path) -> Result<()> {
    if BACKUP_DONE.load(Ordering::Relaxed) {
        return Ok(());
    }
    let bak = backup_path(config_path);
    if !bak.exists() {
        fs::copy(config_path, &bak)
            .with_context(|| format!("writeback: backup → {}", bak.display()))?;
        tracing::info!(
            target: "sniper",
            path = %bak.display(),
            "config_writeback · backup creado (primera escritura del proceso)"
        );
    }
    BACKUP_DONE.store(true, Ordering::Relaxed);
    Ok(())
}

fn backup_path(config_path: &Path) -> PathBuf {
    let stem = config_path
        .file_stem()
        .unwrap_or_default()
        .to_string_lossy();
    let ext = config_path
        .extension()
        .unwrap_or_default()
        .to_string_lossy();
    config_path.with_file_name(format!("{stem}.bak.{ext}"))
}

fn patch_momentum(doc: &mut DocumentMut, p: &TunedParams) {
    let Some(tbl) = doc.get_mut("momentum").and_then(|v| v.as_table_mut()) else {
        return;
    };

    // Remove USD overrides so RL-tuned pct values take effect on next restart.
    tbl.remove("delta_up_usd");
    tbl.remove("delta_down_usd");

    tbl["delta_up_pct"] = value(p.delta_up_pct.to_string());
    tbl["delta_down_pct"] = value(p.delta_down_pct.to_string());
    tbl["prob_scale"] = value(p.prob_scale);
    tbl["min_taker_imbalance"] = value(p.min_taker_imbalance);
}

fn patch_trading(doc: &mut DocumentMut, p: &TunedParams) {
    let Some(tbl) = doc.get_mut("trading").and_then(|v| v.as_table_mut()) else {
        return;
    };

    tbl["edge_min"] = value(p.edge_min.to_string());

    match p.take_profit_ticks {
        Some(tp) => {
            tbl["take_profit_ticks"] = value(tp.to_string());
        }
        None => {
            tbl.remove("take_profit_ticks");
        }
    }

    match p.stop_loss_ticks {
        Some(sl) => {
            tbl["stop_loss_ticks"] = value(sl.to_string());
        }
        None => {
            tbl.remove("stop_loss_ticks");
        }
    }
}

fn atomic_replace(tmp: &Path, dest: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        if dest.exists() {
            fs::remove_file(dest)?;
        }
    }
    fs::rename(tmp, dest)?;
    Ok(())
}
