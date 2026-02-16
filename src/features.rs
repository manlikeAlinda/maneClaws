use crate::candles::Candle;
use anyhow::{anyhow, Result};

#[derive(Debug, Clone, Copy)]
pub struct Features {
    pub ema20_1h: f64,
    pub ema50_1h: f64,
    pub ema20_5m: f64,
    pub ema50_5m: f64,
    pub atr14_5m: f64,
    pub bb_mid20_5m: f64,
    pub rsi14_5m: f64,
    pub donchian_high20_5m: f64,
    pub donchian_low20_5m: f64,
    pub rv_short: f64,
    pub rv_long: f64,
    pub volume_z: f64,
    pub last_close_5m: f64,
    pub last_close_1h: f64,
}

fn closes(candles: &[Candle]) -> Vec<f64> {
    candles.iter().map(|c| c.close).collect()
}

fn volumes(candles: &[Candle]) -> Vec<f64> {
    candles.iter().map(|c| c.volume).collect()
}

pub fn ema_last(values: &[f64], period: usize) -> Result<f64> {
    if period == 0 {
        return Err(anyhow!("EMA period must be > 0"));
    }
    if values.len() < period {
        return Err(anyhow!(
            "Not enough data for EMA{}: have {}, need {}",
            period,
            values.len(),
            period
        ));
    }

    let alpha = 2.0 / (period as f64 + 1.0);
    let mut ema = values[..period].iter().sum::<f64>() / period as f64;
    for &v in &values[period..] {
        ema = alpha * v + (1.0 - alpha) * ema;
    }
    Ok(ema)
}

pub fn atr14_last(candles: &[Candle], period: usize) -> Result<f64> {
    if period == 0 {
        return Err(anyhow!("ATR period must be > 0"));
    }
    // Need at least period+1 candles to compute period true ranges.
    if candles.len() < period + 1 {
        return Err(anyhow!(
            "Not enough candles for ATR{}: have {}, need {}",
            period,
            candles.len(),
            period + 1
        ));
    }

    let mut trs = Vec::with_capacity(candles.len() - 1);
    for i in 1..candles.len() {
        let c = candles[i];
        let prev_close = candles[i - 1].close;
        let hl = c.high - c.low;
        let hc = (c.high - prev_close).abs();
        let lc = (c.low - prev_close).abs();
        let tr = hl.max(hc).max(lc);
        trs.push(tr);
    }

    // Wilder smoothing
    let first_atr = trs[..period].iter().sum::<f64>() / period as f64;
    let mut atr = first_atr;
    for &tr in &trs[period..] {
        atr = (atr * (period as f64 - 1.0) + tr) / period as f64;
    }

    Ok(atr)
}

pub fn donchian_high_low_prev(candles: &[Candle], lookback: usize) -> Result<(f64, f64)> {
    // Use previous N candles excluding the latest candle to avoid self-referential breakouts.
    if lookback == 0 {
        return Err(anyhow!("Donchian lookback must be > 0"));
    }
    if candles.len() < lookback + 1 {
        return Err(anyhow!(
            "Not enough candles for Donchian{}: have {}, need {}",
            lookback,
            candles.len(),
            lookback + 1
        ));
    }

    let end_exclusive = candles.len() - 1;
    let start = end_exclusive - lookback;
    let window = &candles[start..end_exclusive];

    let mut high = f64::NEG_INFINITY;
    let mut low = f64::INFINITY;
    for c in window {
        if c.high > high {
            high = c.high;
        }
        if c.low < low {
            low = c.low;
        }
    }

    Ok((high, low))
}

pub fn realized_vol_stdev_log_returns(closes: &[f64], lookback: usize) -> Result<f64> {
    if lookback == 0 {
        return Err(anyhow!("Vol lookback must be > 0"));
    }
    if closes.len() < lookback + 1 {
        return Err(anyhow!(
            "Not enough closes for vol{}: have {}, need {}",
            lookback,
            closes.len(),
            lookback + 1
        ));
    }

    let start = closes.len() - (lookback + 1);
    let slice = &closes[start..];

    let mut rets = Vec::with_capacity(lookback);
    for i in 1..slice.len() {
        let prev = slice[i - 1];
        let cur = slice[i];
        if prev <= 0.0 || cur <= 0.0 {
            return Err(anyhow!("Bad close value for log return"));
        }
        rets.push((cur / prev).ln());
    }

    let mean = rets.iter().sum::<f64>() / rets.len() as f64;
    let var = rets
        .iter()
        .map(|r| {
            let d = r - mean;
            d * d
        })
        .sum::<f64>()
        / rets.len() as f64;

    Ok(var.sqrt())
}

pub fn zscore_last(values: &[f64], lookback: usize) -> Result<f64> {
    // z-score of last value vs previous lookback values.
    if lookback == 0 {
        return Err(anyhow!("Z-score lookback must be > 0"));
    }
    if values.len() < lookback + 1 {
        return Err(anyhow!(
            "Not enough values for z-score{}: have {}, need {}",
            lookback,
            values.len(),
            lookback + 1
        ));
    }

    let last = *values.last().unwrap();
    let end_exclusive = values.len() - 1;
    let start = end_exclusive - lookback;
    let window = &values[start..end_exclusive];

    let mean = window.iter().sum::<f64>() / window.len() as f64;
    let var = window
        .iter()
        .map(|v| {
            let d = v - mean;
            d * d
        })
        .sum::<f64>()
        / window.len() as f64;

    let std = var.sqrt();
    if std <= 1e-12 {
        return Ok(0.0);
    }

    Ok((last - mean) / std)
}

fn sma_last(values: &[f64], period: usize) -> Result<f64> {
    if period == 0 {
        return Err(anyhow!("SMA period must be > 0"));
    }
    if values.len() < period {
        return Err(anyhow!(
            "Not enough data for SMA{}: have {}, need {}",
            period,
            values.len(),
            period
        ));
    }
    let slice = &values[values.len() - period..];
    Ok(slice.iter().sum::<f64>() / period as f64)
}

fn rsi_last(closes: &[f64], period: usize) -> Result<f64> {
    if period == 0 {
        return Err(anyhow!("RSI period must be > 0"));
    }
    if closes.len() < period + 1 {
        return Err(anyhow!(
            "Not enough closes for RSI{}: have {}, need {}",
            period,
            closes.len(),
            period + 1
        ));
    }

    let start = closes.len() - (period + 1);
    let slice = &closes[start..];

    let mut gain = 0.0;
    let mut loss = 0.0;
    for i in 1..slice.len() {
        let d = slice[i] - slice[i - 1];
        if d >= 0.0 {
            gain += d;
        } else {
            loss += -d;
        }
    }

    let avg_gain = gain / period as f64;
    let avg_loss = loss / period as f64;
    if avg_loss <= 1e-12 {
        return Ok(100.0);
    }
    let rs = avg_gain / avg_loss;
    Ok(100.0 - (100.0 / (1.0 + rs)))
}

pub fn compute_features(candles_5m: &[Candle], candles_1h: &[Candle]) -> Result<Features> {
    let closes_5m = closes(candles_5m);
    let closes_1h = closes(candles_1h);

    let last_close_5m = *closes_5m.last().ok_or_else(|| anyhow!("No 5m candles"))?;
    let last_close_1h = *closes_1h.last().ok_or_else(|| anyhow!("No 1h candles"))?;

    let ema20_5m = ema_last(&closes_5m, 20)?;
    let ema50_5m = ema_last(&closes_5m, 50)?;
    let ema20_1h = ema_last(&closes_1h, 20)?;
    let ema50_1h = ema_last(&closes_1h, 50)?;

    let atr14_5m = atr14_last(candles_5m, 14)?;
    let bb_mid20_5m = sma_last(&closes_5m, 20)?;
    let rsi14_5m = rsi_last(&closes_5m, 14)?;
    let (don_high, don_low) = donchian_high_low_prev(candles_5m, 20)?;

    let rv_short = realized_vol_stdev_log_returns(&closes_5m, 20)?;
    let rv_long = realized_vol_stdev_log_returns(&closes_5m, 100)?;

    let vols_5m = volumes(candles_5m);
    let volume_z = zscore_last(&vols_5m, 100)?;

    Ok(Features {
        ema20_1h,
        ema50_1h,
        ema20_5m,
        ema50_5m,
        atr14_5m,
        bb_mid20_5m,
        rsi14_5m,
        donchian_high20_5m: don_high,
        donchian_low20_5m: don_low,
        rv_short,
        rv_long,
        volume_z,
        last_close_5m,
        last_close_1h,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ema_constant_series_is_constant() {
        let v = vec![10.0; 60];
        let e = ema_last(&v, 20).unwrap();
        assert!((e - 10.0).abs() < 1e-9);
    }

    #[test]
    fn atr_simple_case() {
        // Build candles where TR is constant = 2.
        let mut c = Vec::new();
        let mut close = 100.0;
        for i in 0..30 {
            let open = close;
            let high = open + 1.0;
            let low = open - 1.0;
            close = open; // no gaps
            c.push(Candle {
                open_time: 1_700_000_000_000 + i,
                open,
                high,
                low,
                close,
                volume: 1.0,
            });
        }
        let atr = atr14_last(&c, 14).unwrap();
        assert!((atr - 2.0).abs() < 1e-9);
    }

    #[test]
    fn donchian_prev_excludes_last() {
        let mut c = Vec::new();
        for i in 0..25 {
            c.push(Candle {
                open_time: i,
                open: 1.0,
                high: i as f64,
                low: 0.0,
                close: 1.0,
                volume: 1.0,
            });
        }
        // last candle has highest high, but should be excluded
        c[24].high = 999.0;
        let (h, _l) = donchian_high_low_prev(&c, 20).unwrap();
        assert!(h < 999.0);
    }

    #[test]
    fn realized_vol_zero_for_flat_prices() {
        let v = vec![100.0; 200];
        let rv = realized_vol_stdev_log_returns(&v, 20).unwrap();
        assert!(rv.abs() < 1e-12);
    }

    #[test]
    fn zscore_basic_direction() {
        let mut v = (0..101).map(|i| i as f64).collect::<Vec<_>>();
        v[100] = 200.0;
        let z = zscore_last(&v, 100).unwrap();
        assert!(z > 0.0);
    }
}
