use anyhow::{Context, Result};
use rust_decimal::Decimal;
use serde::Deserialize;
use std::path::Path;
use std::str::FromStr;

/// Bot runtime mode.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// No real orders are posted. Fills are simulated using current best bid/ask.
    Paper,
    /// Real orders are posted to Polymarket CLOB.
    Live,
}

/// CEX spot feed selection / health fallback.
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum CexMode {
    /// Use Binance unless it becomes stale; then fallback to Coinbase.
    #[default]
    Auto,
    /// Always use Binance.
    BinanceOnly,
    /// Always use Coinbase.
    CoinbaseOnly,
}

/// How to combine momentum from multiple live CEX feeds (solo Binance + Coinbase vía WS; el resto es REST solo ancla).
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum CexMomentumPolicy {
    /// Una sola fuente: Binance si está fresco, si no Coinbase (mismo comportamiento histórico).
    #[default]
    Primary,
    /// Si **ambos** feeds están frescos: exige que **los dos** pasen filtros momentum con la **misma**
    /// dirección (Up/Down); entonces fusiona fair % y volumen. Si solo uno está fresco → igual que `primary`.
    Consensus,
}

/// Signature type for Polymarket wallets (EOA / Proxy / Gnosis Safe).
///
/// This is passed to the SDK auth builder.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SignatureType {
    Eoa,
    Proxy,
    GnosisSafe,
}

/// Polymarket and Gamma discovery configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct Endpoints {
    /// CLOB REST base URL, required for live orders.
    pub clob_base_url: Option<String>,
    /// Gamma API base URL for resolving slugs -> token IDs.
    pub gamma_base_url: Option<String>,
}

/// Polymarket CLOB time-in-force for limit orders (SDK `OrderType`, sin GTD aquí).
#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ClobOrderTimeInForce {
    /// Good-til-cancel: orden resting; combina con `cancel_if_unfilled_ms` para no dejar colgadas.
    Gtc,
    /// Fill-or-kill: todo el tamaño al instante o cancela (típico arb pareado / tamaño fijo).
    Fok,
    /// Fill-and-kill: ejecuta lo que haya al instante y cancela el resto (típico “snipe” momentum / libro fino).
    #[default]
    Fak,
}

fn default_entry_time_in_force() -> ClobOrderTimeInForce {
    ClobOrderTimeInForce::Fak
}

fn default_take_profit_time_in_force() -> ClobOrderTimeInForce {
    ClobOrderTimeInForce::Fak
}

fn default_stop_loss_time_in_force() -> ClobOrderTimeInForce {
    ClobOrderTimeInForce::Fak
}

fn default_max_book_staleness_ms() -> Option<u64> {
    Some(3_000)
}

fn default_min_time_remaining_sec() -> u64 {
    60
}

fn default_max_size_to_ask_ratio() -> Decimal {
    Decimal::ONE
}

fn default_exit_order_timeout_ms() -> u64 {
    15_000
}

fn default_min_taker_imbalance() -> f64 {
    0.0
}

/// Momentum signal configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct MomentumConfig {
    /// Rolling window for momentum (seconds, 3–10). Price comparison and volume use this window.
    pub window_sec: u64,
    /// Delta threshold (fraction): Up triggers when pct_change >= this. Used only when delta_up_usd is None.
    pub delta_up_pct: Decimal,
    /// Delta threshold (fraction): Down triggers when pct_change <= -this. Used only when delta_down_usd is None.
    pub delta_down_pct: Decimal,
    /// Minimum quote volume over `window_sec` (USDT/quote currency).
    pub min_quote_volume_window: Decimal,
    /// Probability mapping scale. Higher => faster probability saturation towards 0 or 1.
    pub prob_scale: f64,
    /// Strong momentum gate: enter only when p_fair for the chosen direction >= this.
    pub strong_prob_threshold: f64,
    /// Mínimo desequilibrio taker normalizado \[-1,1\] alineado con la dirección; 0 = desactivado.
    #[serde(default = "default_min_taker_imbalance")]
    pub min_taker_imbalance: f64,
    /// Dollar-based up threshold (e.g. 50.0). Overrides delta_up_pct when set.
    /// Threshold is converted to pct at runtime using the interval anchor price, so it
    /// automatically adapts as BTC price changes.
    #[serde(default)]
    pub delta_up_usd: Option<Decimal>,
    /// Dollar-based down threshold. Overrides delta_down_pct when set.
    #[serde(default)]
    pub delta_down_usd: Option<Decimal>,
}

/// Trading / edge configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct TradingConfig {
    /// Minimum edge required to enter (e.g. 0.03 = 3%).
    pub edge_min: Decimal,
    /// When false, skip YES+NO sum arbitrage (momentum-only).
    #[serde(default = "default_yes_no_arb_enabled")]
    pub yes_no_arb_enabled: bool,
    /// Additional arbitrage: if p_up_market + p_down_market <= this, buy both sides.
    pub arb_yes_no_sum_max: Decimal,
    /// If true, use post-only for entries (maker). Default false for aggressive fills.
    pub post_only: bool,
    /// Extra price slippage vs best ask (bps). Example: 1 => quote price * 1.0001.
    pub entry_limit_slippage_bps: Decimal,
    /// Cancel entry orders if not filled within this window (milliseconds).
    pub cancel_if_unfilled_ms: u64,
    /// Strategy tick interval: recompute signals every `tick_ms` (≥ 1; use 1 for máxima frecuencia).
    pub tick_ms: u64,
    /// Max active positions per market key (asset+interval).
    pub max_positions_per_market: usize,
    /// Cooldown between entries for the same market (milliseconds).
    pub entry_cooldown_ms: u64,
    /// Tras un stop-loss, tiempo mínimo antes de volver a entrar en ese mercado. `None` = no extra (solo `entry_cooldown_ms`).
    #[serde(default)]
    pub sl_cooldown_ms: Option<u64>,
    /// Si está definido, una línea JSON por trade al cerrar (TP / SL / settle) — path absoluto o relativo al cwd.
    #[serde(default)]
    pub trades_jsonl_path: Option<String>,
    /// Take profit: sell when best bid ≥ fill price + this (outcome price units; 0.02 = 2¢). `None` disables.
    #[serde(default = "default_take_profit_ticks")]
    pub take_profit_ticks: Option<Decimal>,
    /// Live entry (momentum / arb): `fak` suele ser óptimo para capturar desfase CEX–libro con liquidez parcial.
    #[serde(default = "default_entry_time_in_force")]
    pub entry_time_in_force: ClobOrderTimeInForce,
    /// Live take-profit sell: `fak` suele ser óptimo para tomar bid visible sin quedar colgado.
    #[serde(default = "default_take_profit_time_in_force")]
    pub take_profit_time_in_force: ClobOrderTimeInForce,
    /// Stop-loss: si `best_bid <= entry_fill − this`, vender agresivo (misma unidad que TP; ej. 0.03 = 3¢). `None` = desactivado.
    #[serde(default)]
    pub stop_loss_ticks: Option<Decimal>,
    /// TIF para la venta de stop-loss (típico `fak` como el TP).
    #[serde(default = "default_stop_loss_time_in_force")]
    pub stop_loss_time_in_force: ClobOrderTimeInForce,
    /// Max age of orderbook WS data (ms) before rejecting signals/exits. `None` = disabled.
    #[serde(default = "default_max_book_staleness_ms")]
    pub max_book_staleness_ms: Option<u64>,
    /// Max spread (best_ask − best_bid) in outcome price units; skip entry when exceeded. `None` = disabled.
    #[serde(default)]
    pub max_spread_ticks: Option<Decimal>,
    /// Minimum seconds remaining in interval to allow new momentum entries. Default 60.
    #[serde(default = "default_min_time_remaining_sec")]
    pub min_time_remaining_sec: u64,
    /// Trailing stop-loss: sell when bid ≤ high_water_mark − this. `None` = use fixed SL only.
    #[serde(default)]
    pub trailing_stop_loss_ticks: Option<Decimal>,
    /// Cap entry size to `ratio × best_ask_size` for thin-book safety. Default 1.0.
    #[serde(default = "default_max_size_to_ask_ratio")]
    pub max_size_to_ask_ratio: Decimal,
    /// Timeout (ms) for pending exit orders (TP/SL); clear stale id and retry. Default 15 000.
    #[serde(default = "default_exit_order_timeout_ms")]
    pub exit_order_timeout_ms: u64,
}

/// Risk management configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct RiskConfig {
    /// Max fraction of account balance to spend per entry position (0.02..0.05 recommended).
    pub risk_per_trade_frac: Decimal,
    /// Kill switch: stop placing orders if daily P&L <= -daily_drawdown_frac * starting_balance.
    pub daily_drawdown_frac: Decimal,
    /// Kill switch is enabled.
    pub kill_switch_enabled: bool,
}

/// Paper trading configuration.
#[derive(Debug, Clone, Deserialize)]
pub struct PaperConfig {
    /// Virtual USDC starting balance for risk sizing and P&L calculations.
    pub balance_usdc: Decimal,
}

/// Paper-only lab: reports per 5m interval, CEX–book lag logging, soft tuning of momentum deltas.
#[derive(Debug, Clone, Deserialize)]
pub struct AdaptivePaperConfig {
    /// Master switch (only applies when `mode = paper`).
    #[serde(default = "default_adaptive_paper_enabled")]
    pub enabled: bool,
    /// Directory for `interval_*.json` and optional `analysis.jsonl`.
    #[serde(default = "default_adaptive_report_dir")]
    pub report_dir: String,
    /// Append unified analysis lines to `analysis.jsonl` under `report_dir`.
    #[serde(default = "default_analysis_jsonl")]
    pub analysis_jsonl: bool,
    /// Min wall-clock spacing between analysis lines (ms).
    #[serde(default = "default_analysis_tick_ms")]
    pub analysis_tick_ms: u64,
    /// Flag `lag_opportunity` when CEX ts − book ts is in `[lag_signal_min_ms, lag_signal_max_ms]`.
    #[serde(default = "default_lag_signal_min_ms")]
    pub lag_signal_min_ms: u64,
    #[serde(default = "default_lag_signal_max_ms")]
    pub lag_signal_max_ms: u64,
    /// Min |Δprecio| relativo en `impulse_window_ms` (misma escala que `delta_*`, ej. 0.002 = 0,2 %).
    #[serde(default = "default_impulse_min_pct")]
    pub impulse_min_pct: Decimal,
    /// Ventana hacia atrás desde el último trade Binance para medir impulso (ms).
    #[serde(default = "default_impulse_window_ms")]
    pub impulse_window_ms: u64,
    /// Cada cuántos intervalos 5m ajustar `delta_*` (después de escribir el reporte).
    #[serde(default = "default_tune_every_n_intervals")]
    pub tune_every_n_intervals: u32,
    /// Si la suma de `trades_opened` en esa ventana es `<=` este valor, se suavizan los deltas (más señales).
    #[serde(default = "default_low_activity_trade_ceiling")]
    pub low_activity_trade_ceiling: u32,
    /// Paso absoluto al endurecer/suavizar umbrales.
    #[serde(default = "default_delta_tune_step")]
    pub delta_tune_step: Decimal,
    /// Piso de `delta_up_pct` / `delta_down_pct` tras tuning.
    #[serde(default = "default_adaptive_delta_min_pct")]
    pub delta_min_pct: Decimal,
    /// Techo de `delta_*` tras tuning.
    #[serde(default = "default_adaptive_delta_max_pct")]
    pub delta_max_pct: Decimal,
    /// Q-learning (solo paper + adaptive): ajusta `delta_*` aprendiendo de PnL / TP / SL por ventana.
    #[serde(default)]
    pub rl: Option<RlTuneConfig>,
}

/// Hiperparámetros del agente tabular (Q-learning) para tuning de momentum en paper.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct RlTuneConfig {
    pub enabled: bool,
    /// Tasa de aprendizaje α (0,1].
    pub alpha: f64,
    /// Factor de descuento γ (recompensa myopic ≈ 0; largo plazo → 1).
    pub gamma: f64,
    /// Exploración ε-greedy inicial.
    pub epsilon: f64,
    /// Piso de ε tras decaimiento exponencial por cada paso de tuning.
    pub epsilon_min: f64,
    /// Factor multiplicativo de ε tras cada actualización (ej. 0.998).
    pub epsilon_decay: f64,
    /// Arch JSON bajo `report_dir` o ruta relativa/absoluta para la tabla Q.
    pub persist_path: String,
    /// Fracción de `delta_tune_step` aplicada **cada intervalo** (evita oscilaciones).
    #[serde(default = "default_rl_interval_step_fraction")]
    pub interval_step_fraction: f64,
    /// Escala USDC en recompensa: `tanh(pnl / pnl_reward_divisor)` domina el objetivo rentabilidad.
    #[serde(default = "default_rl_pnl_reward_divisor")]
    pub pnl_reward_divisor: f64,
    /// Log detallado por intervalo (JSON Lines) para análisis / ML offline.
    #[serde(default = "default_rl_log_interval_jsonl")]
    pub log_interval_jsonl: bool,
    #[serde(default = "default_rl_interval_log_jsonl_path")]
    pub interval_log_jsonl_path: String,
    /// Misma información en CSV plano (Excel); cabecera en primera escritura.
    #[serde(default = "default_rl_log_interval_csv")]
    pub log_interval_csv: bool,
    #[serde(default = "default_rl_interval_log_csv_path")]
    pub interval_log_csv_path: String,
}

impl Default for RlTuneConfig {
    fn default() -> Self {
        Self {
            enabled: default_rl_tune_enabled(),
            alpha: default_rl_alpha(),
            gamma: default_rl_gamma(),
            epsilon: default_rl_epsilon(),
            epsilon_min: default_rl_epsilon_min(),
            epsilon_decay: default_rl_epsilon_decay(),
            persist_path: default_rl_persist_filename(),
            interval_step_fraction: default_rl_interval_step_fraction(),
            pnl_reward_divisor: default_rl_pnl_reward_divisor(),
            log_interval_jsonl: default_rl_log_interval_jsonl(),
            interval_log_jsonl_path: default_rl_interval_log_jsonl_path(),
            log_interval_csv: default_rl_log_interval_csv(),
            interval_log_csv_path: default_rl_interval_log_csv_path(),
        }
    }
}

fn default_rl_tune_enabled() -> bool {
    false
}

fn default_rl_alpha() -> f64 {
    0.18
}

fn default_rl_gamma() -> f64 {
    0.92
}

fn default_rl_epsilon() -> f64 {
    0.18
}

fn default_rl_epsilon_min() -> f64 {
    0.04
}

fn default_rl_epsilon_decay() -> f64 {
    0.998
}

fn default_rl_persist_filename() -> String {
    "rl_qtable.json".to_string()
}

fn default_rl_interval_step_fraction() -> f64 {
    0.42
}

fn default_rl_pnl_reward_divisor() -> f64 {
    12.0
}

fn default_rl_log_interval_jsonl() -> bool {
    true
}

fn default_rl_interval_log_jsonl_path() -> String {
    "rl_interval.jsonl".to_string()
}

fn default_rl_log_interval_csv() -> bool {
    true
}

fn default_rl_interval_log_csv_path() -> String {
    "rl_interval.csv".to_string()
}

fn default_adaptive_paper_enabled() -> bool {
    false
}

fn default_adaptive_report_dir() -> String {
    "paper_reports".to_string()
}

fn default_analysis_jsonl() -> bool {
    true
}

fn default_analysis_tick_ms() -> u64 {
    1_000
}

fn default_lag_signal_min_ms() -> u64 {
    50
}

fn default_lag_signal_max_ms() -> u64 {
    800
}

fn default_impulse_min_pct() -> Decimal {
    Decimal::new(5, 5)
}

fn default_impulse_window_ms() -> u64 {
    1_000
}

fn default_tune_every_n_intervals() -> u32 {
    3
}

fn default_low_activity_trade_ceiling() -> u32 {
    1
}

fn default_delta_tune_step() -> Decimal {
    Decimal::new(2, 4)
}

fn default_adaptive_delta_min_pct() -> Decimal {
    Decimal::new(5, 4)
}

fn default_adaptive_delta_max_pct() -> Decimal {
    Decimal::new(1, 2)
}

/// Main configuration file.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Bot mode: paper or live.
    pub mode: Mode,

    /// Private key hex for Polymarket wallet (Polygon). Required in live mode.
    pub private_key_polygon: Option<String>,
    /// Wallet signature type for auth (EOA / Proxy / GnosisSafe).
    pub signature_type: SignatureType,

    /// Paper-only settings.
    pub paper: Option<PaperConfig>,
    /// Starting balance used for risk sizing and daily drawdown calculations.
    ///
    /// In paper mode, defaults to `paper.balance_usdc`.
    /// In live mode, this must be provided unless you want to implement a balance query.
    pub starting_balance_usdc: Option<Decimal>,

    /// Endpoint overrides.
    pub endpoints: Option<Endpoints>,

    /// CEX selection / fallback.
    pub cex: Option<CexConfig>,

    /// Momentum strategy config.
    pub momentum: MomentumConfig,
    /// Trading config.
    pub trading: TradingConfig,
    /// Risk config.
    pub risk: RiskConfig,

    /// Paper-only: interval reports, lag/analysis JSONL, adaptive momentum deltas.
    #[serde(default)]
    pub adaptive_paper: Option<AdaptivePaperConfig>,

    /// If true, only trade markets that are currently open (default true).
    pub only_active_markets: Option<bool>,
    /// Subscription horizon: also resolve and subscribe markets for the next N intervals.
    /// This reduces WS resubscribe overhead.
    pub subscription_horizon_intervals: Option<u32>,
    /// List of assets (solo se usa **BTC** para Polymarket; el resto se ignora con aviso).
    pub assets: Option<Vec<String>>,
}

/// CEX selection config.
#[derive(Debug, Clone, Deserialize)]
pub struct CexConfig {
    pub mode: CexMode,
    /// If a feed source is older than this, it is considered stale and fallback may activate.
    pub max_feed_staleness_ms: u64,
    /// Cómo combinar señal momentum entre Binance y Coinbase cuando ambos están en vivo.
    #[serde(default)]
    pub momentum_policy: CexMomentumPolicy,
}

fn default_yes_no_arb_enabled() -> bool {
    true
}

fn default_take_profit_ticks() -> Option<Decimal> {
    Some(Decimal::new(2, 2))
}

fn default_endpoints() -> Endpoints {
    Endpoints {
        clob_base_url: Some("https://clob.polymarket.com".to_string()),
        gamma_base_url: Some("https://gamma-api.polymarket.com".to_string()),
    }
}

impl Config {
    pub fn load_from_toml(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref();
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("read config TOML: {}", path.display()))?;
        let mut cfg: Config = toml::from_str(&contents).context("parse TOML")?;

        // Apply defaults.
        let endpoints = cfg.endpoints.take().unwrap_or_else(default_endpoints);
        cfg.endpoints = Some(endpoints);

        cfg.only_active_markets.get_or_insert(true);

        if cfg.subscription_horizon_intervals.is_none() {
            cfg.subscription_horizon_intervals = Some(2);
        }
        if cfg.assets.is_none() {
            cfg.assets = Some(vec!["BTC".to_string()]);
        }
        if cfg.cex.is_none() {
            cfg.cex = Some(CexConfig {
                mode: CexMode::Auto,
                max_feed_staleness_ms: 1_500,
                momentum_policy: CexMomentumPolicy::default(),
            });
        }
        if cfg.mode == Mode::Paper && cfg.paper.is_none() {
            return Err(anyhow::anyhow!(
                "paper.balance_usdc must be set when mode = \"paper\""
            ));
        }

        // Validate.
        cfg.validate()?;
        Ok(cfg)
    }

    fn validate(&mut self) -> Result<()> {
        let momentum = &self.momentum;
        anyhow::ensure!(momentum.window_sec >= 3 && momentum.window_sec <= 10, "momentum.window_sec must be within 3..=10");
        anyhow::ensure!(
            momentum.strong_prob_threshold >= 0.5 && momentum.strong_prob_threshold <= 0.99,
            "momentum.strong_prob_threshold must be in [0.5, 0.99]"
        );
        anyhow::ensure!(
            momentum.min_taker_imbalance >= 0.0 && momentum.min_taker_imbalance <= 1.0,
            "momentum.min_taker_imbalance must be in [0, 1]"
        );
        anyhow::ensure!(self.trading.tick_ms >= 1, "trading.tick_ms must be >= 1");
        anyhow::ensure!(self.trading.edge_min > Decimal::ZERO, "trading.edge_min must be > 0");
        anyhow::ensure!(
            self.trading.arb_yes_no_sum_max >= Decimal::ZERO && self.trading.arb_yes_no_sum_max <= Decimal::ONE,
            "trading.arb_yes_no_sum_max must be within [0, 1]"
        );
        anyhow::ensure!(
            self.risk.risk_per_trade_frac > Decimal::ZERO,
            "risk.risk_per_trade_frac must be > 0"
        );
        anyhow::ensure!(
            self.risk.daily_drawdown_frac > Decimal::ZERO && self.risk.daily_drawdown_frac < Decimal::ONE,
            "risk.daily_drawdown_frac must be in (0, 1)"
        );
        anyhow::ensure!(
            self.trading.cancel_if_unfilled_ms > 0,
            "trading.cancel_if_unfilled_ms must be > 0"
        );
        if let Some(tp) = self.trading.take_profit_ticks {
            anyhow::ensure!(tp >= Decimal::ZERO, "trading.take_profit_ticks must be >= 0");
        }
        if let Some(sl) = self.trading.stop_loss_ticks {
            anyhow::ensure!(sl > Decimal::ZERO, "trading.stop_loss_ticks must be > 0 when set");
        }
        if let Some(bms) = self.trading.max_book_staleness_ms {
            anyhow::ensure!(bms >= 100, "trading.max_book_staleness_ms must be >= 100 when set");
        }
        if let Some(sp) = self.trading.max_spread_ticks {
            anyhow::ensure!(sp > Decimal::ZERO, "trading.max_spread_ticks must be > 0 when set");
        }
        anyhow::ensure!(
            self.trading.min_time_remaining_sec <= 300,
            "trading.min_time_remaining_sec must be <= 300"
        );
        if let Some(tr) = self.trading.trailing_stop_loss_ticks {
            anyhow::ensure!(tr > Decimal::ZERO, "trading.trailing_stop_loss_ticks must be > 0 when set");
        }
        anyhow::ensure!(
            self.trading.max_size_to_ask_ratio > Decimal::ZERO,
            "trading.max_size_to_ask_ratio must be > 0"
        );
        anyhow::ensure!(
            self.trading.exit_order_timeout_ms >= 1_000,
            "trading.exit_order_timeout_ms must be >= 1000"
        );
        if let Some(ms) = self.trading.sl_cooldown_ms {
            anyhow::ensure!(ms >= 100, "trading.sl_cooldown_ms must be >= 100 when set");
        }
        if self.trading.post_only {
            anyhow::ensure!(
                self.trading.entry_time_in_force == ClobOrderTimeInForce::Gtc,
                "trading.post_only requires trading.entry_time_in_force = \"gtc\" (Polymarket API)"
            );
        }

        if let Some(ap) = self.adaptive_paper.as_ref() {
            if ap.enabled {
                anyhow::ensure!(
                    self.mode == Mode::Paper,
                    "adaptive_paper.enabled requires mode = \"paper\""
                );
            }
            anyhow::ensure!(!ap.report_dir.trim().is_empty(), "adaptive_paper.report_dir must be set");
            anyhow::ensure!(
                ap.lag_signal_min_ms <= ap.lag_signal_max_ms,
                "adaptive_paper.lag_signal_min_ms must be <= lag_signal_max_ms"
            );
            anyhow::ensure!(ap.tune_every_n_intervals >= 1, "adaptive_paper.tune_every_n_intervals must be >= 1");
            anyhow::ensure!(
                ap.low_activity_trade_ceiling < 1_000_000,
                "adaptive_paper.low_activity_trade_ceiling must be reasonable"
            );
            anyhow::ensure!(
                ap.delta_min_pct <= ap.delta_max_pct && ap.delta_min_pct > Decimal::ZERO,
                "adaptive_paper.delta_min_pct must be in (0, delta_max_pct]"
            );
            anyhow::ensure!(ap.delta_tune_step >= Decimal::ZERO, "adaptive_paper.delta_tune_step must be >= 0");
            anyhow::ensure!(ap.impulse_window_ms >= 100, "adaptive_paper.impulse_window_ms must be >= 100");
            if let Some(rl) = ap.rl.as_ref() {
                if rl.enabled {
                    anyhow::ensure!(
                        ap.enabled,
                        "adaptive_paper.rl.enabled requires adaptive_paper.enabled = true"
                    );
                    anyhow::ensure!(self.mode == Mode::Paper, "adaptive_paper.rl requires mode = \"paper\"");
                    anyhow::ensure!(
                        rl.alpha > 0.0 && rl.alpha <= 1.0,
                        "adaptive_paper.rl.alpha must be in (0, 1]"
                    );
                    anyhow::ensure!(
                        rl.gamma >= 0.0 && rl.gamma <= 1.0,
                        "adaptive_paper.rl.gamma must be in [0, 1]"
                    );
                    anyhow::ensure!(
                        rl.epsilon >= 0.0 && rl.epsilon <= 1.0,
                        "adaptive_paper.rl.epsilon must be in [0, 1]"
                    );
                    anyhow::ensure!(
                        rl.epsilon_min >= 0.0 && rl.epsilon_min <= rl.epsilon,
                        "adaptive_paper.rl.epsilon_min must be <= epsilon"
                    );
                    anyhow::ensure!(
                        rl.epsilon_decay > 0.0 && rl.epsilon_decay <= 1.0,
                        "adaptive_paper.rl.epsilon_decay must be in (0, 1]"
                    );
                    anyhow::ensure!(!rl.persist_path.trim().is_empty(), "adaptive_paper.rl.persist_path must be set");
                    anyhow::ensure!(
                        rl.interval_step_fraction > 0.0 && rl.interval_step_fraction <= 1.0,
                        "adaptive_paper.rl.interval_step_fraction must be in (0, 1]"
                    );
                    anyhow::ensure!(
                        rl.pnl_reward_divisor > 0.01,
                        "adaptive_paper.rl.pnl_reward_divisor must be > 0.01"
                    );
                }
            }
        }

        if self.mode == Mode::Live {
            anyhow::ensure!(
                self.private_key_polygon.as_deref().unwrap_or("").trim().len() > 0,
                "private_key_polygon is required when mode = \"live\""
            );
            anyhow::ensure!(
                self.starting_balance_usdc.is_some(),
                "starting_balance_usdc is required in live mode"
            );
        }
        if self.mode == Mode::Paper && self.starting_balance_usdc.is_none() {
            self.starting_balance_usdc = self.paper.as_ref().map(|p| p.balance_usdc);
        }
        Ok(())
    }

    pub fn clob_base_url(&self) -> &str {
        self.endpoints
            .as_ref()
            .and_then(|e| e.clob_base_url.as_deref())
            .unwrap_or("https://clob.polymarket.com")
    }

    pub fn gamma_base_url(&self) -> &str {
        self.endpoints
            .as_ref()
            .and_then(|e| e.gamma_base_url.as_deref())
            .unwrap_or("https://gamma-api.polymarket.com")
    }

    pub fn cex_config(&self) -> &CexConfig {
        self.cex
            .as_ref()
            .expect("Config validation sets cex")
    }

    pub fn paper_balance_usdc(&self) -> Decimal {
        if self.mode == Mode::Paper {
            self.paper
                .as_ref()
                .map(|p| p.balance_usdc)
                .unwrap_or(Decimal::ZERO)
        } else {
            Decimal::ZERO
        }
    }

    pub fn starting_balance_usdc(&self) -> Decimal {
        self.starting_balance_usdc
            .unwrap_or_else(|| self.paper_balance_usdc())
    }

    /// Logs a short, aligned boot banner. Never logs `private_key_polygon`, only live hint if missing.
    pub fn log_startup_summary(
        &self,
        cfg_path: &str,
        log_file: &str,
        assets: &[crate::types::Asset],
        horizon: u32,
    ) {
        let cex = self.cex_config();
        let assets_s = assets
            .iter()
            .map(|a| format!("{a:?}"))
            .collect::<Vec<_>>()
            .join(" ");
        let endpoints = {
            let c = self.clob_base_url();
            let g = self.gamma_base_url();
            if c == "https://clob.polymarket.com" && g == "https://gamma-api.polymarket.com" {
                "default".to_string()
            } else {
                format!(
                    "{} | {}",
                    c.trim_start_matches("https://"),
                    g.trim_start_matches("https://")
                )
            }
        };
        let bal = self.starting_balance_usdc();
        let mode_line = match self.mode {
            Mode::Paper => format!("paper · {bal} USDC"),
            Mode::Live => format!(
                "live · {} · {bal} USDC",
                match self.signature_type {
                    SignatureType::Eoa => "eoa",
                    SignatureType::Proxy => "proxy",
                    SignatureType::GnosisSafe => "safe",
                }
            ),
        };
        let cex_m = match cex.mode {
            CexMode::Auto => "auto",
            CexMode::BinanceOnly => "binance",
            CexMode::CoinbaseOnly => "coinbase",
        };
        let mom_pol = match cex.momentum_policy {
            CexMomentumPolicy::Primary => "Δ1",
            CexMomentumPolicy::Consensus => "Δ2",
        };
        let active = if self.only_active_markets.unwrap_or(true) {
            "open-only · BTC 5m"
        } else {
            "all · BTC 5m"
        };
        let mut lines = vec![
            String::new(),
            "  ┌─────────────────────────────────────────────────────────┐".to_string(),
            "  │                      sniper boot                        │".to_string(),
            "  ├─────────────────────────────────────────────────────────┤".to_string(),
            format!("  │  config      {:<43} │", trim_path(cfg_path, 43)),
            format!("  │  mode         {:<43} │", trim_field(&mode_line, 43)),
            format!("  │  assets      {:<43} │", trim_field(&assets_s, 43)),
            format!(
                "  │  discovery   {:<43} │",
                format!("horizon +{horizon} · markets {active}")
            ),
            format!(
                "  │  cex         {:<43} │",
                format!(
                    "{cex_m} · stale {}ms · mom {mom_pol}",
                    cex.max_feed_staleness_ms
                )
            ),
            format!(
                "  │  strategy    {:<43} │",
                trim_field(
                    &{
                        let tp_note = match self.trading.take_profit_ticks {
                            Some(t) if !t.is_zero() => format!(" · TP +{t}"),
                            _ => String::new(),
                        };
                        format!(
                            "tick {}ms · edge ≥ {} · mom {}s Δ {} / {} · {}{}",
                            self.trading.tick_ms,
                            self.trading.edge_min,
                            self.momentum.window_sec,
                            self.momentum.delta_up_pct,
                            self.momentum.delta_down_pct,
                            if self.trading.yes_no_arb_enabled {
                                "yes+no on"
                            } else {
                                "yes+no off"
                            },
                            tp_note,
                        )
                    },
                    43
                )
            ),
            format!(
                "  │  risk        {:<43} │",
                trim_field(
                    &format!(
                        "/trade {} · dd {} · kill {}",
                        self.risk.risk_per_trade_frac,
                        self.risk.daily_drawdown_frac,
                        if self.risk.kill_switch_enabled {
                            "on"
                        } else {
                            "off"
                        }
                    ),
                    43
                )
            ),
            format!("  │  endpoints   {:<43} │", trim_field(&endpoints, 43)),
            format!("  │  log file    {:<43} │", trim_field(log_file, 43)),
            "  └─────────────────────────────────────────────────────────┘".to_string(),
            String::new(),
        ];
        if self.mode == Mode::Live {
            let pk_ok = self
                .private_key_polygon
                .as_deref()
                .map(|s| !s.trim().is_empty())
                .unwrap_or(false);
            if !pk_ok {
                lines.insert(
                    lines.len() - 1,
                    "  │  (live) key  not configured — orders will fail          │".to_string(),
                );
            }
        }
        let banner = lines.join("\n");
        tracing::info!(target: "sniper", "{}", banner);
    }
}

fn trim_field(s: &str, max: usize) -> String {
    let t = s.trim();
    if t.chars().count() <= max {
        return t.to_string();
    }
    let take = max.saturating_sub(1);
    format!("{}…", t.chars().take(take).collect::<String>())
}

fn trim_path(path: &str, max: usize) -> String {
    let t = path.replace('\\', "/");
    let n = t.chars().count();
    if n <= max {
        return t;
    }
    let take = max.saturating_sub(1);
    let suffix: String = t.chars().rev().take(take).collect::<String>().chars().rev().collect();
    format!("…{suffix}")
}

/// Parse asset symbol from TOML ("BTC"/"ETH"/"SOL") for config-driven selection.
pub fn parse_asset_symbol(sym: &str) -> anyhow::Result<crate::types::Asset> {
    crate::types::Asset::from_str(sym).map_err(|_| anyhow::anyhow!("Unknown asset symbol: {sym}"))
}

