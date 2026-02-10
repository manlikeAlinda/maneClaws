use crate::candles::{Candle, Interval};
use crate::{features, regime, signals, state};
use anyhow::{anyhow, Result};
use std::fs;

#[derive(Debug, Clone)]
pub struct BacktestResult {
    pub trades: usize,
    pub exits: usize,
}

fn load_cached(symbol: &str, interval: Interval) -> Result<Vec<Candle>> {
    let path = std::path::Path::new("data").join(format!("{symbol}_{}.json", interval.as_str()));
    if !path.exists() {
        return Err(anyhow!("Missing cache file: {}", path.display()));
    }
    let s = fs::read_to_string(&path)?;
    Ok(serde_json::from_str::<Vec<Candle>>(&s)?)
}

/// Minimal simulation-ready pipeline:
/// - Reads cached 5m + 1h candles
/// - Runs the same features/regime/signal logic
/// - Uses the same position manager for exits
/// No curve fitting; just plumbing.
pub fn simulate_from_cache(symbol: &str) -> Result<BacktestResult> {
    let candles_5m = load_cached(symbol, Interval::FiveMinutes)?;
    let candles_1h = load_cached(symbol, Interval::OneHour)?;

    if candles_5m.len() < 120 || candles_1h.len() < 60 {
        return Err(anyhow!("Not enough cached candles to simulate"));
    }

    let mut st = state::BotState::new(1000.0);
    let mut trades = 0usize;
    let mut exits = 0usize;

    // Walk forward on 5m candles; for each step use slices ending at i.
    for i in 0..candles_5m.len() {
        let end = i + 1;
        if end < 120 {
            continue;
        }

        // Use a rolling window.
        let w5 = &candles_5m[end - 200.min(end)..end];
        let w1 = &candles_1h[candles_1h.len().saturating_sub(200)..];

        let f = match features::compute_features(w5, w1) {
            Ok(x) => x,
            Err(_) => continue,
        };
        let r = regime::detect_regime(&f);

        let price = w5.last().unwrap().close;
        let now_ms = w5.last().unwrap().open_time.max(0) as u64;

        match st.position {
            state::Position::Flat => {
                let sig = signals::trend_breakout_long_only(r.regime, &f, price, r.vol_ratio);
                if sig.action == signals::Action::EnterLong {
                    if let Some(stop) = sig.stop_price {
                        st.enter_long(price, 0.001, stop, now_ms);
                        trades += 1;
                    }
                }
            }
            state::Position::Long { .. } => {
                let (d, _) = st.manage_open_position(price, f.atr14_5m, now_ms);
                if d == state::PositionDecision::ExitLong {
                    st.exit_to_flat_with_cooldown(now_ms);
                    exits += 1;
                }
            }
        }
    }

    Ok(BacktestResult { trades, exits })
}
