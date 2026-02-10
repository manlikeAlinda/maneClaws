use anyhow::{anyhow, Result};

pub fn round_down_to_step(value: f64, step: f64) -> f64 {
    if step <= 0.0 {
        return value;
    }
    (value / step).floor() * step
}

pub fn notional(price: f64, qty: f64) -> f64 {
    price * qty
}

pub fn ensure_min_notional(price: f64, qty: f64, min_notional: f64) -> Result<()> {
    let n = notional(price, qty);
    if n + 1e-12 < min_notional {
        return Err(anyhow!(
            "Order blocked: notional {:.8} < min_notional {:.8}",
            n,
            min_notional
        ));
    }
    Ok(())
}

pub fn clamp_target_notional(target: f64, equity_usdt: f64) -> f64 {
    target.min(equity_usdt)
}

pub fn max_sell_qty(btc_free: f64, step_size: f64) -> f64 {
    round_down_to_step(btc_free, step_size)
}

pub fn sell_qty_for_target_notional(
    price: f64,
    target_notional: f64,
    btc_free: f64,
    step_size: f64,
) -> f64 {
    let raw = target_notional / price;
    let qty = round_down_to_step(raw, step_size);
    qty.min(max_sell_qty(btc_free, step_size))
}

pub fn ensure_affordable_sell(qty: f64, btc_free: f64) -> Result<()> {
    if qty <= 0.0 {
        return Err(anyhow!("Order blocked: qty <= 0 after rounding/clamping"));
    }
    if qty - btc_free > 1e-12 {
        return Err(anyhow!(
            "Order blocked: qty {} > btc_free {}",
            qty,
            btc_free
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_down_respects_step() {
        let v = round_down_to_step(0.123456, 0.001);
        assert!((v - 0.123).abs() < 1e-12);
    }

    #[test]
    fn min_notional_blocks_small_trade() {
        let e = ensure_min_notional(100.0, 0.01, 5.0).unwrap_err();
        assert!(e.to_string().contains("notional"));
    }
}
