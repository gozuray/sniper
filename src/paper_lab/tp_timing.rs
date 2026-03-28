//! EMA del tiempo hasta alcanzar TP (por bucket de `p_strong`), persistido en disco.
//! Alimenta ventanas de espera adaptativas: señales más fuertes reciben más tiempo.

use crate::rl::DeltaQAgent;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BucketPersist {
    ema_ms: f64,
    n: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct FileJson {
    version: u32,
    buckets: [BucketPersist; 3],
}

pub struct TpTimingStore {
    buckets: [BucketPersist; 3],
    persist_path: PathBuf,
    ema_alpha: f64,
}

impl TpTimingStore {
    pub fn load_or_new(path: PathBuf, ema_alpha: f64) -> Result<Self> {
        let ema_alpha = ema_alpha.clamp(0.01, 1.0);
        if let Ok(s) = fs::read_to_string(&path) {
            if let Ok(p) = serde_json::from_str::<FileJson>(&s) {
                if p.version >= 1 {
                    return Ok(Self {
                        buckets: p.buckets,
                        persist_path: path,
                        ema_alpha,
                    });
                }
            }
        }
        Ok(Self {
            buckets: [
                BucketPersist { ema_ms: 0.0, n: 0 },
                BucketPersist { ema_ms: 0.0, n: 0 },
                BucketPersist { ema_ms: 0.0, n: 0 },
            ],
            persist_path: path,
            ema_alpha,
        })
    }

    /// Tiempo permitido desde la entrada hasta que deba haberse alcanzado el nivel de TP,
    /// combinando EMA histórico del bucket y el `p_strong` (señal débil → menos tiempo; fuerte → más).
    pub fn allowed_wait_ms(
        &self,
        p_strong: f64,
        strong_threshold: f64,
        weak_time_factor: f64,
        strong_time_factor: f64,
        default_ms: u64,
        min_ms: u64,
        max_ms: u64,
    ) -> u64 {
        let b = DeltaQAgent::p_strong_bucket(p_strong, 1);
        let base = if self.buckets[b].n == 0 {
            default_ms as f64
        } else {
            self.buckets[b].ema_ms.max(500.0)
        };
        let lo = strong_threshold.clamp(0.5, 0.95);
        let hi = 0.97_f64;
        let t = if (hi - lo).abs() > f64::EPSILON {
            ((p_strong - lo) / (hi - lo)).clamp(0.0, 1.0)
        } else {
            0.5
        };
        let factor = weak_time_factor + t * (strong_time_factor - weak_time_factor);
        let raw = base * factor;
        let ms = if raw.is_finite() && raw > 0.0 {
            raw.round() as u64
        } else {
            default_ms
        };
        ms.clamp(min_ms, max_ms)
    }

    pub fn record_success(&mut self, p_strong: f64, dt_ms: u64) -> Result<()> {
        let b = DeltaQAgent::p_strong_bucket(p_strong, 1);
        let dt = dt_ms.max(1) as f64;
        let a = self.ema_alpha;
        if self.buckets[b].n == 0 {
            self.buckets[b].ema_ms = dt;
        } else {
            self.buckets[b].ema_ms = a * dt + (1.0 - a) * self.buckets[b].ema_ms;
        }
        self.buckets[b].n = self.buckets[b].n.saturating_add(1);
        self.save()
    }

    pub fn save(&self) -> Result<()> {
        if let Some(dir) = self.persist_path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("mkdir {}", dir.display()))?;
        }
        let payload = FileJson {
            version: 1,
            buckets: self.buckets.clone(),
        };
        let json = serde_json::to_string_pretty(&payload)?;
        let tmp = self.persist_path.with_extension("json.tmp");
        fs::write(&tmp, &json).with_context(|| format!("write {}", tmp.display()))?;
        replace_atomic(&tmp, &self.persist_path)
            .with_context(|| format!("replace {}", self.persist_path.display()))?;
        Ok(())
    }
}

fn replace_atomic(temp: &Path, final_path: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        if final_path.exists() {
            fs::remove_file(final_path)?;
        }
    }
    fs::rename(temp, final_path)?;
    Ok(())
}
