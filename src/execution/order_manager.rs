//! Ejecución HFT: órdenes IOC-like (**FAK**/**FOK**) vs resting (**GTC**), reconciliación por WS.
//!
//! **Checklist típico (senior / latencia–libro):**
//! - Sincronía de reloj (NTP); medir `lag_cex_minus_book_ms` y frescura WS vs CEX.
//! - Un solo snapshot de libro por decisión; evitar REST en el camino caliente si el WS va al día.
//! - Fills parciales (**FAK**): acumular tamaño y VWAP en entrada; escalar salidas de TP hasta cerrar inventario.
//! - **FOK** en entradas solo cuando quieras tamaño completo o nada (p. ej. arb pareado); si no, **FAK** reduce misses por libro fino.
//! - TP: **FAK** suele dominar **FOK** cuando el bid es delgado; **GTC** si priorizas precio sobre tiempo.
//! - Límites de API, firmas en caliente, retries idempotentes, y líneas base de P99 de `post_order` + ws latency.

use crate::config::{ClobOrderTimeInForce, Config, Mode};
use crate::paper_lab::PaperLab;
use crate::polymarket::client::{LivePolymarket, OrderbookTop};
use crate::types::{
    momentum_p_strong_from_signal, MarketKey, Outcome, Position, PositionLeg, Price, Signal,
    SpotIntervalState, TokenId, TradeEntryDiag,
};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{sleep, Duration};
use tokio::sync::RwLock as TokioRwLock;
use tokio_util::sync::CancellationToken;

use polymarket_client_sdk::clob::types::OrderType;
use rust_decimal_macros::dec;
use polymarket_client_sdk::clob::ws::types::response::TradeMessage;
use polymarket_client_sdk::clob::ws::types::response::TradeMessageStatus;

/// Fill message forwarded from Polymarket WS.
#[derive(Debug, Clone)]
pub struct TradeFillEvent {
    pub taker_order_id: Option<String>,
    pub maker_order_ids: Vec<String>,
    #[allow(dead_code)]
    pub asset_id: TokenId,
    pub size: Price,
    pub price: Price,
    pub status: TradeMessageStatus,
}

impl From<TradeMessage> for TradeFillEvent {
    fn from(t: TradeMessage) -> Self {
        let maker_order_ids = t.maker_orders.iter().map(|m| m.order_id.clone()).collect();
        Self {
            taker_order_id: t.taker_order_id.clone(),
            maker_order_ids,
            asset_id: t.asset_id,
            size: t.size,
            price: t.price,
            status: t.status,
        }
    }
}

#[derive(Debug, Clone)]
struct CancelEvent {
    order_id: String,
}

/// Executes signals into entry orders and settles P&L at interval close.
pub struct OrderManager {
    cfg: Arc<Config>,
    mode: Mode,
    orderbook: Arc<TokioRwLock<OrderbookTop>>,

    signal_rx: mpsc::UnboundedReceiver<Signal>,
    trade_rx: mpsc::UnboundedReceiver<TradeFillEvent>,
    cancel_rx: mpsc::UnboundedReceiver<CancelEvent>,
    cancel_tx: mpsc::UnboundedSender<CancelEvent>,

    live: Option<Arc<LivePolymarket>>,
    paper_lab: Option<Arc<PaperLab>>,

    // positions tracked until interval settlement.
    positions: HashMap<u64, Position>,
    // order_id -> (position_id, leg_idx) for entry buys
    order_to_leg: HashMap<String, (u64, usize)>,
    /// Sell de salida (TP o SL) por order_id.
    exit_order_to_position: HashMap<String, (u64, ExitReason)>,

    last_entry_ms_by_market: HashMap<MarketKey, u64>,
    /// Tras un SL, bloquea re-entrada en ese mercado hasta `sl_cooldown_ms`.
    last_sl_exit_ms_by_market: HashMap<MarketKey, u64>,
    active_positions_by_market: HashMap<MarketKey, usize>,

    /// Opcional: `trades.jsonl` (path relativo al cwd o absoluto).
    trades_jsonl_path: Option<PathBuf>,

    /// Dedup: at most one entry per (MarketKey, interval_start_unix).
    entered_intervals: HashSet<(MarketKey, u64)>,

    spot_intervals: Arc<TokioRwLock<HashMap<(MarketKey, u64), SpotIntervalState>>>,

    next_position_id: u64,

    // Kill switch / risk.
    pnl_day: Price,
    starting_balance: Price,
    day_bucket: u64,
    kill_switch_triggered: bool,

    // Metrics
    wins: u64,
    losses: u64,
    total_trades: u64,
    edge_sum: Price,
    edge_count: u64,
    slippage_sum: Price,
    slippage_count: u64,
}

fn now_ms() -> u64 {
    let ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    ms as u64
}

fn now_unix_sec() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn day_bucket(now_unix_sec: u64) -> u64 {
    now_unix_sec / 86_400
}

/// Mínimo de shares que seguimos considerando posición abierta tras ventas FAK parciales.
const POSITION_DUST_SHARES: Decimal = dec!(0.0001);

#[derive(Clone, Copy)]
enum ExitReason {
    TakeProfit,
    StopLoss,
}

#[inline]
fn sdk_order_type(tif: ClobOrderTimeInForce) -> OrderType {
    match tif {
        ClobOrderTimeInForce::Gtc => OrderType::GTC,
        ClobOrderTimeInForce::Fok => OrderType::FOK,
        ClobOrderTimeInForce::Fak => OrderType::FAK,
    }
}

fn outcome_short(o: Outcome) -> &'static str {
    match o {
        Outcome::Up => "UP",
        Outcome::Down => "DN",
    }
}

fn position_entry_size(pos: &Position) -> Price {
    let filled: Price = pos.legs.iter().fold(Price::ZERO, |a, l| a + l.filled_size);
    if !filled.is_zero() {
        filled
    } else {
        pos.legs.iter().fold(Price::ZERO, |a, l| a + l.intended_size)
    }
}

/// En paper, también volcamos a **stderr** para que los trades se vean aunque `RUST_LOG` filtre `info`.
fn paper_trade_terminal_line(msg: impl std::fmt::Display) {
    let mut w = std::io::stderr().lock();
    let _ = writeln!(w, "[paper trade] {msg}");
    let _ = w.flush();
}

fn log_trade_entry(paper_terminal: bool, position_id: u64, pos: &Position) {
    match &pos.kind {
        Signal::Momentum {
            outcome,
            edge,
            fair_prob_up,
            market_prob_side,
            ..
        } => {
            let edge_pct = (*edge * Decimal::from(100)).round_dp(2);
            let sz = position_entry_size(pos);
            tracing::info!(
                target: "sniper",
                "▲ ENTRY #{} {} · edge {}% · fair {:.3} · ask {} · size {}",
                position_id,
                outcome_short(*outcome),
                edge_pct,
                fair_prob_up,
                market_prob_side,
                sz,
            );
            if paper_terminal {
                paper_trade_terminal_line(format!(
                    "ENTRY #{} {} · edge {}% · fair {:.3} · ask {} · size {}",
                    position_id,
                    outcome_short(*outcome),
                    edge_pct,
                    fair_prob_up,
                    market_prob_side,
                    sz,
                ));
            }
        }
        Signal::ArbBoth {
            edge,
            market_prob_up,
            market_prob_down,
            ..
        } => {
            let edge_pct = (*edge * Decimal::from(100)).round_dp(2);
            let sz = position_entry_size(pos);
            tracing::info!(
                target: "sniper",
                "ARB #{} | edge {}% | up {} | dn {} | sz {}",
                position_id,
                edge_pct,
                market_prob_up,
                market_prob_down,
                sz,
            );
            if paper_terminal {
                paper_trade_terminal_line(format!(
                    "ARB #{} | edge {}% | up {} | dn {} | sz {}",
                    position_id,
                    edge_pct,
                    market_prob_up,
                    market_prob_down,
                    sz,
                ));
            }
        }
    }
}

#[derive(Serialize)]
struct TradeLogRecord {
    schema: &'static str,
    position_id: u64,
    token_id: String,
    outcome: &'static str,
    entry_avg_price: String,
    entry_time_ms: u64,
    exit_price: Option<String>,
    exit_time_ms: u64,
    exit_reason: &'static str,
    pnl_usdc: String,
    p_strong_at_entry: f64,
    spread_at_entry: String,
    time_remaining_sec_at_entry: u64,
    ob_imbalance_at_entry: Option<f64>,
    btc_spot_at_entry: Option<String>,
    rl_action_applied: Option<String>,
}

fn trade_log_record_from_pos(
    pos: &Position,
    exit_reason: &'static str,
    exit_price: Option<Price>,
    exit_wall_ms: u64,
) -> TradeLogRecord {
    let leg = pos.legs.first().expect("trade log requires at least one leg");
    TradeLogRecord {
        schema: "sniper.trade.v1",
        position_id: pos.position_id,
        token_id: format!("{}", leg.token_id),
        outcome: outcome_str(leg.outcome),
        entry_avg_price: leg.filled_price.to_string(),
        entry_time_ms: pos.created_at_ms,
        exit_price: exit_price.map(|p| p.to_string()),
        exit_time_ms: exit_wall_ms,
        exit_reason,
        pnl_usdc: pos.pnl_usdc.to_string(),
        p_strong_at_entry: pos.entry_diag.p_strong,
        spread_at_entry: pos.entry_diag.spread.to_string(),
        time_remaining_sec_at_entry: pos.entry_diag.time_remaining_sec,
        ob_imbalance_at_entry: pos.entry_diag.ob_imbalance,
        btc_spot_at_entry: pos.entry_diag.btc_spot.map(|p| p.to_string()),
        rl_action_applied: pos.entry_diag.rl_action_applied.clone(),
    }
}

#[inline]
fn outcome_str(o: Outcome) -> &'static str {
    match o {
        Outcome::Up => "up",
        Outcome::Down => "down",
    }
}

fn stop_loss_trigger_level(
    entry_px: Price,
    high_water_mark: Price,
    stop_loss_ticks: Option<Price>,
    trailing_ticks: Option<Price>,
) -> Option<Price> {
    let fixed_j = stop_loss_ticks.map(|f| entry_px - f);
    let trail_j = trailing_ticks.map(|tr| high_water_mark - tr);
    match (fixed_j, trail_j) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

fn build_entry_diag(signal: &Signal, book: &OrderbookTop, close_time_unix: u64) -> TradeEntryDiag {
    let wall_sec = now_ms() / 1000;
    let time_remaining_sec = close_time_unix.saturating_sub(wall_sec);
    match signal {
        Signal::Momentum {
            outcome,
            token_id_up,
            token_id_down,
            ..
        } => {
            let token = match outcome {
                Outcome::Up => *token_id_up,
                Outcome::Down => *token_id_down,
            };
            let mut d = TradeEntryDiag {
                p_strong: momentum_p_strong_from_signal(signal).unwrap_or(0.0),
                spread: Decimal::ZERO,
                time_remaining_sec,
                ob_imbalance: None,
                btc_spot: None,
                rl_action_applied: None,
            };
            if let (Some((ap, asz)), Some((bp, bsz))) = (book.best_ask(token), book.best_bid(token))
            {
                d.spread = ap - bp;
                let sum = asz + bsz;
                if !sum.is_zero() {
                    d.ob_imbalance = ((bsz - asz) / sum).to_f64();
                }
            }
            d
        }
        Signal::ArbBoth { .. } => TradeEntryDiag {
            time_remaining_sec,
            rl_action_applied: None,
            ..Default::default()
        },
    }
}

fn normalize_order_id(order_id: &str) -> String {
    let s = order_id.trim().trim_start_matches("0x").to_lowercase();
    if s.is_empty() {
        order_id.to_string()
    } else {
        format!("0x{s}")
    }
}

impl OrderManager {
    pub fn spawn(
        cfg: Arc<Config>,
        mode: Mode,
        orderbook: Arc<TokioRwLock<OrderbookTop>>,
        live: Option<Arc<LivePolymarket>>,
        signal_rx: mpsc::UnboundedReceiver<Signal>,
        trade_rx: mpsc::UnboundedReceiver<TradeFillEvent>,
        spot_intervals: Arc<TokioRwLock<HashMap<(MarketKey, u64), SpotIntervalState>>>,
        paper_lab: Option<Arc<PaperLab>>,
        shutdown: CancellationToken,
    ) -> JoinHandle<()> {
        tokio::spawn(async move {
            let (cancel_tx, cancel_rx) = mpsc::unbounded_channel::<CancelEvent>();
            let mut mgr = Self {
                pnl_day: Price::ZERO,
                starting_balance: cfg.starting_balance_usdc(),
                day_bucket: 0,
                kill_switch_triggered: false,
                cfg: cfg.clone(),
                mode,
                orderbook,
                signal_rx,
                trade_rx,
                cancel_rx,
                cancel_tx,
                live,
                paper_lab,
                positions: HashMap::new(),
                order_to_leg: HashMap::new(),
                exit_order_to_position: HashMap::new(),
                last_entry_ms_by_market: HashMap::new(),
                last_sl_exit_ms_by_market: HashMap::new(),
                active_positions_by_market: HashMap::new(),
                trades_jsonl_path: cfg
                    .trading
                    .trades_jsonl_path
                    .as_ref()
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                    .map(PathBuf::from),
                entered_intervals: HashSet::new(),
                spot_intervals,
                next_position_id: 1,
                wins: 0,
                losses: 0,
                total_trades: 0,
                edge_sum: Price::ZERO,
                edge_count: 0,
                slippage_sum: Price::ZERO,
                slippage_count: 0,
            };

            mgr.day_bucket = day_bucket(now_unix_sec());

            let mut metrics_tick = tokio::time::interval(Duration::from_secs(20));

            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => {
                        tracing::info!(target: "sniper", "order manager · stop");
                        return;
                    }
                    Some(fill) = mgr.trade_rx.recv() => {
                        mgr.on_fill(fill).await;
                        mgr.check_profit_deadline_violations().await;
                    }
                    Some(signal) = mgr.signal_rx.recv() => {
                        mgr.submit_signal(signal, &shutdown).await;
                        mgr.check_stop_losses(&shutdown).await;
                        mgr.check_take_profits(&shutdown).await;
                        mgr.check_profit_deadline_violations().await;
                    }
                    Some(cancel) = mgr.cancel_rx.recv() => {
                        mgr.on_cancel(cancel).await;
                    }
                    _ = metrics_tick.tick() => {
                        mgr.reset_day_if_needed();
                        mgr.settle_positions().await;
                        mgr.log_metrics();
                        mgr.gc_closed_positions();
                    }
                    _ = sleep(Duration::from_millis(50)) => {
                        mgr.reset_day_if_needed();
                        mgr.settle_positions().await;
                        mgr.reconcile_orphan_exit_orders().await;
                        mgr.check_stop_losses(&shutdown).await;
                        mgr.check_take_profits(&shutdown).await;
                        mgr.check_profit_deadline_violations().await;
                    }
                }
            }
        })
    }

    fn reset_day_if_needed(&mut self) {
        let now_sec = now_unix_sec();
        let current_bucket = day_bucket(now_sec);
        if current_bucket != self.day_bucket {
            self.day_bucket = current_bucket;
            self.pnl_day = Price::ZERO;
            self.kill_switch_triggered = false;
        }
    }

    fn log_metrics(&self) {
        let win_rate = if self.total_trades == 0 {
            0.0
        } else {
            self.wins as f64 / self.total_trades as f64
        };
        let avg_edge = if self.edge_count == 0 {
            Decimal::ZERO
        } else {
            self.edge_sum / Decimal::from(self.edge_count)
        };
        let pnl_frac = if self.starting_balance.is_zero() {
            0.0
        } else {
            self.pnl_day.to_f64().unwrap_or(0.0) / self.starting_balance.to_f64().unwrap_or(1.0)
        };

        let avg_slip = if self.slippage_count == 0 {
            Decimal::ZERO
        } else {
            self.slippage_sum / Decimal::from(self.slippage_count)
        };

        let idle = self.total_trades == 0
            && self.pnl_day.is_zero()
            && !self.kill_switch_triggered;
        if idle {
            return;
        } else {
            let kill = if self.kill_switch_triggered { "on" } else { "off" };
            tracing::info!(
                target: "sniper",
                pnl = %self.pnl_day,
                pnl_pct = %format!("{:.2}%", 100.0 * pnl_frac),
                n = self.total_trades,
                wl = %format!("{}/{}", self.wins, self.losses),
                win_pct = %format!("{:.0}%", 100.0 * win_rate),
                edge = %avg_edge,
                slip_bps = %avg_slip,
                kill = kill,
                "pulse"
            );
        }
    }

    fn gc_closed_positions(&mut self) {
        let before = self.positions.len();
        self.positions.retain(|_id, pos| {
            !(pos.settled || pos.closed_via_take_profit || pos.closed_via_stop_loss)
        });
        let removed = before - self.positions.len();
        if removed > 0 {
            tracing::debug!(target: "sniper", removed, remaining = self.positions.len(), "gc · positions");
        }
    }

    fn can_place_for_market(&self, market: MarketKey) -> bool {
        if self.kill_switch_triggered && self.cfg.risk.kill_switch_enabled {
            return false;
        }
        let active = self.active_positions_by_market.get(&market).copied().unwrap_or(0);
        if active >= self.cfg.trading.max_positions_per_market {
            return false;
        }
        let now = now_ms();
        let last = self.last_entry_ms_by_market.get(&market).copied().unwrap_or(0);
        if now.saturating_sub(last) < self.cfg.trading.entry_cooldown_ms {
            return false;
        }
        if let Some(ms) = self.sl_cooldown_after_sl_ms() {
            if let Some(t_sl) = self.last_sl_exit_ms_by_market.get(&market) {
                if now.saturating_sub(*t_sl) < ms {
                    return false;
                }
            }
        }
        true
    }

    /// TP/SL/trailing con override paper_lab (RL).
    fn take_profit_ticks_effective(&self) -> Option<Decimal> {
        match &self.paper_lab {
            Some(lab) => lab.effective_take_profit_ticks(self.cfg.trading.take_profit_ticks),
            None => self.cfg.trading.take_profit_ticks,
        }
    }

    fn stop_loss_ticks_effective(&self) -> Option<Decimal> {
        match &self.paper_lab {
            Some(lab) => lab.effective_stop_loss_ticks(self.cfg.trading.stop_loss_ticks),
            None => self.cfg.trading.stop_loss_ticks,
        }
    }

    fn trailing_sl_ticks_effective(&self) -> Option<Decimal> {
        match &self.paper_lab {
            Some(lab) => lab.effective_trailing_stop_loss_ticks(self.cfg.trading.trailing_stop_loss_ticks),
            None => self.cfg.trading.trailing_stop_loss_ticks,
        }
    }

    fn sl_cooldown_after_sl_ms(&self) -> Option<u64> {
        match &self.paper_lab {
            Some(lab) => Some(lab.effective_sl_cooldown_ms()),
            None => self.cfg.trading.sl_cooldown_ms,
        }
    }

    fn write_trade_log_record(&self, rec: &TradeLogRecord) {
        let Some(ref path) = self.trades_jsonl_path else {
            return;
        };
        let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) else {
            return;
        };
        if let Ok(s) = serde_json::to_string(rec) {
            let _ = writeln!(f, "{}", s);
        }
    }

    fn book_is_fresh(&self, book: &OrderbookTop) -> bool {
        let Some(max_stale) = self.cfg.trading.max_book_staleness_ms else {
            return true;
        };
        let ts = book.last_book_event_ts_ms();
        if ts <= 0 {
            return false;
        }
        now_ms().saturating_sub(ts as u64) <= max_stale
    }

    async fn on_fill(&mut self, fill: TradeFillEvent) {
        let ok_status = matches!(fill.status, TradeMessageStatus::Matched | TradeMessageStatus::Confirmed);
        if !ok_status {
            return;
        }

        let mut order_ids: Vec<String> = Vec::new();
        if let Some(taker) = &fill.taker_order_id {
            order_ids.push(normalize_order_id(taker));
        }
        for mo in &fill.maker_order_ids {
            order_ids.push(normalize_order_id(mo));
        }

        for oid in order_ids {
            if let Some(&(pos_id, reason)) = self.exit_order_to_position.get(&oid) {
                let fully_closed = match reason {
                    ExitReason::TakeProfit => self.on_take_profit_fill(pos_id, &fill),
                    ExitReason::StopLoss => self.on_stop_loss_fill(pos_id, &fill),
                };
                if fully_closed {
                    self.exit_order_to_position.remove(&oid);
                }
                continue;
            }
            if let Some(&(pos_id, leg_idx)) = self.order_to_leg.get(&oid) {
                let mut schedule_deadline = false;
                if let Some(pos) = self.positions.get_mut(&pos_id) {
                    if pos.settled {
                        continue;
                    }
                    if let Some(leg) = pos.legs.get_mut(leg_idx) {
                        let add = fill.size;
                        if add.is_zero() {
                            continue;
                        }
                        let first_entry_fill = leg.filled_size.is_zero();
                        if leg.filled_size.is_zero() {
                            leg.filled_size = add;
                            leg.filled_price = fill.price;
                        } else {
                            let total_cost = leg.filled_price * leg.filled_size + fill.price * add;
                            leg.filled_size += add;
                            if !leg.filled_size.is_zero() {
                                leg.filled_price = total_cost / leg.filled_size;
                            }
                        }
                        if first_entry_fill
                            && !leg.filled_size.is_zero()
                            && matches!(pos.kind, Signal::Momentum { .. })
                        {
                            pos.entry_fill_ms = Some(now_ms());
                            schedule_deadline = true;
                        }
                        // Realized slippage vs intended price (bps).
                        if !leg.intended_price.is_zero() {
                            let slip_bps = ((leg.filled_price - leg.intended_price) / leg.intended_price)
                                * Decimal::from(10_000u32);
                            leg.realized_slippage_bps = slip_bps;
                            self.slippage_sum += slip_bps.abs();
                            self.slippage_count += 1;
                        }
                        // Initialize high water mark from entry fill.
                        if pos.high_water_mark.is_zero() {
                            pos.high_water_mark = leg.filled_price;
                        }
                    }
                }
                if schedule_deadline {
                    let lab = self.paper_lab.clone();
                    let tp_eff = self.take_profit_ticks_effective();
                    let th = self.cfg.momentum.strong_prob_threshold;
                    if let Some(pos) = self.positions.get_mut(&pos_id) {
                        if let Some(ref lab) = lab {
                            lab.apply_profit_deadline_to_position(pos, tp_eff, th);
                        }
                    }
                }
            }
        }
    }

    /// Returns `true` if the position was fully closed by this fill.
    #[allow(unused_assignments)]
    fn on_take_profit_fill(&mut self, pos_id: u64, fill: &TradeFillEvent) -> bool {
        let mut trade_log: Option<TradeLogRecord> = None;
        let mut pnl_round: Option<String> = None;
        let mut lab_tp: Option<(u64, Decimal)> = None;
        let mut lab_tp_timing: Option<(f64, Option<u64>)> = None;

        {
            let Some(pos) = self.positions.get_mut(&pos_id) else {
                return true;
            };
            if pos.settled {
                return true;
            }
            let Some(leg) = pos.legs.first_mut() else {
                return true;
            };
            let inv = leg.filled_size;
            let sold = fill.size.min(inv);
            if sold.is_zero() {
                return true;
            }
            let cost_part = leg.filled_price * sold;
            let proceeds_part = fill.price * sold;
            let d_pnl = proceeds_part - cost_part;

            pos.pnl_usdc += d_pnl;
            self.pnl_day += d_pnl;
            leg.filled_size = inv - sold;

            if self.cfg.risk.kill_switch_enabled {
                let loss_limit = -(self.starting_balance * self.cfg.risk.daily_drawdown_frac);
                if self.pnl_day <= loss_limit && !self.kill_switch_triggered {
                    self.kill_switch_triggered = true;
                    tracing::warn!(pnl_day = %self.pnl_day, loss_limit = %loss_limit, "kill switch triggered");
                }
            }

            if leg.filled_size > POSITION_DUST_SHARES {
                tracing::info!(
                    target: "sniper",
                    "TP partial #{} · sold {} · left {} · d_pnl {}",
                    pos_id,
                    sold.round_dp(4),
                    leg.filled_size.round_dp(4),
                    d_pnl.round_dp(4),
                );
                return false;
            }

            pos.take_profit_order_id = None;

            pos.win = pos.pnl_usdc > Decimal::ZERO;
            pos.settled = true;
            pos.closed_via_take_profit = true;
            pos.last_exit_price = Some(fill.price);
            leg.filled_size = Decimal::ZERO;

            self.total_trades += 1;
            if pos.win {
                self.wins += 1;
            } else {
                self.losses += 1;
            }
            let edge = match &pos.kind {
                Signal::Momentum { edge, .. } => *edge,
                Signal::ArbBoth { edge, .. } => *edge,
            };
            self.edge_sum += edge;
            self.edge_count += 1;

            let mkt = pos.market;
            if let Some(e) = self.active_positions_by_market.get_mut(&mkt) {
                *e = e.saturating_sub(1);
            }

            pnl_round = Some(pos.pnl_usdc.round_dp(4).to_string());
            if matches!(pos.kind, Signal::Momentum { .. }) {
                lab_tp = Some((pos.interval_start_unix, pos.pnl_usdc));
                lab_tp_timing = Some((pos.entry_diag.p_strong, pos.entry_fill_ms));
            }
            trade_log = Some(trade_log_record_from_pos(
                pos,
                "tp",
                Some(fill.price),
                now_ms(),
            ));
        }

        if let Some(rec) = trade_log {
            self.write_trade_log_record(&rec);
        }
        if let Some(s) = pnl_round {
            tracing::info!(target: "sniper", "TP closed #{} · pnl {} USDC", pos_id, s);
        }
        if let Some((iv, pnl)) = lab_tp {
            if let Some(lab) = self.paper_lab.as_ref() {
                lab.record_take_profit(iv, pnl);
                if let Some((ps, ef)) = lab_tp_timing {
                    lab.on_closed_take_profit_timing(ps, ef, now_ms());
                }
            }
        }
        true
    }

    /// Returns `true` if the position was fully closed by this fill.
    #[allow(unused_assignments)]
    fn on_stop_loss_fill(&mut self, pos_id: u64, fill: &TradeFillEvent) -> bool {
        let mut trade_log: Option<TradeLogRecord> = None;
        let mut pnl_round: Option<String> = None;
        let mut lab_sl: Option<(u64, Decimal)> = None;

        {
            let Some(pos) = self.positions.get_mut(&pos_id) else {
                return true;
            };
            if pos.settled {
                return true;
            }
            let Some(leg) = pos.legs.first_mut() else {
                return true;
            };
            let inv = leg.filled_size;
            let sold = fill.size.min(inv);
            if sold.is_zero() {
                return true;
            }
            let cost_part = leg.filled_price * sold;
            let proceeds_part = fill.price * sold;
            let d_pnl = proceeds_part - cost_part;

            pos.pnl_usdc += d_pnl;
            self.pnl_day += d_pnl;
            leg.filled_size = inv - sold;

            if self.cfg.risk.kill_switch_enabled {
                let loss_limit = -(self.starting_balance * self.cfg.risk.daily_drawdown_frac);
                if self.pnl_day <= loss_limit && !self.kill_switch_triggered {
                    self.kill_switch_triggered = true;
                    tracing::warn!(pnl_day = %self.pnl_day, loss_limit = %loss_limit, "kill switch triggered");
                }
            }

            if leg.filled_size > POSITION_DUST_SHARES {
                tracing::info!(
                    target: "sniper",
                    "SL partial #{} · sold {} · left {} · d_pnl {}",
                    pos_id,
                    sold.round_dp(4),
                    leg.filled_size.round_dp(4),
                    d_pnl.round_dp(4),
                );
                return false;
            }

            pos.stop_loss_order_id = None;
            pos.win = pos.pnl_usdc > Decimal::ZERO;
            pos.settled = true;
            pos.closed_via_stop_loss = true;
            pos.last_exit_price = Some(fill.price);
            leg.filled_size = Decimal::ZERO;

            self.total_trades += 1;
            if pos.win {
                self.wins += 1;
            } else {
                self.losses += 1;
            }
            let edge = match &pos.kind {
                Signal::Momentum { edge, .. } => *edge,
                Signal::ArbBoth { edge, .. } => *edge,
            };
            self.edge_sum += edge;
            self.edge_count += 1;

            let mkt = pos.market;
            self.last_sl_exit_ms_by_market.insert(mkt, now_ms());
            if let Some(e) = self.active_positions_by_market.get_mut(&mkt) {
                *e = e.saturating_sub(1);
            }

            pnl_round = Some(pos.pnl_usdc.round_dp(4).to_string());
            if matches!(pos.kind, Signal::Momentum { .. }) {
                lab_sl = Some((pos.interval_start_unix, pos.pnl_usdc));
            }
            trade_log = Some(trade_log_record_from_pos(
                pos,
                "sl",
                Some(fill.price),
                now_ms(),
            ));
        }

        if let Some(rec) = trade_log {
            self.write_trade_log_record(&rec);
        }
        if let Some(s) = pnl_round {
            tracing::info!(target: "sniper", "SL closed #{} · pnl {} USDC", pos_id, s);
        }
        if let Some((iv, pnl)) = lab_sl {
            if let Some(lab) = self.paper_lab.as_ref() {
                lab.record_stop_loss(iv, pnl);
            }
        }
        true
    }

    async fn check_stop_losses(&mut self, shutdown: &CancellationToken) {
        if shutdown.is_cancelled() {
            return;
        }
        let sl = self.stop_loss_ticks_effective();
        let trailing = self.trailing_sl_ticks_effective();
        if sl.is_none() && trailing.is_none() {
            return;
        }

        let candidates: Vec<u64> = self
            .positions
            .iter()
            .filter_map(|(&id, p)| {
                if p.settled || p.closed_via_take_profit || p.closed_via_stop_loss {
                    return None;
                }
                if !matches!(p.kind, Signal::Momentum { .. }) {
                    return None;
                }
                if p.legs.len() != 1 {
                    return None;
                }
                let leg = &p.legs[0];
                if leg.filled_size.is_zero() {
                    return None;
                }
                if p.take_profit_order_id.is_some() || p.stop_loss_order_id.is_some() {
                    return None;
                }
                Some(id)
            })
            .collect();

        for pos_id in candidates {
            let (token, entry_px, size, market) = {
                let Some(p) = self.positions.get(&pos_id) else {
                    continue;
                };
                let leg = &p.legs[0];
                (leg.token_id, leg.filled_price, leg.filled_size, p.market)
            };

            let book = self.orderbook.read().await;
            if !self.book_is_fresh(&book) {
                continue;
            }
            let Some((bb, _bs)) = book.best_bid(token) else {
                continue;
            };
            drop(book);

            // Update high water mark.
            if let Some(pos) = self.positions.get_mut(&pos_id) {
                if bb > pos.high_water_mark {
                    pos.high_water_mark = bb;
                }
            }

            let hwm = self
                .positions
                .get(&pos_id)
                .map(|p| p.high_water_mark)
                .unwrap_or(entry_px);
            let Some(trigger) =
                stop_loss_trigger_level(entry_px, hwm, sl, trailing)
            else {
                continue;
            };

            if bb > trigger {
                continue;
            }

            if self.mode == Mode::Paper {
                let sell_px = bb;
                let pnl = (sell_px - entry_px) * size;
                let (trade_log, interval_for_lab) = {
                    let Some(pos) = self.positions.get_mut(&pos_id) else {
                        continue;
                    };
                    let interval_for_lab = pos.interval_start_unix;
                    pos.pnl_usdc = pnl;
                    pos.win = false;
                    pos.settled = true;
                    pos.closed_via_stop_loss = true;
                    pos.last_exit_price = Some(sell_px);
                    self.last_sl_exit_ms_by_market.insert(market, now_ms());
                    self.total_trades += 1;
                    self.losses += 1;
                    let edge = match &pos.kind {
                        Signal::Momentum { edge, .. } => *edge,
                        Signal::ArbBoth { edge, .. } => *edge,
                    };
                    self.edge_sum += edge;
                    self.edge_count += 1;
                    self.pnl_day += pnl;
                    if self.cfg.risk.kill_switch_enabled {
                        let loss_limit = -(self.starting_balance * self.cfg.risk.daily_drawdown_frac);
                        if self.pnl_day <= loss_limit && !self.kill_switch_triggered {
                            self.kill_switch_triggered = true;
                            tracing::warn!(pnl_day = %self.pnl_day, loss_limit = %loss_limit, "kill switch triggered");
                        }
                    }
                    if let Some(e) = self.active_positions_by_market.get_mut(&market) {
                        *e = e.saturating_sub(1);
                    }
                    (
                        trade_log_record_from_pos(pos, "sl", Some(sell_px), now_ms()),
                        interval_for_lab,
                    )
                };
                self.write_trade_log_record(&trade_log);
                tracing::info!(
                    target: "sniper",
                    "SL paper #{} · bid {} <= trig {} · pnl {} USDC",
                    pos_id,
                    sell_px.round_dp(4),
                    trigger.round_dp(4),
                    pnl.round_dp(4),
                );
                paper_trade_terminal_line(format!(
                    "SL #{} · bid {} <= trig {} · pnl {} USDC",
                    pos_id,
                    sell_px.round_dp(4),
                    trigger.round_dp(4),
                    pnl.round_dp(4),
                ));
                if let Some(lab) = self.paper_lab.as_ref() {
                    lab.record_stop_loss(interval_for_lab, pnl);
                }
                continue;
            }

            let Some(live) = self.live.as_ref() else {
                tracing::warn!("stop-loss skip: live client missing");
                continue;
            };

            let sl_ot = sdk_order_type(self.cfg.trading.stop_loss_time_in_force);
            match live
                .place_limit_sell(token, bb, size, sl_ot.clone(), false)
                .await
            {
                Ok(oid) => {
                    let oid_n = normalize_order_id(&oid);
                    if let Some(pos) = self.positions.get_mut(&pos_id) {
                        pos.stop_loss_order_id = Some(oid_n.clone());
                        pos.exit_order_posted_at_ms = Some(now_ms());
                    }
                    self.exit_order_to_position
                        .insert(oid_n, (pos_id, ExitReason::StopLoss));
                    tracing::info!(
                        target: "sniper",
                        "SL order #{} @ {} trig {} · {:?}",
                        pos_id,
                        bb.round_dp(4),
                        trigger.round_dp(4),
                        sl_ot,
                    );
                }
                Err(e) => {
                    tracing::warn!(error = %e, position_id = pos_id, "stop-loss sell failed; retry later");
                }
            }
        }
    }

    async fn check_take_profits(&mut self, shutdown: &CancellationToken) {
        if shutdown.is_cancelled() {
            return;
        }
        let Some(tp) = self.take_profit_ticks_effective() else {
            return;
        };
        if tp.is_zero() {
            return;
        }

        let candidates: Vec<u64> = self
            .positions
            .iter()
            .filter_map(|(&id, p)| {
                if p.settled || p.closed_via_take_profit || p.closed_via_stop_loss {
                    return None;
                }
                if !matches!(p.kind, Signal::Momentum { .. }) {
                    return None;
                }
                if p.legs.len() != 1 {
                    return None;
                }
                let leg = &p.legs[0];
                if leg.filled_size.is_zero() {
                    return None;
                }
                if p.take_profit_order_id.is_some() || p.stop_loss_order_id.is_some() {
                    return None;
                }
                Some(id)
            })
            .collect();

        for pos_id in candidates {
            let (token, entry_px, size, market) = {
                let Some(p) = self.positions.get(&pos_id) else {
                    continue;
                };
                let leg = &p.legs[0];
                (leg.token_id, leg.filled_price, leg.filled_size, p.market)
            };

            let book = self.orderbook.read().await;
            if !self.book_is_fresh(&book) {
                continue;
            }
            let Some((bb, _bs)) = book.best_bid(token) else {
                continue;
            };
            // Also update HWM on TP check path.
            drop(book);
            if let Some(pos) = self.positions.get_mut(&pos_id) {
                if bb > pos.high_water_mark {
                    pos.high_water_mark = bb;
                }
            }

            if bb < entry_px + tp {
                continue;
            }

            if self.mode == Mode::Paper {
                let sell_px = bb;
                let pnl = (sell_px - entry_px) * size;
                let (trade_log, interval_for_lab, tp_ps, tp_ef) = {
                    let Some(pos) = self.positions.get_mut(&pos_id) else {
                        continue;
                    };
                    let interval_for_lab = pos.interval_start_unix;
                    let tp_ps = pos.entry_diag.p_strong;
                    let tp_ef = pos.entry_fill_ms;
                    pos.pnl_usdc = pnl;
                    pos.win = pnl > Decimal::ZERO;
                    pos.settled = true;
                    pos.closed_via_take_profit = true;
                    pos.last_exit_price = Some(sell_px);
                    self.total_trades += 1;
                    if pos.win {
                        self.wins += 1;
                    } else {
                        self.losses += 1;
                    }
                    let edge = match &pos.kind {
                        Signal::Momentum { edge, .. } => *edge,
                        Signal::ArbBoth { edge, .. } => *edge,
                    };
                    self.edge_sum += edge;
                    self.edge_count += 1;
                    self.pnl_day += pnl;
                    if self.cfg.risk.kill_switch_enabled {
                        let loss_limit = -(self.starting_balance * self.cfg.risk.daily_drawdown_frac);
                        if self.pnl_day <= loss_limit && !self.kill_switch_triggered {
                            self.kill_switch_triggered = true;
                            tracing::warn!(pnl_day = %self.pnl_day, loss_limit = %loss_limit, "kill switch triggered");
                        }
                    }
                    if let Some(e) = self.active_positions_by_market.get_mut(&market) {
                        *e = e.saturating_sub(1);
                    }
                    (
                        trade_log_record_from_pos(pos, "tp", Some(sell_px), now_ms()),
                        interval_for_lab,
                        tp_ps,
                        tp_ef,
                    )
                };
                self.write_trade_log_record(&trade_log);
                tracing::info!(
                    target: "sniper",
                    "TP paper #{} · bid {} · pnl {} USDC",
                    pos_id,
                    sell_px.round_dp(4),
                    pnl.round_dp(4),
                );
                paper_trade_terminal_line(format!(
                    "TP #{} · bid {} · pnl {} USDC",
                    pos_id,
                    sell_px.round_dp(4),
                    pnl.round_dp(4),
                ));
                if let Some(lab) = self.paper_lab.as_ref() {
                    lab.record_take_profit(interval_for_lab, pnl);
                    lab.on_closed_take_profit_timing(tp_ps, tp_ef, now_ms());
                }
                continue;
            }

            let Some(live) = self.live.as_ref() else {
                tracing::warn!("take-profit skip: live client missing");
                continue;
            };

            let tp_ot = sdk_order_type(self.cfg.trading.take_profit_time_in_force);
            match live
                .place_limit_sell(token, bb, size, tp_ot.clone(), false)
                .await
            {
                Ok(oid) => {
                    let oid_n = normalize_order_id(&oid);
                    if let Some(pos) = self.positions.get_mut(&pos_id) {
                        pos.take_profit_order_id = Some(oid_n.clone());
                        pos.exit_order_posted_at_ms = Some(now_ms());
                    }
                    self.exit_order_to_position
                        .insert(oid_n, (pos_id, ExitReason::TakeProfit));
                    tracing::info!(
                        target: "sniper",
                        "TP order #{} @ {} · {:?}",
                        pos_id,
                        bb.round_dp(4),
                        tp_ot,
                    );
                }
                Err(e) => {
                    tracing::warn!(error = %e, position_id = pos_id, "take-profit sell failed; retry later");
                }
            }
        }
    }

    /// Paper + `profit_window`: si pasó el plazo, el bid sigue por debajo de TP pero no por debajo de entrada → fallo de timing.
    async fn check_profit_deadline_violations(&mut self) {
        let Some(lab) = self.paper_lab.as_ref() else {
            return;
        };
        if !lab.profit_window_enabled() || self.mode != Mode::Paper {
            return;
        }
        let Some(tp) = self.take_profit_ticks_effective() else {
            return;
        };
        if tp.is_zero() {
            return;
        }

        let now = now_ms();
        let book = self.orderbook.read().await;
        if !self.book_is_fresh(&book) {
            return;
        }

        let mut hits: Vec<(u64, u64)> = Vec::new();
        for (&pos_id, p) in &self.positions {
            if p.profit_deadline_violation_logged {
                continue;
            }
            if !matches!(p.kind, Signal::Momentum { .. }) {
                continue;
            }
            if p.settled || p.closed_via_take_profit || p.closed_via_stop_loss {
                continue;
            }
            if p.legs.len() != 1 {
                continue;
            }
            let leg = &p.legs[0];
            if leg.filled_size.is_zero() {
                continue;
            }
            let Some(deadline) = p.tp_profit_deadline_ms else {
                continue;
            };
            if now < deadline {
                continue;
            }
            let Some((bb, _)) = book.best_bid(leg.token_id) else {
                continue;
            };
            let entry_px = leg.filled_price;
            if bb >= entry_px + tp {
                continue;
            }
            if bb < entry_px {
                continue;
            }
            hits.push((pos_id, p.interval_start_unix));
        }
        drop(book);

        for (pos_id, iv) in hits {
            let Some(pos) = self.positions.get_mut(&pos_id) else {
                continue;
            };
            if pos.profit_deadline_violation_logged {
                continue;
            }
            pos.profit_deadline_violation_logged = true;
            tracing::warn!(
                target: "sniper",
                position_id = pos_id,
                interval_start_unix = iv,
                "profit_window · venció plazo sin TP (bid ≥ entrada y < entry+tp)"
            );
            lab.record_profit_deadline_miss(iv);
        }
    }

    async fn on_cancel(&mut self, cancel: CancelEvent) {
        let oid = normalize_order_id(&cancel.order_id);
        let Some(&(pos_id, leg_idx)) = self.order_to_leg.get(&oid) else { return };
        let Some(pos) = self.positions.get_mut(&pos_id) else { return };
        if pos.settled {
            return;
        }
        let Some(leg) = pos.legs.get_mut(leg_idx) else { return };
        if !leg.filled_size.is_zero() {
            return;
        }

        if let Some(live) = &self.live {
            let _ = live.cancel_order(&oid).await;
        }

        // Keep as unfilled for settlement (filled_size remains 0).
        leg.filled_price = leg.intended_price;
    }

    pub async fn submit_signal(&mut self, signal: Signal, shutdown: &CancellationToken) {
        if shutdown.is_cancelled() {
            return;
        }

        let market = match &signal {
            Signal::Momentum { market, .. } => *market,
            Signal::ArbBoth { market, .. } => *market,
        };

        if !self.can_place_for_market(market) {
            return;
        }
        if self.kill_switch_triggered {
            return;
        }

        // Signal dedup: one entry per (market, interval).
        let interval_key = match &signal {
            Signal::Momentum { market, interval_start_unix, .. } => (*market, *interval_start_unix),
            Signal::ArbBoth { market, interval_start_unix, .. } => (*market, *interval_start_unix),
        };
        if self.entered_intervals.contains(&interval_key) {
            return;
        }

        // Book staleness guard on entry.
        {
            let book = self.orderbook.read().await;
            if !self.book_is_fresh(&book) {
                return;
            }
        }

        self.last_entry_ms_by_market.insert(market, now_ms());

        let position_id = self.next_position_id;
        self.next_position_id += 1;

        let (interval_start_unix, close_time_unix, token_id_up, token_id_down) = match &signal {
            Signal::Momentum {
                interval_start_unix,
                close_time_unix,
                token_id_up,
                token_id_down,
                ..
            } => (*interval_start_unix, *close_time_unix, *token_id_up, *token_id_down),
            Signal::ArbBoth {
                interval_start_unix,
                close_time_unix,
                token_id_up,
                token_id_down,
                ..
            } => (*interval_start_unix, *close_time_unix, *token_id_up, *token_id_down),
        };

        let risk_budget_usdc = self.starting_balance * self.cfg.risk.risk_per_trade_frac;

        // Snapshot orderbook under a read lock only once.
        let snapshot = self.orderbook.read().await;

        let mut legs: Vec<PositionLeg> = Vec::new();

        // Helper for limit price computation.
        let limit_price_from_best = |best_bid: Option<(Price, Price)>,
                                     best_ask: (Price, Price)|
         -> (Price, Price, Price) {
            let (ask_price, ask_size) = best_ask;
            let slippage_bps = self.cfg.trading.entry_limit_slippage_bps;
            if self.cfg.trading.post_only {
                let (bid_price, _) = best_bid.unwrap_or((ask_price, Price::ZERO));
                let limit = (bid_price * (Decimal::ONE - slippage_bps / Decimal::from(10_000u64)))
                    .max(Decimal::ZERO);
                (limit, ask_price, ask_size)
            } else {
                let limit = crate::utils::apply_slippage_bps(ask_price, slippage_bps);
                (limit, ask_price, ask_size)
            }
        };

        match &signal {
            Signal::Momentum {
                outcome,
                ..
            } => {
                let token_side = match outcome {
                    Outcome::Up => token_id_up,
                    Outcome::Down => token_id_down,
                };
                let Some((best_bid, best_bid_size)) = snapshot.best_bid(token_side).map(|(p, s)| (p, s)) else {
                    return;
                };
                let Some((best_ask_price, best_ask_size)) = snapshot.best_ask(token_side) else { return; };

                // Aggressive limit price is on ask side unless post_only requested.
                let (limit_price, _, _) = limit_price_from_best(Some((best_bid, best_bid_size)), (best_ask_price, best_ask_size));

                if limit_price.is_zero() || best_ask_size.is_zero() {
                    return;
                }

                // Depth-aware sizing: cap to ratio × ask_size.
                let depth_cap = best_ask_size * self.cfg.trading.max_size_to_ask_ratio;
                let mut size_risk = risk_budget_usdc / limit_price;
                if size_risk > depth_cap {
                    size_risk = depth_cap;
                }
                if size_risk.is_zero() {
                    return;
                }

                legs.push(PositionLeg {
                    token_id: token_side,
                    outcome: *outcome,
                    intended_price: limit_price,
                    intended_size: size_risk,
                    filled_price: Price::ZERO,
                    filled_size: Price::ZERO,
                    order_id: None,
                    realized_slippage_bps: Price::ZERO,
                });
            }
            Signal::ArbBoth { .. } => {
                let Some((best_ask_up_price, best_ask_up_size)) = snapshot.best_ask(token_id_up) else { return; };
                let Some((best_ask_down_price, best_ask_down_size)) = snapshot.best_ask(token_id_down) else { return; };

                let p_sum = best_ask_up_price + best_ask_down_price;
                if p_sum > Decimal::ONE {
                    return;
                }
                let mut shared_size = if p_sum.is_zero() {
                    Decimal::ZERO
                } else {
                    risk_budget_usdc / p_sum
                };
                shared_size = shared_size.min(best_ask_up_size).min(best_ask_down_size);
                if shared_size.is_zero() {
                    return;
                }

                let (_limit_up, _market_up_price, _market_up_size) =
                    limit_price_from_best(None, (best_ask_up_price, best_ask_up_size));
                let (_limit_down, _market_down_price, _market_down_size) =
                    limit_price_from_best(None, (best_ask_down_price, best_ask_down_size));

                legs.push(PositionLeg {
                    token_id: token_id_up,
                    outcome: Outcome::Up,
                    intended_price: _limit_up,
                    intended_size: shared_size,
                    filled_price: Price::ZERO,
                    filled_size: Price::ZERO,
                    order_id: None,
                    realized_slippage_bps: Price::ZERO,
                });
                legs.push(PositionLeg {
                    token_id: token_id_down,
                    outcome: Outcome::Down,
                    intended_price: _limit_down,
                    intended_size: shared_size,
                    filled_price: Price::ZERO,
                    filled_size: Price::ZERO,
                    order_id: None,
                    realized_slippage_bps: Price::ZERO,
                });
            }
        }

        let mut entry_diag = build_entry_diag(&signal, &snapshot, close_time_unix);
        if let Some(lab) = self.paper_lab.as_ref() {
            entry_diag.rl_action_applied = lab.last_rl_action_for_diag();
        }
        let mut pos = Position {
            position_id,
            market,
            interval_start_unix,
            close_time_unix,
            created_at_ms: now_ms(),
            entry_fill_ms: None,
            tp_profit_deadline_ms: None,
            profit_deadline_violation_logged: false,
            kind: signal,
            entry_diag,
            legs,
            settled: false,
            closed_via_take_profit: false,
            closed_via_stop_loss: false,
            take_profit_order_id: None,
            stop_loss_order_id: None,
            high_water_mark: Price::ZERO,
            exit_order_posted_at_ms: None,
            pnl_usdc: Decimal::ZERO,
            win: false,
            last_exit_price: None,
        };

        // PAPER fill simulation: only fills if limit crosses current best ask.
        if self.mode == Mode::Paper {
            let entry_tif = self.cfg.trading.entry_time_in_force;
            for leg in &mut pos.legs {
                if let Some((best_ask_price, best_ask_size)) = snapshot.best_ask(leg.token_id) {
                    if leg.intended_price >= best_ask_price {
                        match entry_tif {
                            ClobOrderTimeInForce::Fok => {
                                if leg.intended_size <= best_ask_size {
                                    leg.filled_price = best_ask_price;
                                    leg.filled_size = leg.intended_size;
                                }
                            }
                            ClobOrderTimeInForce::Fak | ClobOrderTimeInForce::Gtc => {
                                leg.filled_price = best_ask_price;
                                leg.filled_size = leg.intended_size.min(best_ask_size);
                            }
                        }
                        // Slippage tracking (paper).
                        if !leg.filled_size.is_zero() && !leg.intended_price.is_zero() {
                            let slip = ((leg.filled_price - leg.intended_price) / leg.intended_price)
                                * Decimal::from(10_000u32);
                            leg.realized_slippage_bps = slip;
                            self.slippage_sum += slip.abs();
                            self.slippage_count += 1;
                            // Init HWM from fill.
                            if pos.high_water_mark.is_zero() {
                                pos.high_water_mark = leg.filled_price;
                            }
                        }
                    }
                }
            }
            let has_fill = pos.legs.iter().any(|l| !l.filled_size.is_zero());
            // FAK 0-fill cleanup: don't insert ghost positions.
            if !has_fill {
                return;
            }
            pos.entry_fill_ms = Some(now_ms());
            if matches!(pos.kind, Signal::Momentum { .. }) {
                if let Some(ref lab) = self.paper_lab {
                    let tp_eff = self.take_profit_ticks_effective();
                    let th = self.cfg.momentum.strong_prob_threshold;
                    lab.apply_profit_deadline_to_position(&mut pos, tp_eff, th);
                }
            }
            if let Some(lab) = self.paper_lab.as_ref() {
                if matches!(pos.kind, Signal::Momentum { .. }) {
                    lab.record_paper_entry(interval_start_unix, pos.entry_diag.p_strong, pos.entry_diag.time_remaining_sec);
                }
            }
            self.entered_intervals.insert(interval_key);
            log_trade_entry(true, position_id, &pos);
            self.positions.insert(position_id, pos);
            *self.active_positions_by_market.entry(market).or_default() += 1;
            return;
        }

        let Some(live) = &self.live else {
            tracing::warn!("OrderManager: live mode but no LivePolymarket initialized");
            return;
        };

        let entry_ot = sdk_order_type(self.cfg.trading.entry_time_in_force);
        let post_only_entry =
            self.cfg.trading.post_only && matches!(entry_ot, OrderType::GTC);

        for (leg_idx, leg) in pos.legs.iter_mut().enumerate() {
            let oid = match live
                .place_limit_buy(
                    leg.token_id,
                    leg.intended_price,
                    leg.intended_size,
                    entry_ot.clone(),
                    post_only_entry,
                )
                .await
            {
                Ok(oid) => oid,
                Err(e) => {
                    tracing::warn!(error = %e, "failed to place limit order; leaving leg unfilled");
                    continue;
                }
            };

            let oid_norm = normalize_order_id(&oid);
            leg.order_id = Some(oid_norm.clone());
            self.order_to_leg.insert(oid_norm.clone(), (position_id, leg_idx));

            // Solo GTC puede quedar resting; FAK/FOK terminan en el post.
            if matches!(entry_ot, OrderType::GTC) {
                let cancel_tx = self.cancel_tx.clone();
                let order_id_for_cancel = oid_norm.clone();
                let cancel_ms = self.cfg.trading.cancel_if_unfilled_ms;
                let shutdown_clone = shutdown.clone();
                tokio::spawn(async move {
                    sleep(Duration::from_millis(cancel_ms)).await;
                    if shutdown_clone.is_cancelled() {
                        return;
                    }
                    let _ = cancel_tx.send(CancelEvent { order_id: order_id_for_cancel });
                });
            }
        }

        // Ghost position fix: only insert if at least one leg got an order_id.
        let any_order = pos.legs.iter().any(|l| l.order_id.is_some());
        if !any_order {
            tracing::warn!(
                target: "sniper",
                position_id,
                "skip ghost position: no legs received order_id"
            );
            return;
        }
        self.entered_intervals.insert(interval_key);
        log_trade_entry(false, position_id, &pos);
        self.positions.insert(position_id, pos);
        *self.active_positions_by_market.entry(market).or_default() += 1;
    }

    async fn reconcile_orphan_exit_orders(&mut self) {
        let timeout = self.cfg.trading.exit_order_timeout_ms;
        let now = now_ms();
        let mut stale_tp: Vec<u64> = Vec::new();
        let mut stale_sl: Vec<u64> = Vec::new();

        for (&pos_id, pos) in &self.positions {
            if pos.settled {
                continue;
            }
            if let Some(posted_at) = pos.exit_order_posted_at_ms {
                if now.saturating_sub(posted_at) > timeout {
                    if pos.take_profit_order_id.is_some() {
                        stale_tp.push(pos_id);
                    }
                    if pos.stop_loss_order_id.is_some() {
                        stale_sl.push(pos_id);
                    }
                }
            }
        }

        for pos_id in stale_tp {
            if let Some(pos) = self.positions.get_mut(&pos_id) {
                if let Some(oid) = pos.take_profit_order_id.take() {
                    self.exit_order_to_position.remove(&oid);
                    if let Some(live) = &self.live {
                        let _ = live.cancel_order(&oid).await;
                    }
                    pos.exit_order_posted_at_ms = None;
                    tracing::info!(target: "sniper", "orphan TP cleared #{}", pos_id);
                }
            }
        }
        for pos_id in stale_sl {
            if let Some(pos) = self.positions.get_mut(&pos_id) {
                if let Some(oid) = pos.stop_loss_order_id.take() {
                    self.exit_order_to_position.remove(&oid);
                    if let Some(live) = &self.live {
                        let _ = live.cancel_order(&oid).await;
                    }
                    pos.exit_order_posted_at_ms = None;
                    tracing::info!(target: "sniper", "orphan SL cleared #{}", pos_id);
                }
            }
        }
    }

    async fn settle_positions(&mut self) {
        let now_sec = now_unix_sec();
        let mut settled_ids: Vec<u64> = Vec::new();

        for (&pos_id, pos) in &self.positions {
            if pos.settled {
                continue;
            }
            if now_sec < pos.close_time_unix {
                continue;
            }
            settled_ids.push(pos_id);
        }

        if settled_ids.is_empty() {
            return;
        }

        for pos_id in settled_ids {
            // Cancel any pending TP/SL exit orders before settling.
            if let Some(pos) = self.positions.get_mut(&pos_id) {
                if let Some(oid) = pos.take_profit_order_id.take() {
                    self.exit_order_to_position.remove(&oid);
                    if let Some(live) = &self.live {
                        let _ = live.cancel_order(&oid).await;
                    }
                }
                if let Some(oid) = pos.stop_loss_order_id.take() {
                    self.exit_order_to_position.remove(&oid);
                    if let Some(live) = &self.live {
                        let _ = live.cancel_order(&oid).await;
                    }
                }
            }

            let trade_log = {
                let Some(pos) = self.positions.get_mut(&pos_id) else {
                    continue;
                };
                if pos.settled {
                    continue;
                }
                let spot = {
                    let guard = self.spot_intervals.read().await;
                    guard.get(&(pos.market, pos.interval_start_unix)).cloned()
                };
                let Some(spot) = spot else {
                    continue;
                };
                if !spot.open_set || !spot.close_set {
                    continue;
                }

                let up_resolves = spot.close_price >= spot.open_price;
                let mut payout = Decimal::ZERO;
                let mut cost = Decimal::ZERO;
                for leg in &pos.legs {
                    cost += leg.filled_size * leg.filled_price;
                    let correct = match leg.outcome {
                        Outcome::Up => up_resolves,
                        Outcome::Down => !up_resolves,
                    };
                    if correct {
                        payout += leg.filled_size;
                    }
                }

                let pnl = payout - cost;
                let lab_settle = self.mode == Mode::Paper
                    && matches!(pos.kind, Signal::Momentum { .. })
                    && !pos.closed_via_take_profit
                    && !pos.closed_via_stop_loss
                    && pos.legs.iter().any(|l| !l.filled_size.is_zero());
                let interval_for_lab = pos.interval_start_unix;

                pos.pnl_usdc = pnl;
                pos.win = pnl > Decimal::ZERO;
                pos.settled = true;

                tracing::info!(
                    target: "sniper",
                    "SETTLE #{} · pnl {} USDC",
                    pos_id,
                    pnl.round_dp(4),
                );
                if matches!(self.mode, Mode::Paper) {
                    paper_trade_terminal_line(format!(
                        "SETTLE #{} · pnl {} USDC",
                        pos_id,
                        pnl.round_dp(4),
                    ));
                }

                if lab_settle {
                    if let Some(lab) = self.paper_lab.as_ref() {
                        lab.record_settle_no_tp(interval_for_lab, pnl);
                    }
                }

                self.total_trades += 1;
                if pos.win {
                    self.wins += 1;
                } else {
                    self.losses += 1;
                }

                let edge = match &pos.kind {
                    Signal::Momentum { edge, .. } => *edge,
                    Signal::ArbBoth { edge, .. } => *edge,
                };
                self.edge_sum += edge;
                self.edge_count += 1;
                self.pnl_day += pnl;

                if self.cfg.risk.kill_switch_enabled {
                    let loss_limit = -(self.starting_balance * self.cfg.risk.daily_drawdown_frac);
                    if self.pnl_day <= loss_limit && !self.kill_switch_triggered {
                        self.kill_switch_triggered = true;
                        tracing::warn!(pnl_day = %self.pnl_day, loss_limit = %loss_limit, "kill switch triggered");
                    }
                }

                let mkey = pos.market;
                let int_u = pos.interval_start_unix;
                let entry = self.active_positions_by_market.entry(mkey).or_default();
                *entry = entry.saturating_sub(1);
                self.entered_intervals.remove(&(mkey, int_u));

                trade_log_record_from_pos(pos, "settle", None, now_ms())
            };
            self.write_trade_log_record(&trade_log);
        }
    }
}

