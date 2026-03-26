//! Paper mode: métricas por intervalo, log de lag CEX vs libro, ajuste suave de umbrales de momentum.

use crate::config::{AdaptivePaperConfig, MomentumConfig, Mode, TradingConfig};
use crate::rl::{DeltaQAgent, RlTuningState};
use anyhow::{Context, Result};
use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::{json, Value};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};
use rust_decimal::prelude::{FromPrimitive, ToPrimitive};

#[derive(Debug, Default, Clone)]
struct IntervalAgg {
    interval_start_unix: u64,
    slug: String,
    trades_opened: u32,
    take_profit_hits: u32,
    stop_loss_hits: u32,
    settles_without_tp: u32,
    pnl_usdc: Decimal,
    spread_sum: Decimal,
    spread_samples: u32,
    /// Suma de `p_strong` al entrar (solo momentum paper).
    p_strong_sum: f64,
    p_strong_n: u32,
}

#[derive(Debug, Clone, Copy, Default)]
struct IntervalTuneSample {
    trades_opened: u32,
    take_profit_hits: u32,
    stop_loss_hits: u32,
    settles_without_tp: u32,
    /// Conservado para ventanas heurísticas / diagnóstico futuro.
    #[allow(dead_code)]
    pnl_usdc: Decimal,
}

#[derive(Debug, Clone)]
struct AdaptiveInner {
    delta_up_pct: Decimal,
    delta_down_pct: Decimal,
    edge_min: Decimal,
    min_taker_imbalance: f64,
    prob_scale: f64,
    take_profit_ticks: Option<Decimal>,
    stop_loss_ticks: Option<Decimal>,
    trailing_stop_loss_ticks: Option<Decimal>,
    sl_cooldown_ms: u64,
    /// Racha de SL en paper (resetea en TP / settle).
    consecutive_sl_streak: u32,
    /// Última acción RL (`idx:label`) tras el paso por intervalo; se copia al `trades.jsonl` en la entrada.
    last_rl_action_str: Option<String>,
    intervals_since_tune: u32,
    tune_window: Vec<IntervalTuneSample>,
}

/// Laboratorio paper: reportes por franja + tuning online de `delta_*`.
pub struct PaperLab {
    cfg: AdaptivePaperConfig,
    report_dir: PathBuf,
    current: Mutex<IntervalAgg>,
    adaptive: Mutex<AdaptiveInner>,
    last_analysis_ms: AtomicU64,
    /// Q-learning en paper (opcional); excluyente con la rama heurística en `maybe_tune`.
    rl: Option<Mutex<DeltaQAgent>>,
}

impl PaperLab {
    pub fn new(cfg: AdaptivePaperConfig) -> Result<Arc<Self>> {
        let report_dir = PathBuf::from(cfg.report_dir.trim());
        fs::create_dir_all(&report_dir).with_context(|| format!("crear {}", report_dir.display()))?;
        let rl = match cfg.rl.as_ref() {
            Some(rlc) if rlc.enabled => Some(Mutex::new(DeltaQAgent::new(&cfg, rlc.clone())?)),
            _ => None,
        };
        Ok(Arc::new(Self {
            report_dir,
            cfg,
            current: Mutex::new(IntervalAgg::default()),
            adaptive: Mutex::new(AdaptiveInner {
                delta_up_pct: Decimal::ZERO,
                delta_down_pct: Decimal::ZERO,
                edge_min: Decimal::ZERO,
                min_taker_imbalance: 0.0,
                prob_scale: 0.0,
                take_profit_ticks: None,
                stop_loss_ticks: None,
                trailing_stop_loss_ticks: None,
                sl_cooldown_ms: 500,
                consecutive_sl_streak: 0,
                last_rl_action_str: None,
                intervals_since_tune: 0,
                tune_window: Vec::new(),
            }),
            last_analysis_ms: AtomicU64::new(0),
            rl,
        }))
    }

    /// Llamar **antes** de cambiar `last_logged_btc_5m_start` al nuevo intervalo.
    /// `prev_interval_start`: Some(anterior) si había franja activa.
    pub fn rotate_interval(&self, prev_interval_start: Option<u64>, new_interval: u64, slug: &str) {
        if let Some(prev) = prev_interval_start {
            if prev != new_interval {
                let _ = self.flush_interval_report(prev);
            }
        }
        let mut g = self.current.lock().expect("paper_lab current");
        *g = IntervalAgg {
            interval_start_unix: new_interval,
            slug: slug.to_string(),
            trades_opened: 0,
            take_profit_hits: 0,
            stop_loss_hits: 0,
            settles_without_tp: 0,
            pnl_usdc: Decimal::ZERO,
            spread_sum: Decimal::ZERO,
            spread_samples: 0,
            p_strong_sum: 0.0,
            p_strong_n: 0,
        };
    }

    fn flush_interval_report(&self, interval_start: u64) -> Result<()> {
        let agg = {
            let g = self.current.lock().expect("paper_lab current");
            if g.interval_start_unix != interval_start {
                return Ok(());
            }
            g.clone()
        };

        let adap = self.adaptive.lock().expect("paper_lab adaptive").clone();

        #[derive(Serialize)]
        struct Report<'a> {
            interval_start_unix: u64,
            slug: &'a str,
            trades_opened: u32,
            take_profit_hits: u32,
            stop_loss_hits: u32,
            settles_without_tp_without_prior_tp: u32,
            pnl_realized_in_interval_usdc: String,
            momentum_delta_up_pct: String,
            momentum_delta_down_pct: String,
            note: &'a str,
        }

        let closed = agg.take_profit_hits + agg.stop_loss_hits + agg.settles_without_tp;
        let tp_rate = if closed > 0 {
            agg.take_profit_hits as f64 / closed as f64
        } else {
            0.0
        };

        let rep = Report {
            interval_start_unix: agg.interval_start_unix,
            slug: agg.slug.as_str(),
            trades_opened: agg.trades_opened,
            take_profit_hits: agg.take_profit_hits,
            stop_loss_hits: agg.stop_loss_hits,
            settles_without_tp_without_prior_tp: agg.settles_without_tp,
            pnl_realized_in_interval_usdc: agg.pnl_usdc.to_string(),
            momentum_delta_up_pct: adap.delta_up_pct.to_string(),
            momentum_delta_down_pct: adap.delta_down_pct.to_string(),
            note: "tp = TP ganador; stop_loss = salida por bid bajo umbral; settle = fin de intervalo sin TP/SL",
        };

        let path = self
            .report_dir
            .join(format!("interval_{interval_start}.json"));
        let json = serde_json::to_string_pretty(&rep)?;
        fs::write(&path, json).with_context(|| format!("escribir {}", path.display()))?;

        tracing::info!(
            target: "sniper",
            path = %path.display(),
            trades = agg.trades_opened,
            tp_hits = agg.take_profit_hits,
            sl_hits = agg.stop_loss_hits,
            settle_no_tp = agg.settles_without_tp,
            tp_rate = format!("{tp_rate:.2}"),
            pnl = %agg.pnl_usdc,
            "paper_lab · reporte de intervalo"
        );

        let sample = IntervalTuneSample {
            trades_opened: agg.trades_opened,
            take_profit_hits: agg.take_profit_hits,
            stop_loss_hits: agg.stop_loss_hits,
            settles_without_tp: agg.settles_without_tp,
            pnl_usdc: agg.pnl_usdc,
        };

        if self.rl.is_some() {
            self.rl_step_interval(&agg)?;
        } else {
            self.maybe_tune(adap, sample);
        }
        Ok(())
    }

    fn resolve_rl_path(&self, rel: &str) -> PathBuf {
        let p = rel.trim();
        if Path::new(p).is_absolute() {
            PathBuf::from(p)
        } else {
            self.report_dir.join(p)
        }
    }

    /// Q-learning **cada intervalo** (5m): Q-update, acción, tunables completos, log.
    fn rl_step_interval(&self, agg: &IntervalAgg) -> Result<()> {
        let Some(ref rl_mtx) = self.rl else {
            return Ok(());
        };
        let rlc = self
            .cfg
            .rl
            .as_ref()
            .expect("rl_mutex implies rl config");

        let closed = agg.take_profit_hits + agg.stop_loss_hits + agg.settles_without_tp;
        let tp_rate = if closed > 0 {
            agg.take_profit_hits as f64 / closed as f64
        } else {
            0.0
        };
        let avg_spread = if agg.spread_samples > 0 {
            (agg.spread_sum / Decimal::from(agg.spread_samples))
                .to_f64()
                .unwrap_or(0.05)
        } else {
            0.05
        };
        let avg_p_strong = if agg.p_strong_n > 0 {
            agg.p_strong_sum / f64::from(agg.p_strong_n)
        } else {
            0.0
        };

        let mut ad = self.adaptive.lock().expect("paper_lab adaptive");
        let streak = ad.consecutive_sl_streak;
        let s_next = DeltaQAgent::state_index(
            agg.trades_opened,
            tp_rate,
            agg.pnl_usdc,
            avg_spread,
            self.cfg.low_activity_trade_ceiling,
            avg_p_strong,
            streak,
        );
        let r = DeltaQAgent::reward_interval_profit_max(
            agg.pnl_usdc,
            agg.take_profit_hits,
            agg.stop_loss_hits,
            agg.settles_without_tp,
            agg.trades_opened,
            rlc.pnl_reward_divisor,
        );

        let mut rl = rl_mtx.lock().expect("rl agent");

        let frac = Decimal::from_f64(rlc.interval_step_fraction)
            .unwrap_or_else(|| Decimal::new(42, 2) / Decimal::new(100, 0));
        let delta_step = self.cfg.delta_tune_step * frac;
        let edge_step = delta_step;

        let (q_old, q_new, q_bootstrap) = if let (Some(ls), Some(la)) = (rl.last_state, rl.last_action) {
            rl.learn_verbose(ls, la, r, s_next)
        } else {
            (f64::NAN, f64::NAN, rl.max_q_next(s_next))
        };

        let action = rl.pick_action(s_next);

        let up0 = ad.delta_up_pct;
        let down0 = ad.delta_down_pct;
        let edge0 = ad.edge_min;
        let imb0 = ad.min_taker_imbalance;
        let prob0 = ad.prob_scale;
        let tp0 = ad.take_profit_ticks;
        let sl0 = ad.stop_loss_ticks;
        let tr0 = ad.trailing_stop_loss_ticks;
        let cd0 = ad.sl_cooldown_ms;

        use rust_decimal_macros::dec;
        const EDGE_FLOOR: Decimal = dec!(0.005);
        const EDGE_CEIL: Decimal = dec!(0.15);

        let mut st = RlTuningState {
            delta_up_pct: ad.delta_up_pct,
            delta_down_pct: ad.delta_down_pct,
            edge_min: ad.edge_min,
            min_taker_imbalance: ad.min_taker_imbalance,
            prob_scale: ad.prob_scale,
            take_profit_ticks: ad.take_profit_ticks,
            stop_loss_ticks: ad.stop_loss_ticks,
            trailing_stop_loss_ticks: ad.trailing_stop_loss_ticks,
            sl_cooldown_ms: ad.sl_cooldown_ms,
        };
        DeltaQAgent::apply_action(
            action,
            delta_step,
            edge_step,
            self.cfg.delta_min_pct,
            self.cfg.delta_max_pct,
            EDGE_FLOOR,
            EDGE_CEIL,
            &mut st,
        );
        ad.delta_up_pct = st.delta_up_pct;
        ad.delta_down_pct = st.delta_down_pct;
        ad.edge_min = st.edge_min;
        ad.min_taker_imbalance = st.min_taker_imbalance;
        ad.prob_scale = st.prob_scale;
        ad.take_profit_ticks = st.take_profit_ticks;
        ad.stop_loss_ticks = st.stop_loss_ticks;
        ad.trailing_stop_loss_ticks = st.trailing_stop_loss_ticks;
        ad.sl_cooldown_ms = st.sl_cooldown_ms.clamp(500, 30_000);
        ad.last_rl_action_str = Some(format!("{}:{}", action, DeltaQAgent::action_label(action)));

        rl.last_state = Some(s_next);
        rl.last_action = Some(action);

        let wall_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let (ba, bt, bp, bs, bps, bsl) = DeltaQAgent::decode_state(s_next);

        if rlc.log_interval_jsonl {
            let jsonl_path = self.resolve_rl_path(&rlc.interval_log_jsonl_path);
            let core = json!({
                "schema": "sniper.rl_interval.v3",
                "wall_time_ms": wall_ms,
                "interval_start_unix": agg.interval_start_unix,
                "slug": &agg.slug,
                "state": s_next,
                "bucket_activity": ba,
                "bucket_tp": bt,
                "bucket_pnl": bp,
                "bucket_spread": bs,
                "bucket_p_strong": bps,
                "bucket_sl_streak": bsl,
                "consecutive_sl_streak": streak,
                "avg_p_strong_at_entry": avg_p_strong,
                "avg_spread": avg_spread,
                "reward": r,
                "objective": "maximize_pnl_usdc_primary",
                "action": action,
                "action_name": DeltaQAgent::action_label(action),
                "epsilon": rl.exploration_epsilon(),
            });
            let qjson = json!({
                "q_sa_before": if q_old.is_nan() { Value::Null } else { json!(q_old) },
                "q_sa_after": if q_new.is_nan() { Value::Null } else { json!(q_new) },
                "q_bootstrap_max_s_next": q_bootstrap,
            });
            let deltas = json!({
                "delta_up_before": up0.to_string(),
                "delta_up_after": ad.delta_up_pct.to_string(),
                "delta_down_before": down0.to_string(),
                "delta_down_after": ad.delta_down_pct.to_string(),
                "edge_min_before": edge0.to_string(),
                "edge_min_after": ad.edge_min.to_string(),
                "min_taker_imb_before": imb0,
                "min_taker_imb_after": ad.min_taker_imbalance,
                "prob_scale_before": prob0,
                "prob_scale_after": ad.prob_scale,
                "tp_ticks_before": tp0.map(|d| d.to_string()),
                "tp_ticks_after": ad.take_profit_ticks.map(|d| d.to_string()),
                "sl_ticks_before": sl0.map(|d| d.to_string()),
                "sl_ticks_after": ad.stop_loss_ticks.map(|d| d.to_string()),
                "trail_ticks_before": tr0.map(|d| d.to_string()),
                "trail_ticks_after": ad.trailing_stop_loss_ticks.map(|d| d.to_string()),
                "sl_cooldown_ms_before": cd0,
                "sl_cooldown_ms_after": ad.sl_cooldown_ms,
                "delta_step_base": self.cfg.delta_tune_step.to_string(),
                "delta_step_applied": delta_step.to_string(),
                "interval_step_fraction": rlc.interval_step_fraction,
                "pnl_interval_usdc": agg.pnl_usdc.to_string(),
                "trades_opened": agg.trades_opened,
                "take_profit_hits": agg.take_profit_hits,
                "stop_loss_hits": agg.stop_loss_hits,
                "settles_without_tp": agg.settles_without_tp,
                "tp_rate_interval": tp_rate,
                "closed_outcomes": closed,
                "pnl_reward_divisor": rlc.pnl_reward_divisor,
            });
            let mut line = core.as_object().cloned().unwrap_or_default();
            if let Some(o) = qjson.as_object() {
                line.extend(o.clone());
            }
            if let Some(o) = deltas.as_object() {
                line.extend(o.clone());
            }
            let line = Value::Object(line);
            let mut f = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&jsonl_path)
                .with_context(|| format!("rl jsonl {}", jsonl_path.display()))?;
            writeln!(f, "{}", line).with_context(|| format!("rl jsonl write {}", jsonl_path.display()))?;
        }

        if rlc.log_interval_csv {
            let csv_path = self.resolve_rl_path(&rlc.interval_log_csv_path);
            let need_header = !csv_path.exists()
                || fs::metadata(&csv_path)
                    .map(|m| m.len() == 0)
                    .unwrap_or(true);
            let mut f = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&csv_path)
                .with_context(|| format!("rl csv {}", csv_path.display()))?;
            if need_header {
                writeln!(f, "wall_ms,interval_start,slug,state,b_act,b_tp,b_pnl,b_sp,b_ps,b_slst,streak,avg_sp,avg_ps,rwd,act,act_name,eps,q_b,q_a,q_boot,du0,du1,dd0,dd1,e0,e1,imb0,imb1,pb,pa,tp0,tp1,sl0,sl1,cd0,cd1,step,pnl,tr,tp,sl,st,tpr,cl")?;
            }
            let qb = if q_old.is_nan() { "".to_string() } else { format!("{q_old:.6}") };
            let qa = if q_new.is_nan() { "".to_string() } else { format!("{q_new:.6}") };
            let tp0s = tp0.map(|d| d.to_string()).unwrap_or_default();
            let tp1s = ad.take_profit_ticks.map(|d| d.to_string()).unwrap_or_default();
            let sl0s = sl0.map(|d| d.to_string()).unwrap_or_default();
            let sl1s = ad.stop_loss_ticks.map(|d| d.to_string()).unwrap_or_default();
            let row = vec![
                wall_ms.to_string(),
                agg.interval_start_unix.to_string(),
                agg.slug.replace('"', "'"),
                s_next.to_string(),
                ba.to_string(),
                bt.to_string(),
                bp.to_string(),
                bs.to_string(),
                bps.to_string(),
                bsl.to_string(),
                streak.to_string(),
                format!("{avg_spread:.6}"),
                format!("{avg_p_strong:.6}"),
                format!("{r:.8}"),
                action.to_string(),
                DeltaQAgent::action_label(action).to_string(),
                format!("{:.6}", rl.exploration_epsilon()),
                qb,
                qa,
                format!("{q_bootstrap:.6}"),
                up0.to_string(),
                ad.delta_up_pct.to_string(),
                down0.to_string(),
                ad.delta_down_pct.to_string(),
                edge0.to_string(),
                ad.edge_min.to_string(),
                format!("{imb0:.6}"),
                format!("{:.6}", ad.min_taker_imbalance),
                format!("{prob0:.4}"),
                format!("{:.4}", ad.prob_scale),
                tp0s,
                tp1s,
                sl0s,
                sl1s,
                cd0.to_string(),
                ad.sl_cooldown_ms.to_string(),
                delta_step.to_string(),
                agg.pnl_usdc.to_string(),
                agg.trades_opened.to_string(),
                agg.take_profit_hits.to_string(),
                agg.stop_loss_hits.to_string(),
                agg.settles_without_tp.to_string(),
                format!("{tp_rate:.6}"),
                closed.to_string(),
            ];
            writeln!(f, "{}", row.join(","))?;
        }

        drop(ad);
        if let Err(e) = rl.save() {
            tracing::warn!(target: "sniper", error = %e, "rl · persist Q-table falló");
        }

        tracing::info!(
            target: "sniper",
            state = s_next,
            action,
            reward = format!("{r:.4}"),
            epsilon = format!("{:.4}", rl.exploration_epsilon()),
            delta_up = %st.delta_up_pct,
            delta_down = %st.delta_down_pct,
            edge_min = %st.edge_min,
            prob_scale = %st.prob_scale,
            min_taker = %st.min_taker_imbalance,
            pnl = %agg.pnl_usdc,
            "rl · intervalo · Q-learn (rentabilidad)"
        );

        Ok(())
    }

    fn maybe_tune(&self, mut adap: AdaptiveInner, last: IntervalTuneSample) {
        adap.intervals_since_tune = adap.intervals_since_tune.saturating_add(1);
        adap.tune_window.push(last);
        if adap.intervals_since_tune < self.cfg.tune_every_n_intervals {
            let mut g = self.adaptive.lock().expect("paper_lab adaptive");
            *g = adap;
            return;
        }
        adap.intervals_since_tune = 0;
        let window = std::mem::take(&mut adap.tune_window);

        let total_trades: u32 = window.iter().map(|s| s.trades_opened).sum();
        let tp_hits: u32 = window.iter().map(|s| s.take_profit_hits).sum();
        let sl_hits: u32 = window.iter().map(|s| s.stop_loss_hits).sum();
        let settles: u32 = window.iter().map(|s| s.settles_without_tp).sum();
        let closed = tp_hits.saturating_add(sl_hits).saturating_add(settles);
        let tp_rate = if closed > 0 {
            tp_hits as f64 / closed as f64
        } else {
            0.0
        };

        let step = self.cfg.delta_tune_step;
        let mut up = adap.delta_up_pct;
        let mut down = adap.delta_down_pct;

        if total_trades <= self.cfg.low_activity_trade_ceiling {
            up = (up - step).max(self.cfg.delta_min_pct);
            down = (down - step).max(self.cfg.delta_min_pct);
            tracing::info!(
                target: "sniper",
                new_up = %up,
                new_down = %down,
                total_trades,
                window_intervals = window.len(),
                ceiling = self.cfg.low_activity_trade_ceiling,
                "paper_lab · suavizar deltas (poca actividad en la ventana)"
            );
        } else if closed >= 2 && tp_rate < 0.25 {
            up = (up + step).min(self.cfg.delta_max_pct);
            down = (down + step).min(self.cfg.delta_max_pct);
            tracing::info!(
                target: "sniper",
                new_up = %up,
                new_down = %down,
                tp_rate,
                total_trades,
                "paper_lab · endurecer deltas (bajo éxito TP)"
            );
        } else if closed >= 2 && tp_rate > 0.6 {
            up = (up - step).max(self.cfg.delta_min_pct);
            down = (down - step).max(self.cfg.delta_min_pct);
            tracing::info!(
                target: "sniper",
                new_up = %up,
                new_down = %down,
                tp_rate,
                total_trades,
                "paper_lab · suavizar deltas (buen éxito TP)"
            );
        }

        adap.delta_up_pct = up;
        adap.delta_down_pct = down;
        let mut g = self.adaptive.lock().expect("paper_lab adaptive");
        *g = adap;
    }

    /// Clonar `MomentumConfig` sustituyendo deltas, `prob_scale` y `min_taker_imbalance` afinados.
    pub fn effective_momentum(&self, base: &MomentumConfig) -> MomentumConfig {
        let adap = self.adaptive.lock().expect("paper_lab adaptive");
        let mut m = base.clone();
        m.delta_up_pct = adap.delta_up_pct;
        m.delta_down_pct = adap.delta_down_pct;
        m.min_taker_imbalance = adap.min_taker_imbalance;
        m.prob_scale = adap.prob_scale;
        m
    }

    pub fn bootstrap_adaptive_from_config(&self, momentum: &MomentumConfig, trading: &TradingConfig) {
        let mut g = self.adaptive.lock().expect("paper_lab adaptive");
        g.delta_up_pct = momentum.delta_up_pct;
        g.delta_down_pct = momentum.delta_down_pct;
        g.edge_min = trading.edge_min;
        g.min_taker_imbalance = momentum.min_taker_imbalance;
        g.prob_scale = momentum.prob_scale;
        g.take_profit_ticks = trading.take_profit_ticks;
        g.stop_loss_ticks = trading.stop_loss_ticks;
        g.trailing_stop_loss_ticks = trading.trailing_stop_loss_ticks;
        g.sl_cooldown_ms = trading.sl_cooldown_ms.unwrap_or(5000).max(500).min(30_000);
        g.consecutive_sl_streak = 0;
        g.last_rl_action_str = None;
    }

    pub fn effective_take_profit_ticks(&self, base: Option<Decimal>) -> Option<Decimal> {
        let adap = self.adaptive.lock().expect("paper_lab adaptive");
        adap.take_profit_ticks.or(base)
    }

    pub fn effective_stop_loss_ticks(&self, base: Option<Decimal>) -> Option<Decimal> {
        let adap = self.adaptive.lock().expect("paper_lab adaptive");
        adap.stop_loss_ticks.or(base)
    }

    pub fn effective_trailing_stop_loss_ticks(&self, base: Option<Decimal>) -> Option<Decimal> {
        let adap = self.adaptive.lock().expect("paper_lab adaptive");
        adap.trailing_stop_loss_ticks.or(base)
    }

    /// Cooldown post-SL (ms) ajustado por RL y rachas; siempre acotado [500, 30_000].
    pub fn effective_sl_cooldown_ms(&self) -> u64 {
        let adap = self.adaptive.lock().expect("paper_lab adaptive");
        adap.sl_cooldown_ms.clamp(500, 30_000)
    }

    /// Atribución paper: última acción RL tras el tick de intervalo (para `trades.jsonl`).
    pub fn last_rl_action_for_diag(&self) -> Option<String> {
        self.adaptive
            .lock()
            .expect("paper_lab adaptive")
            .last_rl_action_str
            .clone()
    }

    pub fn effective_edge_min(&self, base: Decimal) -> Decimal {
        let adap = self.adaptive.lock().expect("paper_lab adaptive");
        if adap.edge_min.is_zero() { base } else { adap.edge_min }
    }

    pub fn record_spread_sample(&self, interval_start_unix: u64, spread: Decimal) {
        let mut g = self.current.lock().expect("paper_lab current");
        if g.interval_start_unix != interval_start_unix {
            return;
        }
        g.spread_sum += spread;
        g.spread_samples = g.spread_samples.saturating_add(1);
    }

    pub fn record_paper_entry(&self, interval_start_unix: u64, p_strong: f64) {
        let mut g = self.current.lock().expect("paper_lab current");
        if g.interval_start_unix != interval_start_unix {
            return;
        }
        g.trades_opened = g.trades_opened.saturating_add(1);
        g.p_strong_sum += p_strong;
        g.p_strong_n = g.p_strong_n.saturating_add(1);
    }

    pub fn record_take_profit(&self, interval_start_unix: u64, pnl: Decimal) {
        {
            let mut g = self.current.lock().expect("paper_lab current");
            if g.interval_start_unix != interval_start_unix {
                return;
            }
            g.take_profit_hits = g.take_profit_hits.saturating_add(1);
            g.pnl_usdc += pnl;
        }
        let mut ad = self.adaptive.lock().expect("paper_lab adaptive");
        ad.consecutive_sl_streak = 0;
    }

    pub fn record_stop_loss(&self, interval_start_unix: u64, pnl: Decimal) {
        {
            let mut g = self.current.lock().expect("paper_lab current");
            if g.interval_start_unix != interval_start_unix {
                return;
            }
            g.stop_loss_hits = g.stop_loss_hits.saturating_add(1);
            g.pnl_usdc += pnl;
        }
        let mut ad = self.adaptive.lock().expect("paper_lab adaptive");
        ad.consecutive_sl_streak = ad.consecutive_sl_streak.saturating_add(1);
        if ad.consecutive_sl_streak >= 2 {
            ad.sl_cooldown_ms = (ad.sl_cooldown_ms.saturating_add(2000)).min(30_000).max(500);
        }
    }

    pub fn record_settle_no_tp(&self, interval_start_unix: u64, pnl: Decimal) {
        {
            let mut g = self.current.lock().expect("paper_lab current");
            if g.interval_start_unix != interval_start_unix {
                return;
            }
            g.settles_without_tp = g.settles_without_tp.saturating_add(1);
            g.pnl_usdc += pnl;
        }
        let mut ad = self.adaptive.lock().expect("paper_lab adaptive");
        ad.consecutive_sl_streak = 0;
    }

    /// Una línea JSONL con lag estimado CEX vs libro e impulso reciente (solo paper).
    pub fn maybe_log_analysis(
        &self,
        wall_ms: u64,
        interval_start: u64,
        slug: &str,
        bin_price: Decimal,
        anchor: Decimal,
        pct_vs_anchor: f64,
        book_ts_ms: u64,
        cex_ts_ms: u64,
        impulse_abs: Decimal,
        p_up: Decimal,
        p_down: Decimal,
        lag_opportunity: bool,
    ) {
        if !self.cfg.analysis_jsonl {
            return;
        }
        let tick = self.cfg.analysis_tick_ms.max(100);
        let last = self.last_analysis_ms.load(Ordering::Relaxed);
        if wall_ms.saturating_sub(last) < tick {
            return;
        }
        self.last_analysis_ms.store(wall_ms, Ordering::Relaxed);

        if pct_vs_anchor.abs() > 5.0 {
            let drift = (bin_price - anchor).abs();
            tracing::warn!(
                target: "sniper",
                interval_start_unix = interval_start,
                slug,
                pct_vs_anchor,
                anchor_ref = %anchor.round_dp(2),
                binance_mid_proxy = %bin_price.round_dp(2),
                drift_usd = %drift.round_dp(2),
                "paper · |pct_vs_anchor| > 5% — si drift REF vs spot no cuadra con el gráfico, la ancla puede estar mal (ver log BTC 5m · ventana_nueva)"
            );
        }

        let lag_ms = cex_ts_ms as i64 - book_ts_ms as i64;
        let line = serde_json::json!({
            "ts_ms": wall_ms,
            "interval_start_unix": interval_start,
            "slug": slug,
            "binance_mid_proxy": bin_price.to_string(),
            "anchor_venue_avg": anchor.to_string(),
            "pct_vs_anchor": pct_vs_anchor,
            "book_last_ts_ms": book_ts_ms,
            "cex_last_ts_ms": cex_ts_ms,
            "lag_cex_minus_book_ms": lag_ms,
            "impulse_abs_fraction": impulse_abs.to_string(),
            "p_up_ask": p_up.to_string(),
            "p_down_ask": p_down.to_string(),
            "lag_opportunity_flag": lag_opportunity,
        });

        let path = self.report_dir.join("analysis.jsonl");
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&path) {
            let _ = writeln!(f, "{}", line);
        }
    }

    /// |Δprecio| relativo en la ventana (misma escala que `momentum.delta_*`: 0.002 = 0,2%).
    pub fn impulse_from_samples(samples_oldest_price: Option<Decimal>, newest_price: Decimal) -> Decimal {
        let Some(old) = samples_oldest_price else {
            return Decimal::ZERO;
        };
        if old.is_zero() {
            return Decimal::ZERO;
        }
        ((newest_price / old) - Decimal::ONE).abs()
    }

    pub fn lag_opportunity(
        &self,
        impulse_abs: Decimal,
        book_ts_ms: u64,
        cex_ts_ms: u64,
    ) -> (bool, i64) {
        let lag = cex_ts_ms as i64 - book_ts_ms as i64;
        let imp_ok = impulse_abs >= self.cfg.impulse_min_pct;
        let lag_ok = lag >= self.cfg.lag_signal_min_ms as i64 && lag <= self.cfg.lag_signal_max_ms as i64;
        (imp_ok && lag_ok, lag)
    }

    #[inline]
    pub fn impulse_window_ms(&self) -> u64 {
        self.cfg.impulse_window_ms
    }
}

pub fn should_run_adaptive(cfg_mode: &Mode, adaptive: &Option<AdaptivePaperConfig>) -> bool {
    matches!(cfg_mode, Mode::Paper) && adaptive.as_ref().map(|c| c.enabled).unwrap_or(false)
}
