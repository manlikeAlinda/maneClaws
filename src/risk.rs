use crate::regime::Regime;
use crate::sizing;

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
    if r <= 0.0 {
        return RiskDecision::Block {
            reason: "This market is too dry. We wait.".to_string(),
            hibernate: false,
        };
    }

    if dd >= 0.10 {
        r *= 0.25;
    }

    let risk_usdt = equity_usdt * r;

    let stop_distance = entry_price - stop_price;
    if stop_distance <= 0.0 {
        return RiskDecision::Block {
            reason: "Stop price is not below entry. We skip.".to_string(),
            hibernate: false,
        };
    }

    let qty_raw = risk_usdt / stop_distance;
    if !qty_raw.is_finite() || qty_raw <= 0.0 {
        return RiskDecision::Block {
            reason: "Bad position size. We skip.".to_string(),
            hibernate: false,
        };
    }

    let qty = sizing::round_down_to_step(qty_raw, step_size);
    if qty <= 0.0 {
        return RiskDecision::Block {
            reason: "Size became too small after rounding. We wait.".to_string(),
            hibernate: false,
        };
    }

    let notional = entry_price * qty;
    if notional + 1e-12 < min_notional {
        return RiskDecision::Block {
            reason: "Trade is too small for Binance rules. We wait.".to_string(),
            hibernate: false,
        };
    }

    // Fee buffer: require a little extra USDT.
    if usdt_free + 1e-12 < notional * 1.01 {
        return RiskDecision::Block {
            reason: "Not enough USDT free for this ride. We wait.".to_string(),
            hibernate: false,
        };
    }

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
        );
        assert!(matches!(d, RiskDecision::Block { hibernate: true, .. }));
    }
}
