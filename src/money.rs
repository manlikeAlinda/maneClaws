use anyhow::{anyhow, Result};
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::Decimal;
use std::str::FromStr;

pub(crate) const USDT_TOLERANCE_STR: &str = "0.000000000001";

pub(crate) fn dec_from_f64(v: f64) -> Result<Decimal> {
    if !v.is_finite() {
        return Err(anyhow!("non-finite f64"));
    }
    Decimal::from_str(&v.to_string()).map_err(|e| anyhow!("decimal parse failed: {e}"))
}

pub(crate) fn dec_from_str(s: &str) -> Result<Decimal> {
    Decimal::from_str(s).map_err(|e| anyhow!("decimal parse failed: {e}"))
}

pub(crate) fn f64_from_dec(v: Decimal) -> Result<f64> {
    v.to_f64().ok_or_else(|| anyhow!("decimal to f64 conversion failed"))
}

pub(crate) fn tolerance_usdt() -> Decimal {
    Decimal::from_str(USDT_TOLERANCE_STR).unwrap_or(Decimal::ZERO)
}

pub(crate) fn round_down_to_step_dec(value: Decimal, step: Decimal) -> Decimal {
    if step <= Decimal::ZERO {
        return value;
    }
    let q = (value / step).floor();
    q * step
}

pub(crate) fn notional_dec(price: Decimal, qty: Decimal) -> Decimal {
    price * qty
}
