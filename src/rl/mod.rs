//! Agente RL tabular (Q-learning) para tuning **cada intervalo** en paper.
//!
//! Estado: 3⁶ = 729 (time_remaining × TP rate × PnL × spread × p_strong × racha SL).
//! Acciones: deltas, edge, `min_taker_imbalance`, `prob_scale`, TP/SL ticks, hold.
//! Recompensa: PnL del intervalo (tanh) + TP/SL/settle.

use crate::config::RlTuneConfig;
use anyhow::{Context, Result};
use rand::Rng;
use rand::rngs::StdRng;
use rand::SeedableRng;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal_macros::dec;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

pub const NUM_ACTIONS: usize = 13;
/// 3⁶ = 729
pub const NUM_STATES: usize = 729;

#[derive(Debug, Clone)]
pub struct RlTuningState {
    pub delta_up_pct: Decimal,
    pub delta_down_pct: Decimal,
    pub edge_min: Decimal,
    pub min_taker_imbalance: f64,
    pub prob_scale: f64,
    pub take_profit_ticks: Option<Decimal>,
    pub stop_loss_ticks: Option<Decimal>,
    pub trailing_stop_loss_ticks: Option<Decimal>,
    pub sl_cooldown_ms: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct QTablePersist {
    version: u32,
    q: Vec<f64>,
    epsilon: f64,
}

pub struct DeltaQAgent {
    cfg: RlTuneConfig,
    q: Vec<f64>,
    epsilon: f64,
    pub last_state: Option<usize>,
    pub last_action: Option<usize>,
    rng: StdRng,
    persist_path: PathBuf,
}

impl DeltaQAgent {
    pub fn new(ap_cfg: &crate::config::AdaptivePaperConfig, rl: RlTuneConfig) -> Result<Self> {
        let base = PathBuf::from(ap_cfg.report_dir.trim());
        let persist_path = if Path::new(rl.persist_path.trim()).is_absolute() {
            PathBuf::from(rl.persist_path.trim())
        } else {
            base.join(rl.persist_path.trim())
        };

        let mut agent = Self {
            epsilon: rl.epsilon,
            q: vec![0.0; NUM_STATES * NUM_ACTIONS],
            last_state: None,
            last_action: None,
            rng: StdRng::from_entropy(),
            persist_path,
            cfg: rl,
        };

        if let Ok(data) = fs::read_to_string(&agent.persist_path) {
            if let Ok(p) = serde_json::from_str::<QTablePersist>(&data) {
                if p.q.len() == agent.q.len() {
                    agent.q = p.q;
                    let emax = agent.cfg.epsilon;
                    agent.epsilon = p.epsilon.clamp(agent.cfg.epsilon_min, emax);
                    tracing::info!(
                        target: "sniper",
                        path = %agent.persist_path.display(),
                        "rl · Q-table cargada"
                    );
                }
            }
        }

        // Warm-start: when time remaining is low (tr_b=0), favor tighten_delta (less trades);
        // when time remaining is ample (tr_b=2), favor loosen_delta (more trades).
        if agent.q.iter().all(|&v| v == 0.0) {
            for tr_b in 0..3usize {
                for tp_b in 0..3 {
                    for pnl_b in 0..3 {
                        for sp_b in 0..3 {
                            for ps_b in 0..3 {
                                for sl_b in 0..3 {
                                    let s = tr_b * 243 + tp_b * 81 + pnl_b * 27 + sp_b * 9 + ps_b * 3 + sl_b;
                                    if tr_b == 0 {
                                        agent.q[s * NUM_ACTIONS + 0] = 0.1;
                                    } else {
                                        agent.q[s * NUM_ACTIONS + 1] = 0.1;
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(agent)
    }

    /// Bucket racha SL consecutivos: 0 → 0, 1 → 1, 2+ → 2.
    #[inline]
    pub fn sl_streak_bucket(streak: u32) -> usize {
        if streak == 0 {
            0
        } else if streak == 1 {
            1
        } else {
            2
        }
    }

    /// s ∈ [0,728] → (time_rem, tp, pnl, spread, p_strong, sl_streak)
    #[inline]
    pub fn decode_state(s: usize) -> (u8, u8, u8, u8, u8, u8) {
        let s0 = s.min(NUM_STATES.saturating_sub(1));
        let sl_b = (s0 % 3) as u8;
        let mut s = s0 / 3;
        let ps_b = (s % 3) as u8;
        s /= 3;
        let sp_b = (s % 3) as u8;
        s /= 3;
        let pnlb = (s % 3) as u8;
        s /= 3;
        let tp_b = (s % 3) as u8;
        s /= 3;
        let tr_b = (s.min(2)) as u8;
        (tr_b, tp_b, pnlb, sp_b, ps_b, sl_b)
    }

    /// Bucket calibrado para rango real observado (p_strong ~0.55–0.65 en la mayoría de trades).
    #[inline]
    pub fn p_strong_bucket(avg_p_strong: f64, trades_opened: u32) -> usize {
        if trades_opened == 0 {
            return 1;
        }
        if avg_p_strong < 0.57 {
            0
        } else if avg_p_strong < 0.63 {
            1
        } else {
            2
        }
    }

    /// Bucket tiempo restante medio (seg) al abrir trades en el intervalo.
    /// Poco tiempo → peor probabilidad de TP; mucho → más margen.
    #[inline]
    pub fn time_remaining_bucket(avg_time_remaining_sec: f64) -> usize {
        if avg_time_remaining_sec < 90.0 {
            0
        } else if avg_time_remaining_sec < 200.0 {
            1
        } else {
            2
        }
    }

    #[inline]
    pub fn state_index(
        tp_rate: f64,
        pnl_interval: Decimal,
        avg_spread: f64,
        avg_p_strong_at_entry: f64,
        total_trades: u32,
        consecutive_sl_streak: u32,
        avg_time_remaining_sec: f64,
    ) -> usize {
        let tr_b = Self::time_remaining_bucket(avg_time_remaining_sec);
        let tp_b = if tp_rate < 0.34 {
            0usize
        } else if tp_rate < 0.58 {
            1usize
        } else {
            2usize
        };
        let pnlf = pnl_interval.to_f64().unwrap_or(0.0);
        let pnlb = if pnlf < -3.0 {
            0usize
        } else if pnlf <= 3.0 {
            1usize
        } else {
            2usize
        };
        let sp_b = if avg_spread < 0.03 {
            0usize
        } else if avg_spread < 0.08 {
            1usize
        } else {
            2usize
        };
        let ps_b = Self::p_strong_bucket(avg_p_strong_at_entry, total_trades);
        let sl_b = Self::sl_streak_bucket(consecutive_sl_streak);
        tr_b * 243 + tp_b * 81 + pnlb * 27 + sp_b * 9 + ps_b * 3 + sl_b
    }

    pub fn reward_interval_profit_max(
        pnl: Decimal,
        tp_hits: u32,
        sl_hits: u32,
        settles: u32,
        trades_opened: u32,
        pnl_divisor: f64,
        profit_deadline_misses: u32,
        deadline_miss_penalty_each: f64,
    ) -> f64 {
        let closed = tp_hits.saturating_add(sl_hits).saturating_add(settles);
        let p = pnl.to_f64().unwrap_or(0.0);
        let div = pnl_divisor.max(0.05);
        let pnl_r = (p / div).tanh();
        let tp_r = if closed > 0 {
            tp_hits as f64 / closed as f64
        } else {
            0.0
        };
        let idle = if trades_opened == 0 { -0.07 } else { 0.0 };
        let base = 2.4 * pnl_r
            + 0.52 * (2.0 * tp_r - 1.0)
            - 0.52 * sl_hits as f64
            - 0.15 * settles as f64
            + idle;
        let miss = profit_deadline_misses as f64 * deadline_miss_penalty_each.max(0.0);
        base - miss
    }

    fn q_get(&self, s: usize, a: usize) -> f64 {
        self.q[s * NUM_ACTIONS + a]
    }

    fn q_set(&mut self, s: usize, a: usize, v: f64) {
        self.q[s * NUM_ACTIONS + a] = v;
    }

    pub fn max_q_next(&self, s: usize) -> f64 {
        (0..NUM_ACTIONS)
            .map(|a| self.q_get(s, a))
            .fold(f64::NEG_INFINITY, f64::max)
    }

    pub fn learn_verbose(&mut self, s: usize, a: usize, r: f64, s_next: usize) -> (f64, f64, f64) {
        let max_next = self.max_q_next(s_next);
        let q_old = self.q_get(s, a);
        let alpha = self.cfg.alpha;
        let gamma = self.cfg.gamma;
        let target = r + gamma * max_next;
        let q_new = q_old + alpha * (target - q_old);
        self.q_set(s, a, q_new);
        self.epsilon = (self.epsilon * self.cfg.epsilon_decay).max(self.cfg.epsilon_min);
        (q_old, q_new, max_next)
    }

    pub fn pick_action(&mut self, s: usize) -> usize {
        let eps = self.epsilon;
        if self.rng.gen_range(0.0..1.0) < eps {
            return self.rng.gen_range(0..NUM_ACTIONS);
        }
        let mut best = 0usize;
        let mut best_v = self.q_get(s, 0);
        for a in 1..NUM_ACTIONS {
            let v = self.q_get(s, a);
            if v > best_v {
                best_v = v;
                best = a;
            }
        }
        best
    }

    pub fn action_label(action: usize) -> &'static str {
        match action {
            0 => "tighten_delta",
            1 => "loosen_delta",
            2 => "hold",
            3 => "raise_edge",
            4 => "lower_edge",
            5 => "raise_imbalance",
            6 => "lower_imbalance",
            7 => "raise_prob_scale",
            8 => "lower_prob_scale",
            9 => "widen_tp_sl",
            10 => "narrow_tp_sl",
            11 => "shift_tp_up",
            12 => "tighten_sl",
            _ => "hold",
        }
    }

    /// Aplica acción con **bounds** documentados (TP/SL en “ticks” 0.01 = 1¢; prob_scale [5,20]; imbalance [0,0.40]).
    pub fn apply_action(
        action: usize,
        delta_step: Decimal,
        edge_step: Decimal,
        min_pct: Decimal,
        max_pct: Decimal,
        edge_floor: Decimal,
        edge_ceil: Decimal,
        st: &mut RlTuningState,
    ) {
        const ONE_TICK: Decimal = dec!(0.01);
        const TP_MIN: Decimal = dec!(0.01);
        const TP_MAX: Decimal = dec!(0.08);
        const SL_MIN: Decimal = dec!(0.01);
        const SL_MAX: Decimal = dec!(0.10);
        const PROB_MIN: f64 = 100.0;
        const PROB_MAX: f64 = 500.0;
        const IMB_MAX: f64 = 0.40;

        match action {
            0 => {
                st.delta_up_pct = (st.delta_up_pct + delta_step).min(max_pct);
                st.delta_down_pct = (st.delta_down_pct + delta_step).min(max_pct);
            }
            1 => {
                st.delta_up_pct = (st.delta_up_pct - delta_step).max(min_pct);
                st.delta_down_pct = (st.delta_down_pct - delta_step).max(min_pct);
            }
            3 => {
                st.edge_min = (st.edge_min + edge_step).min(edge_ceil);
            }
            4 => {
                st.edge_min = (st.edge_min - edge_step).max(edge_floor);
            }
            5 => {
                st.min_taker_imbalance = (st.min_taker_imbalance + 0.05_f64).min(IMB_MAX);
            }
            6 => {
                st.min_taker_imbalance = (st.min_taker_imbalance - 0.05_f64).max(0.0);
            }
            7 => {
                st.prob_scale = (st.prob_scale + 25.0).clamp(PROB_MIN, PROB_MAX);
            }
            8 => {
                st.prob_scale = (st.prob_scale - 25.0).clamp(PROB_MIN, PROB_MAX);
            }
            9 => {
                if let Some(tp) = st.take_profit_ticks.as_mut() {
                    *tp = (*tp + ONE_TICK).clamp(TP_MIN, TP_MAX);
                }
                if let Some(sl) = st.stop_loss_ticks.as_mut() {
                    *sl = (*sl + ONE_TICK).clamp(SL_MIN, SL_MAX);
                }
            }
            10 => {
                if let Some(tp) = st.take_profit_ticks.as_mut() {
                    *tp = (*tp - ONE_TICK).clamp(TP_MIN, TP_MAX);
                }
                if let Some(sl) = st.stop_loss_ticks.as_mut() {
                    *sl = (*sl - ONE_TICK).clamp(SL_MIN, SL_MAX);
                }
            }
            11 => {
                if let Some(tp) = st.take_profit_ticks.as_mut() {
                    *tp = (*tp + ONE_TICK).clamp(TP_MIN, TP_MAX);
                }
            }
            12 => {
                if let Some(sl) = st.stop_loss_ticks.as_mut() {
                    *sl = (*sl - ONE_TICK).clamp(SL_MIN, SL_MAX);
                }
            }
            _ => {}
        }
    }

    #[inline]
    pub fn exploration_epsilon(&self) -> f64 {
        self.epsilon
    }

    pub fn save(&self) -> Result<()> {
        if let Some(dir) = self.persist_path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("mkdir {}", dir.display()))?;
        }
        let p = QTablePersist {
            version: 4,
            q: self.q.clone(),
            epsilon: self.epsilon,
        };
        let json = serde_json::to_string_pretty(&p)?;
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
