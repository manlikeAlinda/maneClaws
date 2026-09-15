//! Structured trade-level telemetry.
//!
//! Every signal decision and trade lifecycle event can be persisted to a
//! newline-delimited JSON (JSONL) file by setting the `BOT_TELEMETRY_PATH`
//! environment variable.  The file is append-only and safe to tail in real-time.
//!
//! The captured data enables:
//! - Score calibration (bucket analysis)
//! - Regime-aware performance evaluation
//! - Full trade reproduction (entry state + feature snapshot)

use crate::features::Features;
use crate::regime::RegimeResult;
use serde::{Deserialize, Serialize};
use std::io::Write;

// ---------------------------------------------------------------------------
// Feature snapshot
// ---------------------------------------------------------------------------

/// Exact feature vector used by the decision engine at signal time.
/// Storing this ensures every decision is reproducible from the log alone.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeatureSnapshot {
    pub ema20_5m: f64,
    pub ema50_5m: f64,
    pub ema20_1h: f64,
    pub ema50_1h: f64,
    pub ema200_1h: f64,
    pub atr14_5m: f64,
    pub atr_ratio_5m: f64,
    pub bb_mid20_5m: f64,
    pub bb_width_5m: f64,
    pub rsi14_5m: f64,
    pub rsi14_1m: f64,
    pub volume_z: f64,
    pub rv_short: f64,
    pub rv_long: f64,
    pub velocity_1m: f64,
    pub velocity_5m: f64,
    /// Computed trend strength from regime detection (|EMA20-EMA50|/ATR).
    pub trend_strength: f64,
    /// Short/long realized-vol ratio from regime detection.
    pub vol_ratio: f64,
    pub htf_bullish: bool,
    pub bearish_bias: bool,
    pub vol_squeeze: bool,
}

impl FeatureSnapshot {
    pub fn from_features_and_regime(f: &Features, r: &RegimeResult) -> Self {
        Self {
            ema20_5m: f.ema20_5m,
            ema50_5m: f.ema50_5m,
            ema20_1h: f.ema20_1h,
            ema50_1h: f.ema50_1h,
            ema200_1h: f.ema200_1h,
            atr14_5m: f.atr14_5m,
            atr_ratio_5m: f.atr_ratio_5m,
            bb_mid20_5m: f.bb_mid20_5m,
            bb_width_5m: f.bb_width_5m,
            rsi14_5m: f.rsi14_5m,
            rsi14_1m: f.rsi14_1m,
            volume_z: f.volume_z,
            rv_short: f.rv_short,
            rv_long: f.rv_long,
            velocity_1m: f.velocity_1m,
            velocity_5m: f.velocity_5m,
            trend_strength: r.trend_strength,
            vol_ratio: r.vol_ratio,
            htf_bullish: r.htf_bullish,
            bearish_bias: r.bearish_bias,
            vol_squeeze: r.vol_squeeze,
        }
    }
}

// ---------------------------------------------------------------------------
// Trade record
// ---------------------------------------------------------------------------

/// Complete lifecycle record for a single trade, from entry signal through exit.
///
/// Fields are `Option` for the exit side because they are filled in when the
/// position closes.  An open (unfilled) record can be emitted immediately on
/// entry so the file always reflects the current state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeRecord {
    /// Unique identifier — `{entry_time_ms}_{symbol}`.
    pub trade_id: String,
    pub symbol: String,
    /// Regime detected at entry time (e.g. "Trending", "Ranging").
    pub regime: String,
    /// Strategy that produced the entry signal.
    pub strategy: String,
    /// Final scored confidence (confidence × context multiplier).
    pub score: f64,

    // --- Entry side ---
    pub entry_time_ms: u64,
    pub entry_price: f64,
    pub qty: f64,
    pub position_value_usdt: f64,
    pub stop_price: f64,
    pub tp_price: Option<f64>,

    // --- Exit side (filled on close) ---
    pub exit_time_ms: Option<u64>,
    pub exit_price: Option<f64>,
    pub exit_reason: Option<String>,

    // --- PnL accounting ---
    pub gross_pnl_usdt: Option<f64>,
    pub entry_fee_usdt: Option<f64>,
    pub exit_fee_usdt: Option<f64>,
    pub net_pnl_usdt: Option<f64>,
    /// Approximate slippage cost estimate (entry + exit).
    pub slippage_usdt: Option<f64>,

    // --- Excursion metrics (populated by backtest; None in live mode) ---
    /// Max favourable excursion: peak unrealised gain during the trade (USDT).
    pub mfe_usdt: Option<f64>,
    /// Max adverse excursion: worst unrealised drawdown during the trade (USDT).
    pub mae_usdt: Option<f64>,

    pub duration_ms: Option<u64>,

    /// Feature state at the moment of the entry decision.
    pub features: FeatureSnapshot,
}

impl TradeRecord {
    /// Create an open (entry-only) record.
    #[allow(clippy::too_many_arguments)]
    pub fn open(
        symbol: &str,
        regime: &str,
        strategy: &str,
        score: f64,
        entry_time_ms: u64,
        entry_price: f64,
        qty: f64,
        stop_price: f64,
        tp_price: Option<f64>,
        entry_fee_usdt: f64,
        features: FeatureSnapshot,
    ) -> Self {
        let trade_id = format!("{entry_time_ms}_{symbol}");
        Self {
            trade_id,
            symbol: symbol.to_string(),
            regime: regime.to_string(),
            strategy: strategy.to_string(),
            score,
            entry_time_ms,
            entry_price,
            qty,
            position_value_usdt: entry_price * qty,
            stop_price,
            tp_price,
            exit_time_ms: None,
            exit_price: None,
            exit_reason: None,
            gross_pnl_usdt: None,
            entry_fee_usdt: Some(entry_fee_usdt),
            exit_fee_usdt: None,
            net_pnl_usdt: None,
            slippage_usdt: None,
            mfe_usdt: None,
            mae_usdt: None,
            duration_ms: None,
            features,
        }
    }

    /// Populate the exit side of the record in place.
    pub fn close(
        &mut self,
        exit_time_ms: u64,
        exit_price: f64,
        exit_reason: &str,
        exit_fee_usdt: f64,
        mfe_usdt: Option<f64>,
        mae_usdt: Option<f64>,
    ) {
        let gross = (exit_price - self.entry_price) * self.qty;
        let entry_fee = self.entry_fee_usdt.unwrap_or(0.0);
        let net = gross - entry_fee - exit_fee_usdt;
        self.exit_time_ms = Some(exit_time_ms);
        self.exit_price = Some(exit_price);
        self.exit_reason = Some(exit_reason.to_string());
        self.gross_pnl_usdt = Some(gross);
        self.exit_fee_usdt = Some(exit_fee_usdt);
        self.net_pnl_usdt = Some(net);
        self.mfe_usdt = mfe_usdt;
        self.mae_usdt = mae_usdt;
        self.duration_ms = Some(exit_time_ms.saturating_sub(self.entry_time_ms));
    }
}

// ---------------------------------------------------------------------------
// JSONL writer
// ---------------------------------------------------------------------------

/// Append-only JSONL telemetry writer.
///
/// Enabled by setting `BOT_TELEMETRY_PATH` to a writable file path.
/// Each call to `emit_trade` appends one JSON object followed by a newline.
/// The file is safe to tail, rotate, or compress independently of the bot process.
pub struct TelemetryWriter {
    path: Option<String>,
}

impl TelemetryWriter {
    pub fn from_env() -> Self {
        let path = std::env::var("BOT_TELEMETRY_PATH")
            .ok()
            .filter(|s| !s.trim().is_empty());
        Self { path }
    }

    /// Emit a complete or partial trade record as a single JSONL line.
    pub fn emit_trade(&self, record: &TradeRecord) {
        let Some(ref path) = self.path else { return };
        let Ok(line) = serde_json::to_string(record) else { return };
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "{line}");
        }
    }

    /// Emit a lightweight trade-closed event for a position that was opened in a previous tick.
    /// Callers that do not have the original `TradeRecord` can use this to record the exit
    /// without duplicating the full entry snapshot.  Downstream analysis correlates on `trade_id`.
    #[allow(clippy::too_many_arguments)]
    pub fn emit_exit(
        &self,
        symbol: &str,
        entry_time_ms: u64,
        entry_price: f64,
        exit_time_ms: u64,
        exit_price: f64,
        exit_reason: &str,
        qty: f64,
    ) {
        let Some(ref path) = self.path else { return };
        let gross_pnl = (exit_price - entry_price) * qty;
        let trade_id = format!("{entry_time_ms}_{symbol}");
        let value = serde_json::json!({
            "event": "trade_closed",
            "trade_id": trade_id,
            "symbol": symbol,
            "entry_time_ms": entry_time_ms,
            "entry_price": entry_price,
            "exit_time_ms": exit_time_ms,
            "exit_price": exit_price,
            "exit_reason": exit_reason,
            "qty": qty,
            "gross_pnl_usdt": gross_pnl,
            "duration_ms": exit_time_ms.saturating_sub(entry_time_ms),
        });
        let Ok(line) = serde_json::to_string(&value) else { return };
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "{line}");
        }
    }
}
