//! Bulk historical-data fetcher for offline backtesting.
//!
//! Paginates Binance's public `GET /api/v3/klines` endpoint (no API key
//! required) backward-to-forward from `start_ms` to `end_ms`, in chunks of
//! up to 1000 candles, and returns the full series in chronological order.
//!
//! Unlike `candles::fetch_klines_cached_from_base` (used by the live pipeline,
//! which only ever caches the most recent ~200 candles and overwrites that
//! cache every run), this is meant to build a large, stable dataset for
//! `backtest::simulate_from_cache` / `backtest::walk_forward` to run against.
//! Save it to its own directory (e.g. `backtest_data/`) — never point it at
//! the live `data/` cache, which the running bot will overwrite.

use crate::candles::{parse_klines_body, Candle, Interval};
use crate::http_policy::{send_text_with_retry, HttpPolicy};
use crate::persist;
use anyhow::{anyhow, Result};
use reqwest::Client;
use std::path::Path;
use std::time::Duration;

fn interval_ms(interval: Interval) -> i64 {
    match interval {
        Interval::OneMinute => 60_000,
        Interval::FiveMinutes => 5 * 60_000,
        Interval::OneHour => 60 * 60_000,
    }
}

/// Fetch every candle between `start_ms` and `end_ms`, paginating forward in
/// chunks of 1000. Chronologically sorted, deduped by `open_time`. A short
/// delay between pages keeps this well under Binance's public rate limits
/// without needing to lean on 429 backoff.
pub async fn fetch_history_range(
    client: &Client,
    base_url: &str,
    symbol: &str,
    interval: Interval,
    start_ms: i64,
    end_ms: i64,
) -> Result<Vec<Candle>> {
    if start_ms >= end_ms {
        return Err(anyhow!("start_ms must be < end_ms"));
    }

    let policy = HttpPolicy::from_env();
    let step_ms = interval_ms(interval);
    let mut out: Vec<Candle> = Vec::new();
    let mut cursor = start_ms;
    let mut pages = 0u32;

    while cursor < end_ms {
        let url = format!(
            "{base_url}/api/v3/klines?symbol={symbol}&interval={}&startTime={}&endTime={}&limit=1000",
            interval.as_str(),
            cursor,
            end_ms
        );
        let (status, text) =
            send_text_with_retry(client, &policy, &url, || client.get(&url)).await?;
        if !status.is_success() {
            return Err(anyhow!("Klines history returned {status}: {text}"));
        }
        if text.trim().is_empty() {
            break;
        }

        let body: serde_json::Value = serde_json::from_str(&text)?;
        let rows = body
            .as_array()
            .ok_or_else(|| anyhow!("Klines response was not an array"))?;
        if rows.is_empty() {
            break;
        }

        let mut page = parse_klines_body(&body)?;
        let last_open_time = page.last().map(|c| c.open_time).unwrap_or(cursor);
        out.append(&mut page);
        pages += 1;

        if last_open_time <= cursor {
            // Safety valve: no forward progress, stop rather than loop forever.
            break;
        }
        cursor = last_open_time + step_ms;

        // Be a good citizen on the public endpoint even though its weight is low.
        tokio::time::sleep(Duration::from_millis(150)).await;
    }

    let _ = pages;
    out.sort_by_key(|c| c.open_time);
    out.dedup_by_key(|c| c.open_time);
    Ok(out)
}

/// Persist a fetched history series to `{dir}/{symbol}_{interval}.json`,
/// using the same atomic-write-with-.prev pattern as the rest of the app.
/// Compact JSON (not pretty-printed) since these files can hold well over
/// 100,000 candles.
pub fn save_history(dir: &Path, symbol: &str, interval: Interval, candles: &[Candle]) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!("{symbol}_{}.json", interval.as_str()));
    let s = serde_json::to_string(candles)?;
    persist::atomic_write_with_prev(&path, &s)?;
    Ok(())
}
