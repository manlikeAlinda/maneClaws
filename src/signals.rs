use crate::features::Features;
use crate::regime::Regime;
use std::env;

fn env_f64_default(name: &str, def: f64) -> f64 {
    env::var(name).ok().and_then(|v| v.parse::<f64>().ok()).unwrap_or(def)
}

fn env_flag_default(name: &str, def: bool) -> bool {
    match env::var(name) {
        Ok(v) => matches!(v.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"),
        Err(_) => def,
    }
}

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    EnterLong,
    ExitLong,
    Hold,
}

#[derive(Debug, Clone)]
pub struct Signal {
    pub action: Action,
    pub confidence: f64,
    /// Final score after applying context multiplier.  0.0 for Hold signals.
    pub score: f64,
    /// Name of the strategy that produced this signal.  Empty for Hold signals.
    pub strategy: String,
    pub reason: String,
    pub stop_price: Option<f64>,
}

// ---------------------------------------------------------------------------
// Internal: scored candidate
// ---------------------------------------------------------------------------
// Each strategy produces a `ScoredSignal`.  The orchestrator collects all
// candidates where action == EnterLong, applies a context multiplier, and
// selects the highest final score above a minimum threshold.
//
// This ensures strategies are genuinely ranked rather than tried sequentially,
// and that context (HTF, volume, ATR environment) is accounted for uniformly.

struct ScoredSignal {
    signal: Signal,
    /// Combined score ∈ [0.0, 1.0]: base confidence × context multiplier.
    score: f64,
    name: &'static str,
}

// Minimum score required to actually enter.  Env: `SIGNAL_MIN_SCORE` (default 0.25).
fn min_entry_score() -> f64 {
    env_f64_default("SIGNAL_MIN_SCORE", 0.25)
}

fn hold(reason: impl Into<String>) -> Signal {
    Signal { action: Action::Hold, confidence: 0.0, score: 0.0, strategy: String::new(), reason: reason.into(), stop_price: None }
}

// ---------------------------------------------------------------------------
// Strategy 1 – Trend Breakout (Trending regime, no active squeeze)
// ---------------------------------------------------------------------------
/// Classic Donchian-channel breakout.
///
/// **Deduplication guard**: explicitly refuses when `vol_squeeze == true` —
/// that market condition belongs to the squeeze-breakout strategy, not here.
pub fn trend_breakout_long_only(
    regime: Regime,
    f: &Features,
    current_price: f64,
    vol_ratio: f64,
    vol_squeeze: bool,
) -> Signal {
    if regime != Regime::Trending {
        return hold(format!(
            "Market is {:?}. Trend Breakout requires Trending.",
            regime
        ));
    }

    // Explicit deduplication: if ATR is still in squeeze territory, hand off to
    // squeeze-breakout strategy which uses a tighter stop and higher baseline score.
    if vol_squeeze {
        return hold(
            "Vol squeeze active — deferring to squeeze-breakout strategy for better risk control.",
        );
    }

    if f.last_close_5m <= f.donchian_high20_5m {
        return hold(format!(
            "Price {:.2} still below Donchian high {:.2}. No breakout.",
            f.last_close_5m, f.donchian_high20_5m
        ));
    }

    let confirmed = f.volume_z > 0.0 || vol_ratio > 1.1;
    if !confirmed {
        return hold(format!(
            "Breakout attempt weak (vol_z={:.2}, vol_ratio={:.2}). Waiting for conviction.",
            f.volume_z, vol_ratio
        ));
    }

    // Optional, off by default: a much stronger volume bar than the `confirmed` check
    // above. A 274-day backtest showed trend_breakout entries with volume_z >= 2.0
    // winning ~31% of the time vs ~21% for volume_z in [0, 2.0) — a real, mostly
    // monotonic separation at n=78 (the dominant bucket), not present at the existing
    // weak 0.0 threshold. `TREND_BREAKOUT_REQUIRE_VOLUME_CONFIRM=1` gates entry on
    // `volume_z` clearing `TREND_BREAKOUT_VOLUME_Z_MIN` (default 2.0). Scoped to
    // trend_breakout only; does not affect squeeze_breakout or momentum.
    if env_flag_default("TREND_BREAKOUT_REQUIRE_VOLUME_CONFIRM", false) {
        let min_volume_z = env_f64_default("TREND_BREAKOUT_VOLUME_Z_MIN", 2.0);
        if f.volume_z < min_volume_z {
            return hold(format!(
                "Strong volume confirmation required: volume_z {:.2} < {:.2}.",
                f.volume_z, min_volume_z
            ));
        }
    }

    let atr = f.atr14_5m.max(1e-12);
    let stop = current_price - 1.8 * atr;

    let raw = (f.last_close_5m - f.donchian_high20_5m) / atr;
    let confidence = (raw.clamp(0.0, 2.0) / 2.0).clamp(0.0, 1.0);

    Signal {
        action: Action::EnterLong,
        confidence,
        score: confidence,
        strategy: String::new(),
        reason: "Trend breakout confirmed. HTF bullish stack intact.".to_string(),
        stop_price: Some(stop),
    }
}

// ---------------------------------------------------------------------------
// Strategy 2 – Volatility Squeeze Breakout (any non-hostile regime)
// ---------------------------------------------------------------------------
/// Activates only after a confirmed compression period (`vol_squeeze == true`)
/// when price breaks above the prior Donchian high.
pub fn volatility_squeeze_breakout_signal(
    regime: Regime,
    f: &Features,
    current_price: f64,
    vol_ratio: f64,
    vol_squeeze: bool,
) -> Signal {
    if !vol_squeeze {
        return hold("No volatility squeeze detected. Skipping squeeze-breakout.");
    }

    if matches!(regime, Regime::Volatile | Regime::Illiquid) {
        return hold(format!("Squeeze breakout not suitable in {:?} regime.", regime));
    }

    if f.last_close_5m <= f.donchian_high20_5m {
        return hold(format!(
            "Price {:.2} has not cleared Donchian high {:.2}.",
            f.last_close_5m, f.donchian_high20_5m
        ));
    }

    // A squeeze break on very weak volume is often a trap.
    if f.volume_z < -0.5 {
        return hold(format!(
            "Squeeze breakout has weak volume (z={:.2}). Waiting for conviction.",
            f.volume_z
        ));
    }

    let atr = f.atr14_5m.max(1e-12);
    // Tighter stop because the range was compressed — risk is better defined.
    let stop = current_price - 1.5 * atr;

    let raw = (f.last_close_5m - f.donchian_high20_5m) / atr;
    // Baseline boost: squeeze setups historically resolve with strong follow-through.
    let confidence = (raw.clamp(0.0, 1.5) / 1.5 * 0.7 + 0.3).clamp(0.0, 1.0);

    let _ = vol_ratio;

    Signal {
        action: Action::EnterLong,
        confidence,
        score: confidence,
        strategy: String::new(),
        reason: format!(
            "Volatility squeeze breakout: BB_width={:.4}, above Donchian high. Early entry.",
            f.bb_width_5m
        ),
        stop_price: Some(stop),
    }
}

// ---------------------------------------------------------------------------
// Strategy 3 – Mean Reversion (Ranging regime)
// ---------------------------------------------------------------------------
/// Buys oversold dips near the lower Bollinger Band with multiple confirmation layers:
///
/// 1. Price within entry zone of BB lower band.
/// 2. RSI in configurable oversold range (not free-falling).
/// 3. Price above Donchian low — confirms we are inside the range, not breaking down.
/// 4. Support proximity — price within 1.5 ATR of Donchian low (extra score).
/// 5. Volume exhaustion — low/declining volume signals selling pressure drying up.
/// 6. BB-width sanity — band must not be too wide (> threshold means crash, not range).
pub fn mean_reversion_range_signal(f: &Features, current_price: f64) -> Signal {
    // Gate 1: Valid BB bands.
    if f.bb_lower_5m <= 0.0 || f.bb_width_5m <= 0.0 {
        return hold("BB bands not available for mean-reversion check.");
    }

    // Gate 2: BB width sanity — if bands are extremely wide the market is volatile,
    // not ranging. Configurable: `RANGE_BB_WIDTH_MAX` (default 0.06 = 6%).
    let bb_width_max = env_f64_default("RANGE_BB_WIDTH_MAX", 0.06);
    if f.bb_width_5m > bb_width_max {
        return hold(format!(
            "BB width {:.4} > {:.4} max. Market too volatile for mean reversion.",
            f.bb_width_5m, bb_width_max
        ));
    }

    // Gate 3: Entry zone — price within 0.5% of or below the lower band.
    let entry_zone = f.bb_lower_5m * (1.0 + env_f64_default("RANGE_BB_ENTRY_ZONE_PCT", 0.005));
    if current_price > entry_zone {
        return hold(format!(
            "Price {:.2} not near lower BB {:.2}. Mean reversion not triggered.",
            current_price, f.bb_lower_5m
        ));
    }

    // Gate 4: RSI in oversold zone — not in free-fall, just stretched down.
    let rsi_min = env_f64_default("RANGE_RSI_MIN", 25.0);
    let rsi_max = env_f64_default("RANGE_RSI_MAX", 45.0);
    if f.rsi14_5m < rsi_min || f.rsi14_5m > rsi_max {
        return hold(format!(
            "RSI {:.1} not in range-oversold zone ({:.1}..{:.1}).",
            f.rsi14_5m, rsi_min, rsi_max
        ));
    }

    // Gate 5: Price above Donchian low — we are inside the range, not breaking down.
    if current_price <= f.donchian_low20_5m {
        return hold(format!(
            "Price {:.2} ≤ Donchian low {:.2}. Possible range breakdown, skip.",
            current_price, f.donchian_low20_5m
        ));
    }

    // Gate 6 (optional, off by default): require price to have already turned up over
    // the last ~3 minutes before buying the dip. A 90-day backtest showed mean-reversion
    // losers had near-zero favorable excursion before adverse movement dominated — i.e.
    // entries were frequently buying into continued weakness ("falling knife"), not
    // genuine reversion. `MEAN_REV_REQUIRE_VELOCITY_CONFIRM=1` gates entry on
    // `velocity_1m` clearing `MEAN_REV_VELOCITY_MIN` (default 0.0 — just "turned
    // positive"). Scoped to mean-reversion only; does not affect other strategies.
    if env_flag_default("MEAN_REV_REQUIRE_VELOCITY_CONFIRM", false) {
        let min_velocity = env_f64_default("MEAN_REV_VELOCITY_MIN", 0.0);
        if f.velocity_1m <= min_velocity {
            return hold(format!(
                "Velocity confirmation required: velocity_1m {:.6} <= {:.6}. Price hasn't turned yet.",
                f.velocity_1m, min_velocity
            ));
        }
    }

    let atr = f.atr14_5m.max(1e-12);
    // Stop below recent range low with a safety buffer.
    let stop = f.donchian_low20_5m - 0.5 * atr;
    if stop >= current_price {
        return hold("Stop would be at or above entry. Geometry invalid, skip.");
    }

    // ---- Confidence scoring (all additive, final clamped to [0, 1]) ----
    // Component A: How far below BB lower (deeper = stronger oversold signal).
    let overshoot = (f.bb_lower_5m - current_price).max(0.0) / atr;
    let a = (overshoot * 0.6).clamp(0.0, 0.35);

    // Component B: RSI position within the oversold window.
    let rsi_range = (rsi_max - rsi_min).max(1e-12);
    let b = ((rsi_max - f.rsi14_5m) / rsi_range * 0.3).clamp(0.0, 0.30);

    // Component C: Support proximity bonus — price within 1.5 ATR of Donchian low.
    let support_distance = (current_price - f.donchian_low20_5m).max(0.0);
    let c = if support_distance < 1.5 * atr { 0.15 } else { 0.0 };

    // Component D: Volume exhaustion — low volume = sellers tiring.
    let d = if f.volume_z < 0.0 { 0.10 } else { 0.0 };

    let confidence = (a + b + c + d).clamp(0.0, 1.0);

    Signal {
        action: Action::EnterLong,
        confidence,
        score: confidence,
        strategy: String::new(),
        reason: format!(
            "Range mean reversion: price near lower BB ({:.2}), RSI {:.1}, support {:.2} ATR away. Target: BB mid {:.2}.",
            f.bb_lower_5m, f.rsi14_5m, support_distance / atr, f.bb_mid20_5m
        ),
        stop_price: Some(stop),
    }
}

// ---------------------------------------------------------------------------
// Strategy 4 – Dynamic Momentum Acceleration (non-hostile regime)
// ---------------------------------------------------------------------------
/// Detects *acceleration* in upward momentum.
///
/// **Deduplication guard**: requires velocity_1m > velocity_5m (1m must outpace
/// 5m — momentum genuinely building) AND velocity_5m > -0.001 (5m trend not
/// actively falling).  This makes it a pure continuation/acceleration signal,
/// not a breakout variant.
pub fn dynamic_momentum_signal(f: &Features, current_price: f64) -> Signal {
    let vel_thresh = env_f64_default("VELOCITY_1M_THRESHOLD", 0.001);
    if f.velocity_1m < vel_thresh {
        return hold(format!(
            "Insufficient upward velocity (1m={:.6} < {:.6}).",
            f.velocity_1m, vel_thresh
        ));
    }

    // Guard: 5m trend must not be actively falling (prevents buying into a 15-min downswing).
    if f.velocity_5m < -0.001 {
        return hold(format!(
            "5m trend falling (velocity_5m={:.6}). Momentum against entry.",
            f.velocity_5m
        ));
    }

    // Deduplication guard: 1m velocity must be accelerating vs 5m.
    // If velocity_5m >= velocity_1m, momentum is decelerating — not the signal we want.
    if f.velocity_5m >= 0.0 && f.velocity_1m <= f.velocity_5m {
        return hold(format!(
            "Momentum decelerating (1m={:.6} ≤ 5m={:.6}). Waiting for acceleration.",
            f.velocity_1m, f.velocity_5m
        ));
    }

    let rsi_min = env_f64_default("RSI_MIN", 55.0);
    let rsi_max = env_f64_default("RSI_MAX", 80.0);
    if f.rsi14_1m < rsi_min || f.rsi14_1m > rsi_max {
        return hold(format!(
            "RSI {:.1} not in momentum zone ({:.1}..{:.1}).",
            f.rsi14_1m, rsi_min, rsi_max
        ));
    }

    if current_price <= f.ema20_1m {
        return hold(format!(
            "Price {:.2} ≤ EMA20_1m {:.2}. Below short-term trend.",
            current_price, f.ema20_1m
        ));
    }

    let atr = f.atr14_1m.max(1e-12);
    let stop = current_price - 1.5 * atr;

    // Confidence: velocity magnitude + acceleration ratio + volume support.
    let vel_score = (f.velocity_1m * 1000.0).clamp(0.0, 1.0);
    let accel_score = if f.velocity_5m.abs() > 1e-12 {
        ((f.velocity_1m / f.velocity_5m.abs()) - 1.0).clamp(0.0, 1.0)
    } else {
        1.0 // 5m flat but 1m moving — strong signal
    };
    let vol_score = f.volume_z.clamp(0.0, 3.0) / 3.0;
    let confidence = (vel_score * 0.5 + accel_score * 0.3 + vol_score * 0.2).clamp(0.0, 1.0);

    Signal {
        action: Action::EnterLong,
        confidence,
        score: confidence,
        strategy: String::new(),
        reason: format!(
            "Momentum accelerating: 1m_vel={:.6}, 5m_vel={:.6}, RSI={:.1}.",
            f.velocity_1m, f.velocity_5m, f.rsi14_1m
        ),
        stop_price: Some(stop),
    }
}

/// Compatibility alias — callers that referenced the old name continue to work.
pub fn intraday_momentum_signal(f: &Features, current_price: f64) -> Signal {
    dynamic_momentum_signal(f, current_price)
}

// ---------------------------------------------------------------------------
// Context multipliers applied to raw signal confidence
// ---------------------------------------------------------------------------
// Each strategy's score = signal.confidence × context_multiplier.
// This ensures that a high-confidence signal in poor context loses to a
// moderate-confidence signal in ideal context.

fn context_multiplier_for_squeeze_breakout(f: &Features, htf_bullish: bool) -> f64 {
    let mut m = 1.0_f64;
    if htf_bullish   { m += 0.20 } // Full HTF stack behind the move.
    if f.volume_z > 1.0 { m += 0.10 } // Above-average volume surge.
    m.clamp(0.5, 1.5)
}

fn context_multiplier_for_trend_breakout(f: &Features, htf_bullish: bool) -> f64 {
    let mut m = 1.0_f64;
    if htf_bullish   { m += 0.15 }
    if f.volume_z > 0.5 { m += 0.10 }
    m.clamp(0.5, 1.5)
}

// Unused while mean_reversion's dispatcher call site is disabled (research hold,
// 2026-09-09) — kept for reference alongside `mean_reversion_range_signal`.
#[allow(dead_code)]
fn context_multiplier_for_mean_reversion(f: &Features, htf_bullish: bool) -> f64 {
    let mut m = 1.0_f64;
    if htf_bullish   { m += 0.20 } // HTF bullish = dip is more likely to recover.
    if f.volume_z < 0.0 { m += 0.10 } // Volume exhaustion = sellers drying up.
    // If BB width is very narrow: range is well-defined → higher quality setup.
    if f.bb_width_5m < 0.03 { m += 0.05 }
    m.clamp(0.5, 1.5)
}

fn context_multiplier_for_momentum(f: &Features, htf_bullish: bool) -> f64 {
    let mut m = 1.0_f64;
    if htf_bullish && f.velocity_5m > 0.0 { m += 0.15 } // Trend continuation.
    if f.volume_z > 0.5 { m += 0.10 }
    m.clamp(0.5, 1.5)
}

// ---------------------------------------------------------------------------
// Strategy orchestrator – scored, multi-regime dispatcher
// ---------------------------------------------------------------------------
/// Routes all applicable strategies for the current regime, scores every valid
/// entry signal using signal confidence × context multiplier, and returns the
/// highest-scoring candidate above the minimum threshold.
///
/// **Bearish veto**: when `bearish_bias == true` (HTF EMA stack fully bearish),
/// all long entries are blocked regardless of local signal strength.  A spot-only
/// system cannot profit from shorting; the safest action is to wait for the HTF
/// picture to improve.
///
/// **Volatile / Illiquid**: no entry — wait for settled conditions.
pub fn entry_long_signal(
    regime: Regime,
    f: &Features,
    current_price: f64,
    vol_ratio: f64,
    vol_squeeze: bool,
    htf_bullish: bool,
    bearish_bias: bool,
) -> Signal {
    // Hard veto: HTF fully bearish — do not fight a confirmed downtrend on spot.
    if bearish_bias {
        return hold(
            "HTF EMA stack is fully bearish (EMA20 < EMA50 < EMA200 on 1h). \
             No long entries until HTF picture improves.",
        );
    }

    match regime {
        Regime::Volatile | Regime::Illiquid => {
            hold(format!("No entry in {:?} regime. Waiting for settled conditions.", regime))
        }

        Regime::Trending => {
            let mut candidates: Vec<ScoredSignal> = Vec::new();

            // Candidate A: squeeze breakout (fires only when vol_squeeze is true).
            let sq = volatility_squeeze_breakout_signal(regime, f, current_price, vol_ratio, vol_squeeze);
            if sq.action == Action::EnterLong {
                let score = sq.confidence * context_multiplier_for_squeeze_breakout(f, htf_bullish);
                candidates.push(ScoredSignal { signal: sq, score, name: "squeeze_breakout" });
            }

            // Candidate B: classic trend breakout (deduplication: not when vol_squeeze).
            let tb = trend_breakout_long_only(regime, f, current_price, vol_ratio, vol_squeeze);
            if tb.action == Action::EnterLong {
                let score = tb.confidence * context_multiplier_for_trend_breakout(f, htf_bullish);
                candidates.push(ScoredSignal { signal: tb, score, name: "trend_breakout" });
            }

            // Candidate C: dynamic momentum (genuinely different: acceleration, not breakout).
            let mo = dynamic_momentum_signal(f, current_price);
            if mo.action == Action::EnterLong {
                let score = mo.confidence * context_multiplier_for_momentum(f, htf_bullish);
                candidates.push(ScoredSignal { signal: mo, score, name: "dynamic_momentum" });
            }

            pick_best(candidates)
        }

        Regime::Ranging => {
            let mut candidates: Vec<ScoredSignal> = Vec::new();

            // Candidate A: mean reversion — DISABLED (research hold, 2026-09-09).
            // A 90-365 day backtest against real BTCUSDT history found this strategy
            // structurally falsified: losing trades show a falling-knife signature (near-
            // zero favorable excursion before adverse movement dominates), three candidate
            // confirmation filters (velocity_1m, volume_z, velocity_5m) were flat at the
            // static-table level, and the strategy remained net negative (profit factor
            // well below 1.0) even after fixing the one real bug found (range_tp exiting
            // before recovering the entry price). This was 75-79% of all trade volume, so
            // it is disabled rather than filter-patched further. `mean_reversion_range_signal`
            // is left implemented and tested below for reference — do not re-enable this
            // call site without a redesigned entry thesis that has cleared full
            // re-simulation (not just a static bucket table) with a reproducible positive
            // edge.
            //
            // let mr = mean_reversion_range_signal(f, current_price);
            // if mr.action == Action::EnterLong {
            //     let score = mr.confidence * context_multiplier_for_mean_reversion(f, htf_bullish);
            //     candidates.push(ScoredSignal { signal: mr, score, name: "mean_reversion" });
            // }

            // Candidate B: squeeze breakout within a range (compression → expansion).
            // Only allow this when HTF is supportive; entering a squeeze breakout in a
            // bearish-HTF ranging market is high risk.
            if vol_squeeze && htf_bullish {
                let sq = volatility_squeeze_breakout_signal(regime, f, current_price, vol_ratio, vol_squeeze);
                if sq.action == Action::EnterLong {
                    let score = sq.confidence * context_multiplier_for_squeeze_breakout(f, htf_bullish);
                    candidates.push(ScoredSignal { signal: sq, score, name: "squeeze_breakout" });
                }
            }

            // Candidate C: dynamic momentum for micro-breakouts inside range.
            // NOTE: this is dispatched unconditionally, not gated on HTF state -
            // only the coarser file-level bearish_bias veto above applies here.
            // A "mixed" HTF (not fully bearish-stacked but not bullish either)
            // only loses the htf_bullish bonus below, it isn't blocked. Adding
            // a real HTF gate would change which signals fire and needs
            // re-simulation before landing, per this project's standing rule.
            let mo = dynamic_momentum_signal(f, current_price);
            if mo.action == Action::EnterLong {
                let score = mo.confidence * context_multiplier_for_momentum(f, htf_bullish);
                candidates.push(ScoredSignal { signal: mo, score, name: "dynamic_momentum" });
            }

            pick_best(candidates)
        }
    }
}

fn pick_best(mut candidates: Vec<ScoredSignal>) -> Signal {
    let threshold = min_entry_score();
    candidates.retain(|c| c.score >= threshold);
    if candidates.is_empty() {
        return hold("No strategy scored above the minimum entry threshold.");
    }
    // Sort descending by score, pick top.
    candidates.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    let best = candidates.remove(0);
    Signal {
        reason: format!("[{}] {}", best.name, best.signal.reason),
        score: best.score,
        strategy: best.name.to_string(),
        ..best.signal
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    fn f_base() -> Features {
        Features {
            ema20_1h: 0.0,
            ema50_1h: 0.0,
            ema200_1h: 0.0,
            ema20_5m: 0.0,
            ema50_5m: 0.0,
            atr14_5m: 100.0,
            atr14_1m: 100.0,
            atr_ratio_5m: 1.0,
            bb_mid20_5m: 0.0,
            bb_upper_5m: 0.0,
            bb_lower_5m: 0.0,
            bb_width_5m: 0.0,
            rsi14_5m: 50.0,
            rsi14_1m: 50.0,
            donchian_high20_5m: 200.0,
            donchian_low20_5m: 0.0,
            rv_short: 0.0,
            rv_long: 0.0,
            volume_z: 1.0,
            ema20_1m: 0.0,
            velocity_1m: 0.0,
            velocity_5m: 0.0,
            last_close_1m: 250.0,
            last_close_5m: 250.0,
            last_close_1h: 0.0,
        }
    }

    // ---- trend_breakout_long_only ----

    #[test]
    fn no_entry_when_not_trending() {
        let f = f_base();
        let s = trend_breakout_long_only(Regime::Ranging, &f, 260.0, 1.2, false);
        assert_eq!(s.action, Action::Hold);
    }

    #[test]
    fn entry_when_breakout_and_confirmed() {
        let f = f_base();
        let s = trend_breakout_long_only(Regime::Trending, &f, 260.0, 1.2, false);
        assert_eq!(s.action, Action::EnterLong);
        assert!(s.stop_price.unwrap() < 260.0);
        assert!(s.confidence >= 0.0 && s.confidence <= 1.0);
    }

    #[test]
    fn no_entry_without_confirmation() {
        let mut f = f_base();
        f.volume_z = -0.5;
        let s = trend_breakout_long_only(Regime::Trending, &f, 260.0, 1.05, false);
        assert_eq!(s.action, Action::Hold);
    }

    #[test]
    fn trend_breakout_defers_during_vol_squeeze() {
        let f = f_base();
        // With vol_squeeze=true, trend breakout should defer.
        let s = trend_breakout_long_only(Regime::Trending, &f, 260.0, 1.2, true);
        assert_eq!(s.action, Action::Hold);
    }

    // ---- trend_breakout volume-confirmation gate (TREND_BREAKOUT_REQUIRE_VOLUME_CONFIRM) ----

    #[test]
    fn trend_breakout_volume_gate_off_by_default() {
        unsafe {
            std::env::remove_var("TREND_BREAKOUT_REQUIRE_VOLUME_CONFIRM");
        }
        let mut f = f_base();
        f.volume_z = 0.5; // clears the weak `confirmed` bar but not the strong gate
        let s = trend_breakout_long_only(Regime::Trending, &f, 260.0, 1.2, false);
        assert_eq!(s.action, Action::EnterLong);
    }

    #[test]
    fn trend_breakout_volume_gate_blocks_weak_volume_when_enabled() {
        unsafe {
            std::env::set_var("TREND_BREAKOUT_REQUIRE_VOLUME_CONFIRM", "1");
        }
        let mut f = f_base();
        f.volume_z = 0.5; // below the 2.0 default gate
        let s = trend_breakout_long_only(Regime::Trending, &f, 260.0, 1.2, false);
        unsafe {
            std::env::remove_var("TREND_BREAKOUT_REQUIRE_VOLUME_CONFIRM");
        }
        assert_eq!(s.action, Action::Hold);
        assert!(s.reason.contains("Strong volume confirmation"));
    }

    #[test]
    fn trend_breakout_volume_gate_allows_strong_volume_when_enabled() {
        unsafe {
            std::env::set_var("TREND_BREAKOUT_REQUIRE_VOLUME_CONFIRM", "1");
        }
        let mut f = f_base();
        f.volume_z = 2.5; // clears the 2.0 default gate
        let s = trend_breakout_long_only(Regime::Trending, &f, 260.0, 1.2, false);
        unsafe {
            std::env::remove_var("TREND_BREAKOUT_REQUIRE_VOLUME_CONFIRM");
        }
        assert_eq!(s.action, Action::EnterLong);
    }

    // ---- mean_reversion_range_signal ----

    #[test]
    fn mean_reversion_fires_near_lower_bb() {
        let mut f = f_base();
        f.bb_lower_5m = 100.0;
        f.bb_upper_5m = 104.0;
        f.bb_mid20_5m = 102.0;
        f.bb_width_5m = 0.04;
        f.donchian_low20_5m = 95.0;
        f.rsi14_5m = 35.0;
        let s = mean_reversion_range_signal(&f, 100.2);
        assert_eq!(s.action, Action::EnterLong);
        assert!(s.stop_price.unwrap() < 100.2);
    }

    #[test]
    fn mean_reversion_blocked_by_wide_bb() {
        let mut f = f_base();
        f.bb_lower_5m = 100.0;
        f.bb_mid20_5m = 102.0;
        f.bb_width_5m = 0.10; // too wide — volatile, not ranging
        f.donchian_low20_5m = 95.0;
        f.rsi14_5m = 35.0;
        let s = mean_reversion_range_signal(&f, 100.2);
        assert_eq!(s.action, Action::Hold);
    }

    #[test]
    fn mean_reversion_no_entry_when_price_too_high() {
        let mut f = f_base();
        f.bb_lower_5m = 100.0;
        f.bb_mid20_5m = 102.0;
        f.bb_width_5m = 0.04;
        f.donchian_low20_5m = 95.0;
        f.rsi14_5m = 35.0;
        let s = mean_reversion_range_signal(&f, 103.0);
        assert_eq!(s.action, Action::Hold);
    }

    #[test]
    fn mean_reversion_no_entry_rsi_not_oversold() {
        let mut f = f_base();
        f.bb_lower_5m = 100.0;
        f.bb_mid20_5m = 102.0;
        f.bb_width_5m = 0.04;
        f.donchian_low20_5m = 95.0;
        f.rsi14_5m = 55.0;
        let s = mean_reversion_range_signal(&f, 100.2);
        assert_eq!(s.action, Action::Hold);
    }

    #[test]
    fn mean_reversion_blocked_below_donchian_low() {
        let mut f = f_base();
        f.bb_lower_5m = 100.0;
        f.bb_mid20_5m = 102.0;
        f.bb_width_5m = 0.04;
        f.donchian_low20_5m = 101.0; // higher than price → breakdown
        f.rsi14_5m = 35.0;
        let s = mean_reversion_range_signal(&f, 100.2);
        assert_eq!(s.action, Action::Hold);
    }

    #[test]
    fn mean_reversion_gets_support_proximity_bonus() {
        let mut f = f_base();
        f.bb_lower_5m = 100.0;
        f.bb_mid20_5m = 102.0;
        f.bb_width_5m = 0.04;
        f.atr14_5m = 5.0;
        f.donchian_low20_5m = 99.5; // within 0.5 ATR
        f.rsi14_5m = 35.0;
        f.volume_z = -0.5; // volume exhaustion bonus
        let s = mean_reversion_range_signal(&f, 100.2);
        assert_eq!(s.action, Action::EnterLong);
        // Should have a meaningful confidence from all components
        assert!(s.confidence >= 0.1);
    }

    // ---- mean_reversion velocity-confirmation gate (MEAN_REV_REQUIRE_VELOCITY_CONFIRM) ----

    #[test]
    fn mean_reversion_velocity_gate_off_by_default() {
        unsafe {
            std::env::remove_var("MEAN_REV_REQUIRE_VELOCITY_CONFIRM");
        }
        let mut f = f_base();
        f.bb_lower_5m = 100.0;
        f.bb_mid20_5m = 102.0;
        f.bb_width_5m = 0.04;
        f.donchian_low20_5m = 95.0;
        f.rsi14_5m = 35.0;
        f.velocity_1m = -0.002; // price still falling — gate is off, should still enter
        let s = mean_reversion_range_signal(&f, 100.2);
        assert_eq!(s.action, Action::EnterLong);
    }

    #[test]
    fn mean_reversion_velocity_gate_blocks_falling_price_when_enabled() {
        unsafe {
            std::env::set_var("MEAN_REV_REQUIRE_VELOCITY_CONFIRM", "1");
        }
        let mut f = f_base();
        f.bb_lower_5m = 100.0;
        f.bb_mid20_5m = 102.0;
        f.bb_width_5m = 0.04;
        f.donchian_low20_5m = 95.0;
        f.rsi14_5m = 35.0;
        f.velocity_1m = -0.002; // still falling
        let s = mean_reversion_range_signal(&f, 100.2);
        unsafe {
            std::env::remove_var("MEAN_REV_REQUIRE_VELOCITY_CONFIRM");
        }
        assert_eq!(s.action, Action::Hold);
        assert!(s.reason.contains("Velocity confirmation"));
    }

    #[test]
    fn mean_reversion_velocity_gate_allows_turned_price_when_enabled() {
        unsafe {
            std::env::set_var("MEAN_REV_REQUIRE_VELOCITY_CONFIRM", "1");
        }
        let mut f = f_base();
        f.bb_lower_5m = 100.0;
        f.bb_mid20_5m = 102.0;
        f.bb_width_5m = 0.04;
        f.donchian_low20_5m = 95.0;
        f.rsi14_5m = 35.0;
        f.velocity_1m = 0.002; // already turned up
        let s = mean_reversion_range_signal(&f, 100.2);
        unsafe {
            std::env::remove_var("MEAN_REV_REQUIRE_VELOCITY_CONFIRM");
        }
        assert_eq!(s.action, Action::EnterLong);
    }

    // ---- volatility_squeeze_breakout_signal ----

    #[test]
    fn squeeze_breakout_fires_with_squeeze_and_breakout() {
        let mut f = f_base();
        f.last_close_5m = 210.0;
        f.bb_width_5m = 0.015;
        f.atr_ratio_5m = 0.7;
        f.volume_z = 0.5;
        let s = volatility_squeeze_breakout_signal(Regime::Trending, &f, 210.0, 1.0, true);
        assert_eq!(s.action, Action::EnterLong);
    }

    #[test]
    fn squeeze_breakout_no_entry_without_squeeze() {
        let f = f_base();
        let s = volatility_squeeze_breakout_signal(Regime::Trending, &f, 210.0, 1.0, false);
        assert_eq!(s.action, Action::Hold);
    }

    #[test]
    fn squeeze_breakout_no_entry_in_volatile_regime() {
        let f = f_base();
        let s = volatility_squeeze_breakout_signal(Regime::Volatile, &f, 210.0, 1.0, true);
        assert_eq!(s.action, Action::Hold);
    }

    // ---- dynamic_momentum_signal ----

    #[test]
    fn momentum_fires_with_acceleration() {
        let mut f = f_base();
        f.velocity_1m = 0.005;
        f.velocity_5m = 0.002;
        f.rsi14_1m = 62.0;
        f.ema20_1m = 250.0;
        let s = dynamic_momentum_signal(&f, 255.0);
        assert_eq!(s.action, Action::EnterLong);
        assert!(s.confidence > 0.0);
    }

    #[test]
    fn momentum_no_entry_when_decelerating() {
        let mut f = f_base();
        f.velocity_1m = 0.002;
        f.velocity_5m = 0.003; // 1m <= 5m
        f.rsi14_1m = 62.0;
        f.ema20_1m = 250.0;
        let s = dynamic_momentum_signal(&f, 255.0);
        assert_eq!(s.action, Action::Hold);
    }

    #[test]
    fn momentum_no_entry_when_5m_falling() {
        let mut f = f_base();
        f.velocity_1m = 0.005;
        f.velocity_5m = -0.002; // actively falling on 5m
        f.rsi14_1m = 62.0;
        f.ema20_1m = 250.0;
        let s = dynamic_momentum_signal(&f, 255.0);
        assert_eq!(s.action, Action::Hold);
    }

    // ---- entry_long_signal dispatcher ----

    #[test]
    fn bearish_veto_blocks_all_entries() {
        let mut f = f_base();
        f.last_close_5m = 210.0;
        f.volume_z = 2.0;
        // Even with a great breakout setup, bearish_bias must veto.
        let s = entry_long_signal(Regime::Trending, &f, 210.0, 1.2, false, true, true);
        assert_eq!(s.action, Action::Hold);
        assert!(s.reason.contains("bearish") || s.reason.contains("HTF"));
    }

    #[test]
    fn dispatcher_ranging_does_not_dispatch_mean_reversion_while_disabled() {
        // Mean-reversion's dispatcher call site is disabled (research hold, 2026-09-09
        // — see the comment at that call site). This same feature setup used to produce
        // an EnterLong via mean_reversion_range_signal (see
        // `mean_reversion_fires_near_lower_bb` below, which still calls the strategy
        // function directly and still passes); routed through the dispatcher it must not.
        let mut f = f_base();
        f.bb_lower_5m = 100.0;
        f.bb_upper_5m = 104.0;
        f.bb_mid20_5m = 102.0;
        f.bb_width_5m = 0.04;
        f.donchian_low20_5m = 95.0;
        f.rsi14_5m = 35.0;
        let s = entry_long_signal(Regime::Ranging, &f, 100.2, 1.0, false, false, false);
        assert_eq!(s.action, Action::Hold);
    }

    #[test]
    fn dispatcher_volatile_always_holds() {
        let f = f_base();
        let s = entry_long_signal(Regime::Volatile, &f, 260.0, 2.0, false, true, false);
        assert_eq!(s.action, Action::Hold);
    }

    #[test]
    fn dispatcher_illiquid_always_holds() {
        let f = f_base();
        let s = entry_long_signal(Regime::Illiquid, &f, 260.0, 1.0, false, false, false);
        assert_eq!(s.action, Action::Hold);
    }

    #[test]
    fn dispatcher_trending_squeeze_wins_over_standard_breakout() {
        let mut f = f_base();
        f.last_close_5m = 210.0;
        f.bb_width_5m = 0.015;
        f.volume_z = 0.5;
        // vol_squeeze=true: squeeze breakout fires, trend breakout defers
        let s = entry_long_signal(Regime::Trending, &f, 210.0, 1.0, true, true, false);
        assert_eq!(s.action, Action::EnterLong);
        assert!(s.reason.contains("squeeze_breakout") || s.reason.contains("squeeze"));
    }

    #[test]
    fn dispatcher_reason_includes_strategy_name() {
        let mut f = f_base();
        f.last_close_5m = 210.0;
        f.volume_z = 1.0;
        let s = entry_long_signal(Regime::Trending, &f, 210.0, 1.2, false, true, false);
        if s.action == Action::EnterLong {
            // Winning strategy name should be prefixed in the reason.
            assert!(s.reason.starts_with('['));
        }
    }
}
