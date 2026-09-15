use crate::regime::Regime;
use crate::{money, sizing::ensure_min_notional_dec};
use rust_decimal::Decimal;

#[derive(Debug, Clone)]
pub enum RiskDecision {
    Allow {
        qty: f64,
        notional: f64,
        risk_usdt: f64,
        was_capped: bool,
        reason: String,
    },
    Block {
        reason: String,
        hibernate: bool,
    },
}

pub fn drawdown_fraction(peak_equity_usdt: f64, equity_usdt: f64) -> f64 {
    if peak_equity_usdt <= 0.0 {
        return 0.0;
    }
    let dd = (peak_equity_usdt - equity_usdt) / peak_equity_usdt;
    dd.max(0.0)
}

pub fn risk_fraction_for_regime(regime: Regime) -> f64 {
    match regime {
        Regime::Trending => 0.005,
        Regime::Volatile => 0.0025,
        Regime::Ranging => 0.0035,
        Regime::Illiquid => 0.0,
    }
}

/// Returns a position-size multiplier (0.5 – 1.0) that shrinks exposure when the
/// current ATR is elevated versus its own longer-term average.
///
/// `atr_ratio` = ATR14 / ATR50 on the 5m timeframe (from `Features::atr_ratio_5m`).
///
/// | atr_ratio | multiplier |
/// |-----------|------------|
/// | ≤ 1.0     | 1.00 (no reduction) |
/// | 1.25      | 0.875      |
/// | 1.5       | 0.75       |
/// | ≥ 2.0     | 0.50 (floor) |
pub fn atr_size_multiplier(atr_ratio: f64) -> f64 {
    if !atr_ratio.is_finite() || atr_ratio <= 0.0 {
        return 1.0;
    }
    // Linear interpolation: every 0.1 above 1.0 cuts 5 percentage points, floored at 0.50.
    let reduction = ((atr_ratio - 1.0).max(0.0) * 0.5).min(0.5);
    (1.0 - reduction).clamp(0.5, 1.0)
}

#[allow(clippy::too_many_arguments)]
pub fn size_entry_long(
    regime: Regime,
    equity_usdt: f64,
    usdt_free: f64,
    peak_equity_usdt: f64,
    daily_loss_start_equity_usdt: f64,
    session_pnl_usdt: f64,
    entry_price: f64,
    stop_price: f64,
    step_size: f64,
    min_notional: f64,
    risk_fraction_multiplier: f64,
    max_notional_cap_usdt: f64,
) -> RiskDecision {
    if equity_usdt <= 0.0 {
        return RiskDecision::Block {
            reason: "No money left. We stop.".to_string(),
            hibernate: true,
        };
    }

    if daily_loss_start_equity_usdt > 0.0 {
        let max_loss = daily_loss_start_equity_usdt * 0.02;
        if daily_loss_start_equity_usdt - equity_usdt >= max_loss {
            return RiskDecision::Block {
                reason: "We lost too much today. We rest until tomorrow.".to_string(),
                hibernate: true,
            };
        }
    }

    // Session risk gating: stop if session loss exceeds 1% of total equity
    let session_limit = equity_usdt * 0.01;
    if session_pnl_usdt < -session_limit {
        return RiskDecision::Block {
            reason: "Session loss limit reached. Cooling down.".to_string(),
            hibernate: false,
        };
    }

    let dd = drawdown_fraction(peak_equity_usdt, equity_usdt);
    if dd >= 0.15 {
        return RiskDecision::Block {
            reason: "Big drawdown. We sleep to survive.".to_string(),
            hibernate: true,
        };
    }

    let mut r = risk_fraction_for_regime(regime);
    if risk_fraction_multiplier.is_finite() && risk_fraction_multiplier > 0.0 {
        r *= risk_fraction_multiplier;
    }
    if r <= 0.0 {
        return RiskDecision::Block {
            reason: format!(
                "Calculated risk limit for {:?} is {:.2}%. This is too low to trade.",
                regime, r * 100.0
            ),
            hibernate: false,
        };
    }

    if dd >= 0.10 {
        r *= 0.25;
    }

    let Ok(equity_d) = money::dec_from_f64(equity_usdt) else {
        return RiskDecision::Block {
            reason: "Bad equity value. We stop.".to_string(),
            hibernate: true,
        };
    };
    let Ok(usdt_free_d) = money::dec_from_f64(usdt_free) else {
        return RiskDecision::Block {
            reason: "Bad wallet value. We stop.".to_string(),
            hibernate: true,
        };
    };
    let Ok(entry_d) = money::dec_from_f64(entry_price) else {
        return RiskDecision::Block {
            reason: "Bad entry price. We skip.".to_string(),
            hibernate: false,
        };
    };
    let Ok(stop_d) = money::dec_from_f64(stop_price) else {
        return RiskDecision::Block {
            reason: "Bad stop price. We skip.".to_string(),
            hibernate: false,
        };
    };
    let Ok(step_d) = money::dec_from_f64(step_size) else {
        return RiskDecision::Block {
            reason: "Bad step size. We skip.".to_string(),
            hibernate: false,
        };
    };
    let Ok(min_notional_d) = money::dec_from_f64(min_notional) else {
        return RiskDecision::Block {
            reason: "Bad min_notional. We skip.".to_string(),
            hibernate: false,
        };
    };
    let Ok(r_d) = money::dec_from_f64(r) else {
        return RiskDecision::Block {
            reason: "Bad risk fraction. We skip.".to_string(),
            hibernate: false,
        };
    };

    let risk_usdt_d = equity_d * r_d;

    let stop_distance_d = entry_d - stop_d;
    if stop_distance_d <= Decimal::ZERO {
        return RiskDecision::Block {
            reason: "Stop price is not below entry. We skip.".to_string(),
            hibernate: false,
        };
    }

    let qty_raw_d = risk_usdt_d / stop_distance_d;
    if qty_raw_d <= Decimal::ZERO {
        return RiskDecision::Block {
            reason: "Bad position size. We skip.".to_string(),
            hibernate: false,
        };
    }

    let mut qty_d = money::round_down_to_step_dec(qty_raw_d, step_d);
    if qty_d <= Decimal::ZERO {
        return RiskDecision::Block {
            reason: "Size became too small after rounding. We wait.".to_string(),
            hibernate: false,
        };
    }

    let mut notional_d = money::notional_dec(entry_d, qty_d);

    // Apply the per-trade notional cap (e.g. 20% of equity) BEFORE the min-notional and
    // affordability checks below. Capping only after those checks (the previous behavior)
    // rejected trades outright whenever the uncapped risk-parity notional exceeded free
    // USDT, even though the capped size the caller would have used anyway was affordable.
    // On small accounts this discarded the large majority of otherwise-tradeable signals.
    let mut was_capped = false;
    if max_notional_cap_usdt > 0.0
        && let Ok(cap_d) = money::dec_from_f64(max_notional_cap_usdt)
        && notional_d > cap_d
    {
        let capped_qty_d = money::round_down_to_step_dec(cap_d / entry_d, step_d);
        if capped_qty_d > Decimal::ZERO {
            qty_d = capped_qty_d;
            notional_d = money::notional_dec(entry_d, qty_d);
            was_capped = true;
        }
    }

    if let Err(_e) = ensure_min_notional_dec(entry_d, qty_d, min_notional_d) {
        return RiskDecision::Block {
            reason: format!(
                "Trade size ({:.2} USDT) is smaller than Binance's minimum requirement ({:.2} USDT).",
                notional_d, min_notional_d
            ),
            hibernate: false,
        };
    }

    // Fee buffer: require a little extra USDT.
    let needed = notional_d * money::dec_from_str("1.01").unwrap_or(Decimal::ONE);
    if usdt_free_d + money::tolerance_usdt() < needed {
        return RiskDecision::Block {
            reason: format!(
                "Trade requires {:.2} USDT (including buffer), but only {:.2} USDT is available.",
                needed, usdt_free_d
            ),
            hibernate: false,
        };
    }

    let qty = money::f64_from_dec(qty_d).unwrap_or(0.0);
    let notional = money::f64_from_dec(notional_d).unwrap_or(0.0);
    let risk_usdt = money::f64_from_dec(risk_usdt_d).unwrap_or(equity_usdt * r);

    RiskDecision::Allow {
        qty,
        notional,
        risk_usdt,
        was_capped,
        reason: "Size is safe for your pocket.".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_when_illiquid() {
        let d = size_entry_long(
            Regime::Illiquid,
            1000.0,
            1000.0,
            1000.0,
            1000.0,
            0.0,
            100.0,
            90.0,
            0.001,
            5.0,
            1.0,
            0.0,
        );
        matches!(d, RiskDecision::Block { .. });
    }

    #[test]
    fn blocks_on_daily_loss_limit() {
        let d = size_entry_long(
            Regime::Trending,
            979.0,
            979.0,
            1000.0,
            1000.0,
            0.0,
            100.0,
            90.0,
            0.001,
            5.0,
            1.0,
            0.0,
        );
        // daily loss is 21 >= 20
        assert!(matches!(d, RiskDecision::Block { hibernate: true, .. }));
    }

    #[test]
    fn cap_applied_before_affordability_check_rescues_small_account_trade() {
        // Reproduces the real-world pattern observed on a ~$34.73 account: a tight
        // 5m-ATR stop on BTCUSDT makes the risk-parity notional (~$95) far exceed
        // free USDT (~$33.99), but the 20%-of-equity cap (~$6.95) is affordable and
        // still clears Binance min_notional. Before this fix, the affordability
        // check ran on the *uncapped* notional and blocked the trade outright.
        let d = size_entry_long(
            Regime::Ranging,
            34.73,
            33.99,
            34.73,
            34.73,
            0.0,
            79_000.0,
            78_900.0,
            0.00001,
            5.0,
            1.0,
            6.95, // ~20% of 34.73
        );
        match d {
            RiskDecision::Allow { qty, notional, was_capped, .. } => {
                assert!(was_capped, "expected the cap to engage");
                assert!(qty > 0.0);
                assert!(notional <= 6.95 + 1e-9);
                assert!(notional >= 5.0, "must still clear min_notional");
            }
            RiskDecision::Block { reason, .. } => {
                panic!("expected Allow after capping, got Block: {reason}");
            }
        }
    }

    #[test]
    fn drawdown_throttles_risk() {
        let d = size_entry_long(
            Regime::Trending,
            900.0,
            900.0,
            1000.0,
            900.0,
            0.0,
            100.0,
            90.0,
            0.001,
            5.0,
            1.0,
            0.0,
        );
        match d {
            RiskDecision::Allow { qty, .. } => {
                // risk = 0.5% * 0.25 = 0.125% => 1.125 usdt risk, qty=0.1125 -> 0.112 after rounding 0.001
                assert!(qty > 0.0);
            }
            _ => panic!("expected allow"),
        }
    }

    #[test]
    fn atr_multiplier_no_reduction_at_normal_ratio() {
        assert!((atr_size_multiplier(1.0) - 1.0).abs() < 1e-9);
        assert!((atr_size_multiplier(0.8) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn atr_multiplier_reduces_at_high_ratio() {
        let m = atr_size_multiplier(2.0);
        assert!((m - 0.5).abs() < 1e-9, "expected 0.50 floor, got {m}");
    }

    #[test]
    fn atr_multiplier_clamped_to_floor() {
        assert!(atr_size_multiplier(10.0) >= 0.5);
    }

    #[test]
    fn atr_multiplier_invalid_input_returns_one() {
        assert!((atr_size_multiplier(0.0) - 1.0).abs() < 1e-9);
        assert!((atr_size_multiplier(f64::NAN) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn blocks_on_big_drawdown() {
        let d = size_entry_long(
            Regime::Trending,
            840.0,
            840.0,
            1000.0,
            1000.0,
            0.0,
            100.0,
            90.0,
            0.001,
            5.0,
            1.0,
            0.0,
        );
        assert!(matches!(d, RiskDecision::Block { hibernate: true, .. }));
    }
}
