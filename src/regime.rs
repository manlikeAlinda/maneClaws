use crate::features::Features;

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
}

pub fn detect_regime(f: &Features) -> RegimeResult {
    let atr = f.atr14_5m.max(1e-12);
    let trend_strength = (f.ema20_5m - f.ema50_5m).abs() / atr;

    let rv_long = f.rv_long.max(1e-12);
    let vol_ratio = f.rv_short / rv_long;

    // Priority: Illiquid -> Volatile -> Trending -> Ranging
    if f.volume_z < -1.5 {
        return RegimeResult {
            regime: Regime::Illiquid,
            reason: format!("Volume is low (z={:.2}).", f.volume_z),
            trend_strength,
            vol_ratio,
        };
    }

    if vol_ratio >= 1.5 {
        return RegimeResult {
            regime: Regime::Volatile,
            reason: format!("Market is shaking (vol ratio={:.2}).", vol_ratio),
            trend_strength,
            vol_ratio,
        };
    }

    if trend_strength >= 0.8 && f.ema20_1h > f.ema50_1h {
        return RegimeResult {
            regime: Regime::Trending,
            reason: format!(
                "Trend looks strong (strength={:.2}, 1h up).",
                trend_strength
            ),
            trend_strength,
            vol_ratio,
        };
    }

    if trend_strength <= 0.5 {
        return RegimeResult {
            regime: Regime::Ranging,
            reason: format!("Price is moving sideways (strength={:.2}).", trend_strength),
            trend_strength,
            vol_ratio,
        };
    }

    // Default: ranging-ish if unclear
    RegimeResult {
        regime: Regime::Ranging,
        reason: format!("No clear trend (strength={:.2}).", trend_strength),
        trend_strength,
        vol_ratio,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_features() -> Features {
        Features {
            ema20_1h: 100.0,
            ema50_1h: 100.0,
            ema20_5m: 100.0,
            ema50_5m: 100.0,
            atr14_5m: 10.0,
            bb_mid20_5m: 0.0,
            rsi14_5m: 50.0,
            donchian_high20_5m: 0.0,
            donchian_low20_5m: 0.0,
            rv_short: 0.01,
            rv_long: 0.01,
            volume_z: 0.0,
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
    fn trending_when_strength_and_1h_up() {
        let mut f = base_features();
        f.ema20_5m = 108.0;
        f.ema50_5m = 100.0;
        f.atr14_5m = 10.0; // strength = 0.8
        f.ema20_1h = 101.0;
        f.ema50_1h = 100.0;
        let r = detect_regime(&f);
        assert_eq!(r.regime, Regime::Trending);
    }

    #[test]
    fn ranging_when_low_strength() {
        let mut f = base_features();
        f.ema20_5m = 101.0;
        f.ema50_5m = 100.0;
        f.atr14_5m = 10.0; // strength 0.1
        let r = detect_regime(&f);
        assert_eq!(r.regime, Regime::Ranging);
    }
}
