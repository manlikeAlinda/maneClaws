//! Portfolio-level backtesting engine.
//!
//! `simulate_from_cache` runs the full feature → regime → signal pipeline on
//! cached candle data and produces a `BacktestReport` with realistic capital
//! accounting, fee modelling, MFE/MAE excursion tracking, an equity curve,
//! score-bucket calibration, and regime-stratified performance metrics.
//!
//! `walk_forward` slices the same data into rolling train/test windows and
//! returns one report per test window — providing an estimate of forward
//! performance under shifting market conditions.

use crate::candles::{Candle, Interval};
use crate::telemetry::{FeatureSnapshot, TradeRecord};
use crate::{features, regime, signals, state};
use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BacktestConfig {
    pub starting_capital: f64,
    /// Taker fee per side (e.g. 0.001 = 0.1 %).
    pub fee_rate: f64,
    /// One-way slippage estimate as a fraction of price (e.g. 0.0005 = 0.05 %).
    pub slippage_pct: f64,
    /// Fraction of equity risked per trade (e.g. 0.01 = 1 %).
    pub risk_fraction: f64,
    /// Hard cap on risk per trade in USDT.
    pub max_risk_usdt: f64,
    /// Hard cap on a single position's notional as a fraction of current equity
    /// (e.g. 0.20 = 20%). Mirrors the live pipeline's `BOT_MAX_TRADE_NOTIONAL_FRACTION`
    /// cap in `risk::size_entry_long`. Without this, risk-parity sizing (risk_usdt /
    /// stop_distance) on a tight ATR-based stop can imply notional many multiples of
    /// account equity — exposure the live spot-only bot could never actually take.
    pub max_notional_fraction: f64,
}

impl Default for BacktestConfig {
    fn default() -> Self {
        Self {
            starting_capital: 10_000.0,
            fee_rate: 0.001,
            slippage_pct: 0.0005,
            risk_fraction: 0.01,
            max_risk_usdt: 200.0,
            max_notional_fraction: 0.20,
        }
    }
}

// ---------------------------------------------------------------------------
// Output structures
// ---------------------------------------------------------------------------

/// Performance metrics aggregated across all completed trades.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BacktestMetrics {
    pub final_equity: f64,
    pub total_return_pct: f64,
    pub max_drawdown_pct: f64,
    /// Annualised Sharpe ratio estimated from per-trade returns.
    pub sharpe_ratio: f64,
    /// Gross profits / gross losses.
    pub profit_factor: f64,
    pub win_rate: f64,
    pub trade_count: usize,
    pub winning_trades: usize,
    pub losing_trades: usize,
    pub avg_winner_pct: f64,
    pub avg_loser_pct: f64,
    pub avg_trade_duration_ms: u64,
    pub total_fees_usdt: f64,
    pub total_slippage_usdt: f64,
}

/// Signal score partition with outcome statistics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoreBucket {
    pub label: String,
    pub range_low: f64,
    pub range_high: f64,
    pub count: usize,
    pub wins: usize,
    pub win_rate: f64,
    pub avg_return_pct: f64,
    /// Expected value: win_rate × avg_winner − loss_rate × |avg_loser|.
    pub expectancy_pct: f64,
    pub profit_factor: f64,
}

/// Per-regime performance breakdown.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegimeStats {
    pub regime: String,
    pub trades: usize,
    pub wins: usize,
    pub win_rate: f64,
    pub avg_return_pct: f64,
    pub expectancy_pct: f64,
    pub profit_factor: f64,
}

/// Full backtest output.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BacktestReport {
    pub config: BacktestConfig,
    /// Completed trade ledger.
    pub trades: Vec<TradeRecord>,
    /// Equity sampled at every signal step: (timestamp_ms, equity_usdt).
    pub equity_curve: Vec<(u64, f64)>,
    /// Drawdown from peak at each sample: (timestamp_ms, drawdown_pct).
    pub drawdown_curve: Vec<(u64, f64)>,
    pub metrics: BacktestMetrics,
    /// Score calibration — are higher scores associated with better outcomes?
    pub score_buckets: Vec<ScoreBucket>,
    /// Performance breakdown by detected regime at entry.
    pub regime_stats: Vec<RegimeStats>,
    /// How many simulated 5m bars were classified into each regime, independent of
    /// whether any trade followed. A trade-count-only view (`regime_stats`) can't tell
    /// you whether a strategy never fired because the regime never occurred, or because
    /// the regime occurred but the entry gate never triggered — this does.
    pub regime_bar_counts: Vec<(String, usize)>,
}

// ---------------------------------------------------------------------------
// Internal simulation state
// ---------------------------------------------------------------------------

struct SimPosition {
    regime: String,
    strategy: String,
    score: f64,
    entry_time_ms: u64,
    entry_price: f64,
    qty: f64,
    stop_price: f64,
    tp_price: Option<f64>,
    peak_price: f64,
    last_peak_time_ms: u64,
    entry_fee: f64,
    features: FeatureSnapshot,
    mfe_price: f64,
    mae_price: f64,
}

// ---------------------------------------------------------------------------
// Data loading
// ---------------------------------------------------------------------------

fn load_cached(data_dir: &str, symbol: &str, interval: Interval) -> Result<Vec<Candle>> {
    let path = std::path::Path::new(data_dir).join(format!("{symbol}_{}.json", interval.as_str()));
    if !path.exists() {
        return Err(anyhow!("Missing cache file: {}", path.display()));
    }
    let s = fs::read_to_string(&path)?;
    Ok(serde_json::from_str::<Vec<Candle>>(&s)?)
}

// ---------------------------------------------------------------------------
// Core simulation loop
// ---------------------------------------------------------------------------

fn simulate(
    candles_1m: &[Candle],
    candles_5m: &[Candle],
    candles_1h: &[Candle],
    config: &BacktestConfig,
) -> BacktestReport {
    let mut equity = config.starting_capital;
    let mut peak_equity = equity;
    let mut total_fees = 0.0_f64;
    let mut total_slippage = 0.0_f64;

    let mut equity_curve: Vec<(u64, f64)> = Vec::new();
    let mut drawdown_curve: Vec<(u64, f64)> = Vec::new();
    let mut completed_trades: Vec<TradeRecord> = Vec::new();
    let mut regime_bar_counts: HashMap<String, usize> = HashMap::new();

    let mut open_pos: Option<SimPosition> = None;

    for i in 0..candles_5m.len() {
        let end = i + 1;
        if end < 120 {
            continue;
        }

        let w5 = &candles_5m[end.saturating_sub(200)..end];

        let last_time_5m = w5.last().unwrap().open_time;
        let w1m_idx = candles_1m
            .iter()
            .position(|c| c.open_time >= last_time_5m)
            .unwrap_or(candles_1m.len().saturating_sub(1));
        let w1m_end = (w1m_idx + 1).min(candles_1m.len());
        let w1m = &candles_1m[w1m_idx.saturating_sub(200)..w1m_end];

        // Bug fix (look-ahead bias): this used to be a fixed `candles_1h[len-200..]`
        // slice — the *last* 200 1h candles of the whole dataset, on every iteration,
        // regardless of the current bar's time. For any bar before the final ~8 days of
        // the sample, that fed compute_features() (and therefore htf_bullish/bearish_bias,
        // which gate Trending classification and the bearish-entry veto) 1h candles from
        // the future. Window it the same way w1m is: up to and including the 1h candle at
        // or after the current 5m bar's time.
        let h1_idx = candles_1h
            .iter()
            .position(|c| c.open_time >= last_time_5m)
            .unwrap_or(candles_1h.len().saturating_sub(1));
        let h1_end = (h1_idx + 1).min(candles_1h.len());
        let w1h = &candles_1h[h1_idx.saturating_sub(200)..h1_end];

        let f = match features::compute_features(w1m, w5, w1h) {
            Ok(x) => x,
            Err(_) => continue,
        };
        let r = regime::detect_regime(&f);
        *regime_bar_counts.entry(format!("{:?}", r.regime)).or_insert(0) += 1;
        let current_candle = w5.last().unwrap();
        let price = current_candle.close;
        let now_ms = current_candle.open_time.max(0) as u64;

        // --- Manage open position ---
        if let Some(ref mut pos) = open_pos {
            let bar_mfe = (current_candle.high - pos.entry_price).max(0.0);
            let bar_mae = (pos.entry_price - current_candle.low).max(0.0);
            if bar_mfe > pos.mfe_price { pos.mfe_price = bar_mfe; }
            if bar_mae > pos.mae_price { pos.mae_price = bar_mae; }

            // Intrabar stop and TP checks.
            let stop_hit = current_candle.low <= pos.stop_price;
            let tp_hit = pos.tp_price.is_some_and(|tp| current_candle.high >= tp);

            let intrabar_exit: Option<(f64, &'static str)> = if stop_hit {
                Some((pos.stop_price, "stop_hit"))
            } else if tp_hit {
                Some((pos.tp_price.unwrap(), "tp_hit"))
            } else {
                None
            };

            // Replicate position manager state for trailing / time-stop logic.
            let mut st = state::BotState::new(equity);
            st.enter_long(pos.entry_price, pos.qty, pos.stop_price, pos.tp_price, pos.entry_time_ms);
            if let state::Position::Long { ref mut peak_price, ref mut last_peak_time_ms, .. } = st.position {
                *peak_price = pos.peak_price;
                *last_peak_time_ms = pos.last_peak_time_ms;
            }

            // Mirror pipeline::run_once_core's mean-reversion-specific exit: while the
            // *current* bar reads as Ranging, once price reclaims the BB mid-band with
            // RSI back above 50, take profit rather than waiting on the 3R target or the
            // trailing stop. This applies to any open long while Ranging, not only ones
            // opened by the mean-reversion strategy — that matches live behavior exactly,
            // odd as it looks; see pipeline/mod.rs's `range_tp` check.
            //
            // Bug fix (mirrors the live-pipeline fix): bb_mid20_5m is a live 20-period SMA
            // that drifts with price, so "price reclaimed the mid-band" does not imply
            // "price recovered above what we paid" — this exit was firing on ~45% of all
            // trades at an average of -0.10% net. Gate it on the higher of the mid-band and
            // a cost-aware breakeven line, using this simulation's own fee/slippage config
            // (rather than the live pipeline's BOT_TAKER_FEE_BPS/BOT_SLIPPAGE_BPS env vars,
            // which may not match `config` here) so the gate is consistent with the costs
            // actually being simulated.
            let round_trip_cost_frac = 2.0 * config.fee_rate + 2.0 * config.slippage_pct;
            let breakeven_price = pos.entry_price * (1.0 + round_trip_cost_frac);
            let range_tp = r.regime == regime::Regime::Ranging
                && price >= f.bb_mid20_5m.max(breakeven_price)
                && f.rsi14_5m >= 50.0;
            let (decision, exit_msg) = if range_tp {
                (
                    state::PositionDecision::ExitLong,
                    "Mean reversion done. We take profit.".to_string(),
                )
            } else {
                st.manage_open_position(price, f.atr14_5m, now_ms, r.bearish_bias)
            };
            // Sync back updated trailing stop.
            if let state::Position::Long { stop_price, peak_price, last_peak_time_ms, .. } = st.position {
                pos.stop_price = stop_price;
                pos.peak_price = peak_price;
                pos.last_peak_time_ms = last_peak_time_ms;
            }

            let exit_info: Option<(f64, String)> = intrabar_exit
                .map(|(ep, r)| (ep, r.to_string()))
                .or_else(|| {
                    if decision == state::PositionDecision::ExitLong {
                        Some((price, exit_msg))
                    } else {
                        None
                    }
                });

            if let Some((exit_price, exit_reason)) = exit_info {
                let exit_slip = exit_price * config.slippage_pct * pos.qty;
                let exit_fee = pos.qty * exit_price * config.fee_rate;
                total_fees += exit_fee;
                total_slippage += exit_slip;

                let effective_exit = exit_price - exit_price * config.slippage_pct;
                let gross = (effective_exit - pos.entry_price) * pos.qty;
                let net = gross - pos.entry_fee - exit_fee;
                equity += net;
                if equity > peak_equity { peak_equity = equity; }

                let mut rec = TradeRecord::open(
                    "BACKTEST",
                    &pos.regime,
                    &pos.strategy,
                    pos.score,
                    pos.entry_time_ms,
                    pos.entry_price,
                    pos.qty,
                    pos.stop_price,
                    pos.tp_price,
                    pos.entry_fee,
                    pos.features.clone(),
                );
                rec.close(
                    now_ms,
                    exit_price,
                    &exit_reason,
                    exit_fee,
                    Some(pos.mfe_price * pos.qty),
                    Some(pos.mae_price * pos.qty),
                );
                rec.slippage_usdt = Some(exit_slip + pos.entry_price * config.slippage_pct * pos.qty);
                completed_trades.push(rec);
                open_pos = None;
            }
        }

        let dd_pct = if peak_equity > 0.0 {
            ((peak_equity - equity) / peak_equity) * 100.0
        } else {
            0.0
        };
        equity_curve.push((now_ms, equity));
        drawdown_curve.push((now_ms, dd_pct));

        // --- Evaluate entry if flat ---
        if open_pos.is_none() {
            let sig = signals::entry_long_signal(
                r.regime,
                &f,
                price,
                r.vol_ratio,
                r.vol_squeeze,
                r.htf_bullish,
                r.bearish_bias,
            );

            if sig.action == signals::Action::EnterLong
                && let Some(stop) = sig.stop_price
            {
                let stop_dist = (price - stop).max(1e-12);
                let risk_usdt = (equity * config.risk_fraction).min(config.max_risk_usdt);
                let mut qty = risk_usdt / stop_dist;

                // Cap notional the same way the live pipeline does: without this, a
                // tight ATR stop can imply a position many multiples of account
                // equity, which is not a position the live spot-only bot could
                // actually hold.
                let cap = (equity * config.max_notional_fraction).max(0.0);
                if cap > 0.0 && qty * price > cap {
                    qty = cap / price;
                }

                if qty > 0.0 && price * qty >= 5.0 {
                    let entry_slip = price * config.slippage_pct;
                    let effective_entry = price + entry_slip;
                    let entry_fee = qty * effective_entry * config.fee_rate;
                    total_fees += entry_fee;
                    total_slippage += entry_slip * qty;

                    let snap = FeatureSnapshot::from_features_and_regime(&f, &r);
                    // Mirror pipeline::run_once_core's hard take-profit at 3:1 reward-to-risk,
                    // computed off the pre-slippage signal price exactly as the live pipeline
                    // does (`let tp = md.price + 3.0 * (md.price - stop);`). Previously this was
                    // always `None`, so the backtest never let a trade reach a take-profit at
                    // all — every win depended on the trailing stop catching a favorable move.
                    let tp_price = Some(price + 3.0 * (price - stop));
                    open_pos = Some(SimPosition {
                        regime: format!("{:?}", r.regime),
                        strategy: sig.strategy.clone(),
                        score: sig.score,
                        entry_time_ms: now_ms,
                        entry_price: effective_entry,
                        qty,
                        stop_price: stop,
                        tp_price,
                        peak_price: effective_entry,
                        last_peak_time_ms: now_ms,
                        entry_fee,
                        features: snap,
                        mfe_price: 0.0,
                        mae_price: 0.0,
                    });
                }
            }
        }
    }

    // Force-close any still-open position at final candle price.
    if let Some(pos) = open_pos.take()
        && let Some(last) = candles_5m.last()
    {
        let exit_price = last.close;
        let exit_fee = pos.qty * exit_price * config.fee_rate;
        total_fees += exit_fee;
        let gross = (exit_price - pos.entry_price) * pos.qty;
        let net = gross - pos.entry_fee - exit_fee;
        equity += net;

        let mut rec = TradeRecord::open(
            "BACKTEST",
            &pos.regime,
            &pos.strategy,
            pos.score,
            pos.entry_time_ms,
            pos.entry_price,
            pos.qty,
            pos.stop_price,
            pos.tp_price,
            pos.entry_fee,
            pos.features,
        );
        rec.close(
            last.open_time.max(0) as u64,
            exit_price,
            "end_of_data",
            exit_fee,
            Some(pos.mfe_price * pos.qty),
            Some(pos.mae_price * pos.qty),
        );
        completed_trades.push(rec);
    }

    let metrics = compute_metrics(
        config.starting_capital,
        equity,
        &completed_trades,
        &drawdown_curve,
        total_fees,
        total_slippage,
    );
    let score_buckets = compute_score_buckets(&completed_trades);
    let regime_stats = compute_regime_stats(&completed_trades);
    let mut regime_bar_counts: Vec<(String, usize)> = regime_bar_counts.into_iter().collect();
    regime_bar_counts.sort_by(|a, b| a.0.cmp(&b.0));

    BacktestReport {
        config: config.clone(),
        trades: completed_trades,
        equity_curve,
        drawdown_curve,
        metrics,
        score_buckets,
        regime_stats,
        regime_bar_counts,
    }
}

// ---------------------------------------------------------------------------
// Metrics helpers
// ---------------------------------------------------------------------------

fn compute_metrics(
    starting: f64,
    final_equity: f64,
    trades: &[TradeRecord],
    drawdown_curve: &[(u64, f64)],
    total_fees: f64,
    total_slippage: f64,
) -> BacktestMetrics {
    let max_dd = drawdown_curve.iter().map(|(_, d)| *d).fold(0.0_f64, f64::max);

    if trades.is_empty() {
        return BacktestMetrics {
            final_equity,
            total_return_pct: (final_equity / starting.max(1e-12) - 1.0) * 100.0,
            max_drawdown_pct: max_dd,
            sharpe_ratio: 0.0,
            profit_factor: 0.0,
            win_rate: 0.0,
            trade_count: 0,
            winning_trades: 0,
            losing_trades: 0,
            avg_winner_pct: 0.0,
            avg_loser_pct: 0.0,
            avg_trade_duration_ms: 0,
            total_fees_usdt: total_fees,
            total_slippage_usdt: total_slippage,
        };
    }

    let returns_pct: Vec<f64> = trades
        .iter()
        .filter_map(|t| {
            let net = t.net_pnl_usdt?;
            if t.position_value_usdt > 0.0 { Some(net / t.position_value_usdt * 100.0) } else { None }
        })
        .collect();

    let winners: Vec<f64> = returns_pct.iter().copied().filter(|r| *r > 0.0).collect();
    let losers:  Vec<f64> = returns_pct.iter().copied().filter(|r| *r <= 0.0).collect();

    let win_rate = winners.len() as f64 / returns_pct.len().max(1) as f64;
    let avg_winner = if winners.is_empty() { 0.0 } else { winners.iter().sum::<f64>() / winners.len() as f64 };
    let avg_loser  = if losers.is_empty()  { 0.0 } else { losers.iter().sum::<f64>()  / losers.len()  as f64 };

    let gross_profit: f64 = trades.iter().filter_map(|t| t.net_pnl_usdt).filter(|&p| p > 0.0).sum();
    let gross_loss:   f64 = trades.iter().filter_map(|t| t.net_pnl_usdt).filter(|&p| p < 0.0).map(|p| p.abs()).sum();
    let profit_factor = if gross_loss > 0.0 { gross_profit / gross_loss } else { f64::INFINITY };

    let mean_ret = returns_pct.iter().sum::<f64>() / returns_pct.len() as f64;
    let var_ret  = returns_pct.iter().map(|r| (r - mean_ret).powi(2)).sum::<f64>() / returns_pct.len() as f64;
    let std_ret  = var_ret.sqrt();
    // Annualise with √252 (trade-based estimate, not calendar-based).
    let sharpe = if std_ret > 1e-12 { mean_ret / std_ret * (252.0_f64).sqrt() } else { 0.0 };

    let avg_dur = {
        let durs: Vec<u64> = trades.iter().filter_map(|t| t.duration_ms).collect();
        if durs.is_empty() { 0 } else { durs.iter().sum::<u64>() / durs.len() as u64 }
    };

    BacktestMetrics {
        final_equity,
        total_return_pct: (final_equity / starting.max(1e-12) - 1.0) * 100.0,
        max_drawdown_pct: max_dd,
        sharpe_ratio: sharpe,
        profit_factor,
        win_rate,
        trade_count: returns_pct.len(),
        winning_trades: winners.len(),
        losing_trades: losers.len(),
        avg_winner_pct: avg_winner,
        avg_loser_pct: avg_loser,
        avg_trade_duration_ms: avg_dur,
        total_fees_usdt: total_fees,
        total_slippage_usdt: total_slippage,
    }
}

fn compute_score_buckets(trades: &[TradeRecord]) -> Vec<ScoreBucket> {
    let bucket_edges = [0.0_f64, 0.2, 0.4, 0.6, 0.8, 1.001];

    struct Acc { returns: Vec<f64>, gross_profit: f64, gross_loss: f64 }
    let mut accs: Vec<Acc> = (0..bucket_edges.len() - 1)
        .map(|_| Acc { returns: Vec::new(), gross_profit: 0.0, gross_loss: 0.0 })
        .collect();

    for t in trades {
        let net = match t.net_pnl_usdt { Some(n) => n, None => continue };
        if t.position_value_usdt <= 0.0 { continue; }
        let ret_pct = net / t.position_value_usdt * 100.0;
        let idx = bucket_edges
            .windows(2)
            .position(|w| t.score >= w[0] && t.score < w[1])
            .unwrap_or(accs.len() - 1);
        accs[idx].returns.push(ret_pct);
        if net > 0.0 { accs[idx].gross_profit += net; } else { accs[idx].gross_loss += net.abs(); }
    }

    bucket_edges
        .windows(2)
        .enumerate()
        .map(|(i, w)| {
            let acc = &accs[i];
            let n = acc.returns.len();
            let label = format!("{:.1}–{:.1}", w[0], w[1].min(1.0));
            if n == 0 {
                return ScoreBucket {
                    label, range_low: w[0], range_high: w[1].min(1.0),
                    count: 0, wins: 0, win_rate: 0.0, avg_return_pct: 0.0,
                    expectancy_pct: 0.0, profit_factor: 0.0,
                };
            }
            let wins: Vec<f64> = acc.returns.iter().copied().filter(|r| *r > 0.0).collect();
            let losses: Vec<f64> = acc.returns.iter().copied().filter(|r| *r <= 0.0).collect();
            let win_rate = wins.len() as f64 / n as f64;
            let avg_w = if wins.is_empty() { 0.0 } else { wins.iter().sum::<f64>() / wins.len() as f64 };
            let avg_l = if losses.is_empty() { 0.0 } else { losses.iter().sum::<f64>() / losses.len() as f64 };
            ScoreBucket {
                label, range_low: w[0], range_high: w[1].min(1.0),
                count: n, wins: wins.len(), win_rate,
                avg_return_pct: acc.returns.iter().sum::<f64>() / n as f64,
                expectancy_pct: win_rate * avg_w + (1.0 - win_rate) * avg_l,
                profit_factor: if acc.gross_loss > 0.0 { acc.gross_profit / acc.gross_loss } else { f64::INFINITY },
            }
        })
        .collect()
}

fn compute_regime_stats(trades: &[TradeRecord]) -> Vec<RegimeStats> {
    let mut map: HashMap<String, (Vec<f64>, f64, f64)> = HashMap::new();

    for t in trades {
        let net = match t.net_pnl_usdt { Some(n) => n, None => continue };
        if t.position_value_usdt <= 0.0 { continue; }
        let ret_pct = net / t.position_value_usdt * 100.0;
        let e = map.entry(t.regime.clone()).or_insert_with(|| (Vec::new(), 0.0, 0.0));
        e.0.push(ret_pct);
        if net > 0.0 { e.1 += net; } else { e.2 += net.abs(); }
    }

    let mut stats: Vec<RegimeStats> = map
        .into_iter()
        .map(|(regime, (returns, gp, gl))| {
            let n = returns.len();
            let wins = returns.iter().filter(|&&r| r > 0.0).count();
            let win_rate = wins as f64 / n.max(1) as f64;
            let avg_ret = returns.iter().sum::<f64>() / n.max(1) as f64;
            let avg_w = { let w: Vec<f64> = returns.iter().copied().filter(|r| *r > 0.0).collect(); if w.is_empty() { 0.0 } else { w.iter().sum::<f64>() / w.len() as f64 } };
            let avg_l = { let l: Vec<f64> = returns.iter().copied().filter(|r| *r <= 0.0).collect(); if l.is_empty() { 0.0 } else { l.iter().sum::<f64>() / l.len() as f64 } };
            RegimeStats {
                regime,
                trades: n,
                wins,
                win_rate,
                avg_return_pct: avg_ret,
                expectancy_pct: win_rate * avg_w + (1.0 - win_rate) * avg_l,
                profit_factor: if gl > 0.0 { gp / gl } else { f64::INFINITY },
            }
        })
        .collect();
    stats.sort_by(|a, b| a.regime.cmp(&b.regime));
    stats
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Run a full backtest on cached candle data under `data_dir`.
pub fn simulate_from_cache(symbol: &str, data_dir: &str) -> Result<BacktestReport> {
    simulate_from_cache_with_config(symbol, data_dir, BacktestConfig::default())
}

pub fn simulate_from_cache_with_config(
    symbol: &str,
    data_dir: &str,
    config: BacktestConfig,
) -> Result<BacktestReport> {
    let candles_1m = load_cached(data_dir, symbol, Interval::OneMinute)?;
    let candles_5m = load_cached(data_dir, symbol, Interval::FiveMinutes)?;
    let candles_1h = load_cached(data_dir, symbol, Interval::OneHour)?;

    if candles_1m.len() < 120 || candles_5m.len() < 120 || candles_1h.len() < 60 {
        return Err(anyhow!("Not enough cached candles to simulate"));
    }

    Ok(simulate(&candles_1m, &candles_5m, &candles_1h, &config))
}

/// Run a backtest restricted to a chronological slice of the cached data, expressed as
/// fractions of the full 5m series (e.g. `start_frac=0.0, end_frac=0.75` = the earliest
/// 75%). Intended for a research/holdout split: explore and iterate against one slice,
/// then confirm a finalized change once against a slice never used during exploration —
/// without needing a second fetch or a second on-disk dataset.
///
/// 1m/1h context is padded ~200 candles before the slice start (mirroring `simulate`'s
/// own per-bar lookback) so the first bars of the slice aren't starved of the trailing
/// history `compute_features` needs (`walk_forward`'s per-window slicing doesn't do this
/// padding, which costs each window a multi-day warm-up gap at its start — acceptable
/// there given windows are short and unmeasured train time absorbs it, but worth doing
/// properly here since a holdout slice is precious and shouldn't lose real data to it).
pub fn simulate_from_cache_range(
    symbol: &str,
    data_dir: &str,
    start_frac: f64,
    end_frac: f64,
    config: BacktestConfig,
) -> Result<BacktestReport> {
    let candles_1m = load_cached(data_dir, symbol, Interval::OneMinute)?;
    let candles_5m = load_cached(data_dir, symbol, Interval::FiveMinutes)?;
    let candles_1h = load_cached(data_dir, symbol, Interval::OneHour)?;

    if candles_1m.len() < 120 || candles_5m.len() < 120 || candles_1h.len() < 60 {
        return Err(anyhow!("Not enough cached candles to simulate"));
    }

    let start_frac = start_frac.clamp(0.0, 1.0);
    let end_frac = end_frac.clamp(0.0, 1.0);
    if end_frac <= start_frac {
        return Err(anyhow!("end_frac ({end_frac}) must be > start_frac ({start_frac})"));
    }

    let total = candles_5m.len();
    let start_idx = ((total as f64) * start_frac).floor() as usize;
    let end_idx = (((total as f64) * end_frac).ceil() as usize).min(total);
    if end_idx.saturating_sub(start_idx) < 120 {
        return Err(anyhow!(
            "Selected range too small ({} candles). Widen start/end fractions.",
            end_idx.saturating_sub(start_idx)
        ));
    }

    let w5 = &candles_5m[start_idx..end_idx];
    let range_start_time = w5.first().unwrap().open_time;

    let h1_pos = candles_1h
        .iter()
        .position(|c| c.open_time >= range_start_time)
        .unwrap_or(candles_1h.len());
    let w1h = &candles_1h[h1_pos.saturating_sub(210)..];

    let m1_pos = candles_1m
        .iter()
        .position(|c| c.open_time >= range_start_time)
        .unwrap_or(candles_1m.len());
    let w1m = &candles_1m[m1_pos.saturating_sub(210)..];

    Ok(simulate(w1m, w5, w1h, &config))
}

/// Rolling walk-forward validation.
///
/// Splits the 5m candle series into `n_windows` equal windows.  The last
/// `test_fraction` of each window is the out-of-sample test period.  Returns
/// one `BacktestReport` per test window, filtered to only trades that entered
/// within that test period.
pub fn walk_forward(
    symbol: &str,
    data_dir: &str,
    n_windows: usize,
    test_fraction: f64,
) -> Result<Vec<(String, BacktestReport)>> {
    walk_forward_with_config(symbol, data_dir, n_windows, test_fraction, BacktestConfig::default())
}

pub fn walk_forward_with_config(
    symbol: &str,
    data_dir: &str,
    n_windows: usize,
    test_fraction: f64,
    config: BacktestConfig,
) -> Result<Vec<(String, BacktestReport)>> {
    let candles_1m = load_cached(data_dir, symbol, Interval::OneMinute)?;
    let candles_5m = load_cached(data_dir, symbol, Interval::FiveMinutes)?;
    let candles_1h = load_cached(data_dir, symbol, Interval::OneHour)?;
    walk_forward_over(&candles_1m, &candles_5m, &candles_1h, n_windows, test_fraction, config)
}

/// Same as `walk_forward_with_config`, restricted to a chronological slice of the
/// cached data (see `simulate_from_cache_range` for the research/holdout rationale).
/// Windows are computed within the slice only, so a research-slice walk-forward never
/// draws windows from data reserved for holdout.
pub fn walk_forward_range_with_config(
    symbol: &str,
    data_dir: &str,
    start_frac: f64,
    end_frac: f64,
    n_windows: usize,
    test_fraction: f64,
    config: BacktestConfig,
) -> Result<Vec<(String, BacktestReport)>> {
    let candles_1m = load_cached(data_dir, symbol, Interval::OneMinute)?;
    let candles_5m = load_cached(data_dir, symbol, Interval::FiveMinutes)?;
    let candles_1h = load_cached(data_dir, symbol, Interval::OneHour)?;

    let start_frac = start_frac.clamp(0.0, 1.0);
    let end_frac = end_frac.clamp(0.0, 1.0);
    if end_frac <= start_frac {
        return Err(anyhow!("end_frac ({end_frac}) must be > start_frac ({start_frac})"));
    }

    let total = candles_5m.len();
    let start_idx = ((total as f64) * start_frac).floor() as usize;
    let end_idx = (((total as f64) * end_frac).ceil() as usize).min(total);
    if end_idx.saturating_sub(start_idx) < 240 {
        return Err(anyhow!(
            "Selected range too small ({} candles) for walk-forward. Widen start/end fractions.",
            end_idx.saturating_sub(start_idx)
        ));
    }

    let w5 = &candles_5m[start_idx..end_idx];
    let range_start_time = w5.first().unwrap().open_time;

    let h1_pos = candles_1h
        .iter()
        .position(|c| c.open_time >= range_start_time)
        .unwrap_or(candles_1h.len());
    let w1h = &candles_1h[h1_pos.saturating_sub(210)..];

    let m1_pos = candles_1m
        .iter()
        .position(|c| c.open_time >= range_start_time)
        .unwrap_or(candles_1m.len());
    let w1m = &candles_1m[m1_pos.saturating_sub(210)..];

    walk_forward_over(w1m, w5, w1h, n_windows, test_fraction, config)
}

fn walk_forward_over(
    candles_1m: &[Candle],
    candles_5m: &[Candle],
    candles_1h: &[Candle],
    n_windows: usize,
    test_fraction: f64,
    config: BacktestConfig,
) -> Result<Vec<(String, BacktestReport)>> {
    if n_windows == 0 {
        return Err(anyhow!("n_windows must be > 0"));
    }
    let test_frac = test_fraction.clamp(0.05, 0.50);

    if candles_5m.len() < 240 {
        return Err(anyhow!("Need at least 240 5m candles for walk-forward"));
    }

    let total = candles_5m.len();
    let window_size = total / n_windows;
    if window_size < 120 {
        return Err(anyhow!("Windows too small ({window_size}). Reduce n_windows."));
    }

    let mut results = Vec::new();

    for w in 0..n_windows {
        let win_start = w * window_size;
        let win_end = if w + 1 < n_windows { (w + 1) * window_size } else { total };
        let test_start = win_start + ((win_end - win_start) as f64 * (1.0 - test_frac)) as usize;

        let w5 = &candles_5m[win_start..win_end];
        let test_start_ms = candles_5m[test_start].open_time.max(0) as u64;

        let h1_start = candles_1h.iter().position(|c| c.open_time >= candles_5m[win_start].open_time).unwrap_or(0);
        let m1_start = candles_1m.iter().position(|c| c.open_time >= candles_5m[win_start].open_time).unwrap_or(0);
        let w1h = &candles_1h[h1_start..];
        let w1m = &candles_1m[m1_start..];

        if w5.len() < 120 || w1h.is_empty() || w1m.is_empty() {
            continue;
        }

        let mut report = simulate(w1m, w5, w1h, &config);

        // Keep only trades from the test slice to avoid look-ahead contamination.
        report.trades.retain(|t| t.entry_time_ms >= test_start_ms);
        report.score_buckets = compute_score_buckets(&report.trades);
        report.regime_stats = compute_regime_stats(&report.trades);
        // Recompute metrics on the filtered set (equity/drawdown curves remain from full run).
        report.metrics = compute_metrics(
            config.starting_capital,
            report.metrics.final_equity,
            &report.trades,
            &report.drawdown_curve,
            report.metrics.total_fees_usdt,
            report.metrics.total_slippage_usdt,
        );

        results.push((format!("window_{w}_test_from_{test_start_ms}"), report));
    }

    Ok(results)
}
