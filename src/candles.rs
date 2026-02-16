use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::http_policy::{send_with_retry, HttpPolicy};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
pub struct Candle {
    pub open_time: i64,
    pub open: f64,
    pub high: f64,
    pub low: f64,
    pub close: f64,
    pub volume: f64,
}

#[derive(Debug, Clone, Copy)]
pub enum Interval {
    OneMinute,
    FiveMinutes,
    OneHour,
}

impl Interval {
    pub fn as_str(self) -> &'static str {
        match self {
            Interval::OneMinute => "1m",
            Interval::FiveMinutes => "5m",
            Interval::OneHour => "1h",
        }
    }
}

fn cache_path(cache_dir: &Path, symbol: &str, interval: Interval) -> PathBuf {
    cache_dir.join(format!("{symbol}_{}.json", interval.as_str()))
}

fn cache_is_fresh(path: &Path, max_age: Duration) -> Result<bool> {
    let meta = match fs::metadata(path) {
        Ok(m) => m,
        Err(_) => return Ok(false),
    };
    let modified = meta.modified().unwrap_or(SystemTime::UNIX_EPOCH);
    let age = SystemTime::now()
        .duration_since(modified)
        .unwrap_or(Duration::from_secs(999_999));
    Ok(age <= max_age)
}

fn parse_klines_body(body: &serde_json::Value) -> Result<Vec<Candle>> {
    // Binance kline format: array of arrays.
    // [
    //   [
    //     0 openTime,
    //     1 open,
    //     2 high,
    //     3 low,
    //     4 close,
    //     5 volume,
    //     ...
    //   ],
    // ]
    let rows = body
        .as_array()
        .ok_or_else(|| anyhow!("Klines response was not an array"))?;

    let mut out = Vec::with_capacity(rows.len());

    for row in rows {
        let cols = row
            .as_array()
            .ok_or_else(|| anyhow!("Kline row was not an array"))?;
        if cols.len() < 6 {
            return Err(anyhow!("Kline row had too few columns"));
        }

        let open_time = cols[0]
            .as_i64()
            .ok_or_else(|| anyhow!("openTime was not an integer"))?;
        let open = cols[1]
            .as_str()
            .ok_or_else(|| anyhow!("open was not a string"))?
            .parse::<f64>()?;
        let high = cols[2]
            .as_str()
            .ok_or_else(|| anyhow!("high was not a string"))?
            .parse::<f64>()?;
        let low = cols[3]
            .as_str()
            .ok_or_else(|| anyhow!("low was not a string"))?
            .parse::<f64>()?;
        let close = cols[4]
            .as_str()
            .ok_or_else(|| anyhow!("close was not a string"))?
            .parse::<f64>()?;
        let volume = cols[5]
            .as_str()
            .ok_or_else(|| anyhow!("volume was not a string"))?
            .parse::<f64>()?;

        out.push(Candle {
            open_time,
            open,
            high,
            low,
            close,
            volume,
        });
    }

    Ok(out)
}

pub fn load_cached(cache_dir: &Path, symbol: &str, interval: Interval, max_age: Duration) -> Result<Option<Vec<Candle>>> {
    let path = cache_path(cache_dir, symbol, interval);
    if !cache_is_fresh(&path, max_age)? {
        return Ok(None);
    }

    let s = fs::read_to_string(&path)
        .with_context(|| format!("Failed reading candle cache: {}", path.display()))?;
    let candles = serde_json::from_str::<Vec<Candle>>(&s)
        .with_context(|| format!("Failed parsing candle cache: {}", path.display()))?;
    Ok(Some(candles))
}

pub fn save_cache(cache_dir: &Path, symbol: &str, interval: Interval, candles: &[Candle]) -> Result<()> {
    let path = cache_path(cache_dir, symbol, interval);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let s = serde_json::to_string_pretty(candles)?;
    fs::write(&path, s).with_context(|| format!("Failed writing candle cache: {}", path.display()))?;
    Ok(())
}

pub async fn fetch_klines_from_base(
    client: &Client,
    base_url: &str,
    symbol: &str,
    interval: Interval,
    limit: usize,
) -> Result<Vec<Candle>> {
    if limit == 0 || limit > 1000 {
        return Err(anyhow!("Bad kline limit: {limit} (must be 1..=1000)"));
    }

    let url = format!(
        "{base_url}/api/v3/klines?symbol={symbol}&interval={}&limit={}",
        interval.as_str(),
        limit
    );

    let policy = HttpPolicy::from_env();
    let resp = send_with_retry(client, &policy, || client.get(&url)).await?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();

    if !status.is_success() {
        return Err(anyhow!("Klines returned {status}: {text}"));
    }

    let body = serde_json::from_str::<serde_json::Value>(&text)?;
    parse_klines_body(&body)
}

pub async fn fetch_klines_cached_from_base(
    client: &Client,
    base_url: &str,
    cache_dir: &Path,
    symbol: &str,
    interval: Interval,
    limit: usize,
    max_age: Duration,
) -> Result<Vec<Candle>> {
    if let Some(cached) = load_cached(cache_dir, symbol, interval, max_age)? {
        return Ok(cached);
    }

    let candles = fetch_klines_from_base(client, base_url, symbol, interval, limit).await?;
    save_cache(cache_dir, symbol, interval, &candles)?;
    Ok(candles)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_binance_klines_shape() {
        let body = serde_json::json!([
            [1700000000000i64, "1.0", "2.0", "0.5", "1.5", "10.0", 1700000001000i64],
            [1700000001000i64, "1.5", "2.5", "1.2", "2.0", "12.0", 1700000002000i64]
        ]);

        let candles = parse_klines_body(&body).unwrap();
        assert_eq!(candles.len(), 2);
        assert_eq!(candles[0].open_time, 1700000000000);
        assert!((candles[0].open - 1.0).abs() < 1e-12);
        assert!((candles[0].high - 2.0).abs() < 1e-12);
        assert!((candles[0].low - 0.5).abs() < 1e-12);
        assert!((candles[0].close - 1.5).abs() < 1e-12);
        assert!((candles[0].volume - 10.0).abs() < 1e-12);
    }

    #[test]
    fn cache_freshness_works() {
        use std::time::UNIX_EPOCH;

        let uniq = std::time::SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("binance_survival_bot_test_{uniq}"));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("x.json");
        fs::write(&path, "[]").unwrap();

        assert!(cache_is_fresh(&path, Duration::from_secs(60)).unwrap());

        // best-effort cleanup
        let _ = fs::remove_file(&path);
        let _ = fs::remove_dir(&dir);
    }
}
