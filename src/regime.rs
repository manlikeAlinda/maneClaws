use crate::features::Features;
use std::env;

fn env_f64_default(name: &str, def: f64) -> f64 {
    env::var(name).ok().and_then(|v| v.parse::<f64>().ok()).unwrap_or(def)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Regime {
    Trending,
    Ranging,
    Volatile,
    Illiquid,
}

#[derive(Debug, Clone)]
pub struct RegimeResult {
    pub regime: Regime,
    pub reason: String,
    pub trend_strength: f64,
    pub vol_ratio: f64,
    /// True when ATR14/ATR50 < threshold — volatility is compressing (pre-breakout squeeze).
    /// Threshold: env `REGIME_ATR_RATIO_SQUEEZE` (default 0.8).
    pub vol_squeeze: bool,
    /// True when 1h EMA stack is fully bullish: EMA20 > EMA50 > EMA200.
    pub htf_bullish: bool,
    /// True when 1h EMA stack is fully bearish: EMA20 < EMA50 < EMA200.
    /// Spot-only systems use this to veto new long entries and tighten exit logic.
    pub bearish_bias: bool,
}

pub fn detect_regime(f: &Features) -> RegimeResult {
    // --- Configurable thresholds (all overridable via environment variables) ---
    let trend_strength_min    = env_f64_default("REGIME_TREND_STRENGTH_MIN", 0.8);
    let trend_strength_ranging= env_f64_default("REGIME_TREND_STRENGTH_RANGING", 0.5);
    let vol_ratio_volatile    = env_f64_default("REGIME_VOL_RATIO_VOLATILE", 1.5);
    let atr_ratio_volatile    = env_f64_default("REGIME_ATR_RATIO_VOLATILE", 1.5);
    let atr_ratio_squeeze     = env_f64_default("REGIME_ATR_RATIO_SQUEEZE", 0.8);
    let volume_z_illiquid     = env_f64_default("REGIME_VOLUME_Z_ILLIQUID", -1.5);

    let atr = f.atr14_5m.max(1e-12);
    let trend_strength = (f.ema20_5m - f.ema50_5m).abs() / atr;

    let rv_long = f.rv_long.max(1e-12);
    let vol_ratio = f.rv_short / rv_long;

    // Volatility squeeze: ATR has contracted relative to its longer-term average.
    let vol_squeeze = f.atr_ratio_5m > 0.0 && f.atr_ratio_5m < atr_ratio_squeeze;

    // Higher-timeframe alignment checks (all three EMAs must stack correctly).
    let htf_bullish  = f.ema20_1h > f.ema50_1h && f.ema50_1h > f.ema200_1h;
    let bearish_bias = f.ema20_1h < f.ema50_1h && f.ema50_1h < f.ema200_1h;

    // Priority: Illiquid → Volatile → Trending → Ranging
    if f.volume_z < volume_z_illiquid {
        return RegimeResult {
            regime: Regime::Illiquid,
            reason: format!("Volume is low (z={:.2}).", f.volume_z),
            trend_strength,
            vol_ratio,
            vol_squeeze,
            htf_bullish,
            bearish_bias,
        };
    }

    // Volatile: elevated short-term vol ratio OR ATR has expanded sharply above its norm.
    if vol_ratio >= vol_ratio_volatile || f.atr_ratio_5m > atr_ratio_volatile {
        return RegimeResult {
            regime: Regime::Volatile,
            reason: format!(
                "Market is shaking (vol_ratio={:.2}, atr_ratio={:.2}).",
                vol_ratio, f.atr_ratio_5m
            ),
            trend_strength,
            vol_ratio,
            vol_squeeze,
            htf_bullish,
            bearish_bias,
        };
    }

    // Trending: strong 5m EMA separation AND the 1h higher-timeframe stack is bullish.
    // Bearish conditions (bearish_bias) prevent Trending classification even with
    // local strength — avoids entering longs into a confirmed HTF downtrend.
    if trend_strength >= trend_strength_min && htf_bullish && !bearish_bias {
        return RegimeResult {
            regime: Regime::Trending,
            reason: format!(
                "Trend looks strong (strength={:.2}, HTF bullish aligned).",
                trend_strength
            ),
            trend_strength,
            vol_ratio,
            vol_squeeze,
            htf_bullish,
            bearish_bias,
        };
    }

    // Ranging: low trend strength on 5m — price moving sideways.
    if trend_strength <= trend_strength_ranging {
        return RegimeResult {
            regime: Regime::Ranging,
            reason: format!("Price is moving sideways (strength={:.2}).", trend_strength),
            trend_strength,
            vol_ratio,
            vol_squeeze,
            htf_bullish,
            bearish_bias,
        };
    }

    // Default: ranging-ish when no clear directional signal.
    RegimeResult {
        regime: Regime::Ranging,
        reason: format!("No clear trend (strength={:.2}).", trend_strength),
        trend_strength,
        vol_ratio,
        vol_squeeze,
        htf_bullish,
        bearish_bias,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::features::Features;

    fn base_features() -> Features {
        Features {
            ema20_1h: 100.0,
            ema50_1h: 100.0,
            ema200_1h: 100.0,
            ema20_5m: 100.0,
            ema50_5m: 100.0,
            atr14_5m: 10.0,
            atr14_1m: 10.0,
            atr_ratio_5m: 1.0,
            bb_mid20_5m: 100.0,
            bb_upper_5m: 102.0,
            bb_lower_5m: 98.0,
            bb_width_5m: 0.04,
            rsi14_5m: 50.0,
            rsi14_1m: 50.0,
            donchian_high20_5m: 0.0,
            donchian_low20_5m: 0.0,
            rv_short: 0.01,
            rv_long: 0.01,
            volume_z: 0.0,
            ema20_1m: 100.0,
            velocity_1m: 0.0,
            velocity_5m: 0.0,
            last_close_1m: 0.0,
            last_close_5m: 0.0,
            last_close_1h: 0.0,
        }
    }

    #[test]
    fn illiquid_has_priority() {
        let mut f = base_features();
        f.volume_z = -2.0;
        f.rv_short = 0.05;
        f.rv_long = 0.01;
        f.ema20_5m = 200.0;
        let r = detect_regime(&f);
        assert_eq!(r.regime, Regime::Illiquid);
    }

    #[test]
    fn volatile_overrides_trending() {
        let mut f = base_features();
        f.ema20_5m = 120.0;
        f.ema50_5m = 100.0;
        f.atr14_5m = 10.0; // strength = 2.0
        f.ema20_1h = 101.0;
        f.ema50_1h = 100.0;
        f.rv_short = 0.03;
        f.rv_long = 0.01;
        let r = detect_regime(&f);
        assert_eq!(r.regime, Regime::Volatile);
    }

    #[test]
    fn volatile_also_on_high_atr_ratio() {
        let mut f = base_features();
        f.atr_ratio_5m = 1.8;
        let r = detect_regime(&f);
        assert_eq!(r.regime, Regime::Volatile);
    }

    #[test]
    fn trending_when_strength_and_htf_bullish() {
        let mut f = base_features();
        f.ema20_5m = 108.0;
        f.ema50_5m = 100.0;
        f.atr14_5m = 10.0; // strength = 0.8
        f.ema20_1h = 105.0;
        f.ema50_1h = 102.0;
        f.ema200_1h = 100.0;
        let r = detect_regime(&f);
        assert_eq!(r.regime, Regime::Trending);
        assert!(r.htf_bullish);
        assert!(!r.bearish_bias);
    }

    #[test]
    fn not_trending_without_htf_bullish() {
        let mut f = base_features();
        f.ema20_5m = 108.0;
        f.ema50_5m = 100.0;
        f.atr14_5m = 10.0;
        f.ema20_1h = 100.0;
        f.ema50_1h = 100.0;
        f.ema200_1h = 100.0;
        let r = detect_regime(&f);
        assert_eq!(r.regime, Regime::Ranging);
        assert!(!r.htf_bullish);
    }

    #[test]
    fn ranging_when_low_strength() {
        let mut f = base_features();
        f.ema20_5m = 101.0;
        f.ema50_5m = 100.0;
        f.atr14_5m = 10.0;
        let r = detect_regime(&f);
        assert_eq!(r.regime, Regime::Ranging);
    }

    #[test]
    fn vol_squeeze_detected() {
        let mut f = base_features();
        f.atr_ratio_5m = 0.6;
        let r = detect_regime(&f);
        assert!(r.vol_squeeze);
    }

    #[test]
    fn no_squeeze_at_normal_atr_ratio() {
        let mut f = base_features();
        f.atr_ratio_5m = 1.0;
        let r = detect_regime(&f);
        assert!(!r.vol_squeeze);
    }

    #[test]
    fn bearish_bias_detected_when_ema_stack_fully_bearish() {
        let mut f = base_features();
        f.ema20_1h = 98.0;
        f.ema50_1h = 100.0;
        f.ema200_1h = 102.0;
        let r = detect_regime(&f);
        assert!(r.bearish_bias);
        assert!(!r.htf_bullish);
    }

    #[test]
    fn bearish_bias_blocks_trending_classification() {
        let mut f = base_features();
        // Strong 5m trend
        f.ema20_5m = 108.0;
        f.ema50_5m = 100.0;
        f.atr14_5m = 10.0;
        // But HTF is bearish
        f.ema20_1h = 98.0;
        f.ema50_1h = 100.0;
        f.ema200_1h = 102.0;
        let r = detect_regime(&f);
        // Should NOT classify as Trending
        assert_ne!(r.regime, Regime::Trending);
        assert!(r.bearish_bias);
    }
}
