use crate::features::Features;
use crate::regime::Regime;

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
    pub reason: String,
    pub stop_price: Option<f64>,
}

fn clamp(v: f64, lo: f64, hi: f64) -> f64 {
    if v < lo {
        lo
    } else if v > hi {
        hi
    } else {
        v
    }
}

pub fn trend_breakout_long_only(
    regime: Regime,
    f: &Features,
    current_price: f64,
    vol_ratio: f64,
) -> Signal {
    if regime != Regime::Trending {
        return Signal {
            action: Action::Hold,
            confidence: 0.0,
            reason: "Not a good road for breakouts.".to_string(),
            stop_price: None,
        };
    }

    if f.last_close_5m <= f.donchian_high20_5m {
        return Signal {
            action: Action::Hold,
            confidence: 0.0,
            reason: "No breakout yet.".to_string(),
            stop_price: None,
        };
    }

    let confirmed = f.volume_z > 0.0 || vol_ratio > 1.1;
    if !confirmed {
        return Signal {
            action: Action::Hold,
            confidence: 0.0,
            reason: "Breakout is weak. We wait.".to_string(),
            stop_price: None,
        };
    }

    let atr = f.atr14_5m.max(1e-12);
    let stop = current_price - 1.8 * atr;

    let raw = (f.last_close_5m - f.donchian_high20_5m) / atr;
    let confidence = clamp(raw, 0.0, 2.0) / 2.0;

    Signal {
        action: Action::EnterLong,
        confidence,
        reason: "Breakout is strong. We can ride the trend.".to_string(),
        stop_price: Some(stop),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f_base() -> Features {
        Features {
            ema20_1h: 0.0,
            ema50_1h: 0.0,
            ema20_5m: 0.0,
            ema50_5m: 0.0,
            atr14_5m: 100.0,
            donchian_high20_5m: 200.0,
            donchian_low20_5m: 0.0,
            rv_short: 0.0,
            rv_long: 0.0,
            volume_z: 1.0,
            last_close_5m: 250.0,
            last_close_1h: 0.0,
        }
    }

    #[test]
    fn no_entry_when_not_trending() {
        let f = f_base();
        let s = trend_breakout_long_only(Regime::Ranging, &f, 260.0, 1.2);
        assert_eq!(s.action, Action::Hold);
    }

    #[test]
    fn entry_when_breakout_and_confirmed() {
        let f = f_base();
        let s = trend_breakout_long_only(Regime::Trending, &f, 260.0, 1.2);
        assert_eq!(s.action, Action::EnterLong);
        assert!(s.stop_price.unwrap() < 260.0);
        assert!(s.confidence >= 0.0 && s.confidence <= 1.0);
    }

    #[test]
    fn no_entry_without_confirmation() {
        let mut f = f_base();
        f.volume_z = -0.5;
        let s = trend_breakout_long_only(Regime::Trending, &f, 260.0, 1.05);
        assert_eq!(s.action, Action::Hold);
    }
}
