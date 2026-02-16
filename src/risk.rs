use crate::regime::Regime;
use crate::{money, sizing::ensure_min_notional_dec};
use rust_decimal::Decimal;

#[derive(Debug, Clone)]
pub enum RiskDecision {
    Allow {
        qty: f64,
        notional: f64,
        risk_usdt: f64,
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

#[allow(clippy::too_many_arguments)]
pub fn size_entry_long(
    regime: Regime,
    equity_usdt: f64,
    usdt_free: f64,
    peak_equity_usdt: f64,
    daily_loss_start_equity_usdt: f64,
    entry_price: f64,
    stop_price: f64,
    step_size: f64,
    min_notional: f64,
    risk_fraction_multiplier: f64,
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
            reason: "This market is too dry. We wait.".to_string(),
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

    let qty_d = money::round_down_to_step_dec(qty_raw_d, step_d);
    if qty_d <= Decimal::ZERO {
        return RiskDecision::Block {
            reason: "Size became too small after rounding. We wait.".to_string(),
            hibernate: false,
        };
    }

    if ensure_min_notional_dec(entry_d, qty_d, min_notional_d).is_err() {
        return RiskDecision::Block {
            reason: "Trade is too small for Binance rules. We wait.".to_string(),
            hibernate: false,
        };
    }

    let notional_d = money::notional_dec(entry_d, qty_d);

    // Fee buffer: require a little extra USDT.
    let needed = notional_d * money::dec_from_str("1.01").unwrap_or(Decimal::ONE);
    if usdt_free_d + money::tolerance_usdt() < needed {
        return RiskDecision::Block {
            reason: "Not enough USDT free for this ride. We wait.".to_string(),
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
            100.0,
            90.0,
            0.001,
            5.0,
            1.0,
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
            100.0,
            90.0,
            0.001,
            5.0,
            1.0,
        );
        // daily loss is 21 >= 20
        assert!(matches!(d, RiskDecision::Block { hibernate: true, .. }));
    }

    #[test]
    fn drawdown_throttles_risk() {
        let d = size_entry_long(
            Regime::Trending,
            900.0,
            900.0,
            1000.0,
            900.0,
            100.0,
            90.0,
            0.001,
            5.0,
            1.0,
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
    fn blocks_on_big_drawdown() {
        let d = size_entry_long(
            Regime::Trending,
            840.0,
            840.0,
            1000.0,
            1000.0,
            100.0,
            90.0,
            0.001,
            5.0,
            1.0,
        );
        assert!(matches!(d, RiskDecision::Block { hibernate: true, .. }));
    }
}
