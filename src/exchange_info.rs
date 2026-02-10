use anyhow::{anyhow, Result};
use reqwest::Client;
use serde::Deserialize;
use tokio::time::sleep;

#[derive(Debug, Deserialize)]
pub struct ExchangeInfo {
    pub symbols: Vec<SymbolInfo>,
}

#[derive(Debug, Deserialize)]
pub struct SymbolInfo {
    pub symbol: String,
    pub status: String,
    #[serde(rename = "baseAsset")]
    pub base_asset: String,
    #[serde(rename = "quoteAsset")]
    pub quote_asset: String,
    pub filters: Vec<Filter>,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "filterType")]
pub enum Filter {
    LOT_SIZE {
        #[serde(rename = "minQty")]
        min_qty: String,
        #[serde(rename = "maxQty")]
        max_qty: String,
        #[serde(rename = "stepSize")]
        step_size: String,
    },
    PRICE_FILTER {
        #[serde(rename = "minPrice")]
        min_price: String,
        #[serde(rename = "maxPrice")]
        max_price: String,
        #[serde(rename = "tickSize")]
        tick_size: String,
    },
    MIN_NOTIONAL {
        #[serde(rename = "minNotional")]
        min_notional: String,
    },
    NOTIONAL {
        #[serde(rename = "minNotional")]
        min_notional: String,
        #[serde(rename = "applyMinToMarket")]
        apply_min_to_market: bool,
    },
    #[serde(other)]
    OTHER,
}

pub async fn fetch_exchange_info(client: &Client) -> Result<ExchangeInfo> {
    fetch_exchange_info_from_base(client, "https://api.binance.com").await
}

pub async fn fetch_exchange_info_from_base(client: &Client, base_url: &str) -> Result<ExchangeInfo> {
    let url = format!("{base_url}/api/v3/exchangeInfo");

    let retries = std::env::var("BOT_HTTP_RETRIES")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(3);
    let base_backoff_ms = std::env::var("BOT_HTTP_BACKOFF_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(250);
    let max_backoff_ms = std::env::var("BOT_HTTP_BACKOFF_MAX_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(2_000);

    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 0..=retries {
        let resp = match client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                last_err = Some(anyhow!("exchangeInfo request failed: {e}"));
                if attempt < retries {
                    let backoff = (base_backoff_ms.saturating_mul(1u64 << attempt)).min(max_backoff_ms);
                    sleep(std::time::Duration::from_millis(backoff)).await;
                    continue;
                }
                break;
            }
        };

        let status = resp.status();
        if status.as_u16() == 429 || status.is_server_error() {
            let text = resp.text().await.unwrap_or_default();
            last_err = Some(anyhow!("exchangeInfo returned {status}: {text}"));
            if attempt < retries {
                let backoff = (base_backoff_ms.saturating_mul(1u64 << attempt)).min(max_backoff_ms);
                sleep(std::time::Duration::from_millis(backoff)).await;
                continue;
            }
            break;
        }

        let resp = resp.error_for_status()?;
        return Ok(resp.json::<ExchangeInfo>().await?);
    }

    Err(last_err.unwrap_or_else(|| anyhow!("exchangeInfo request failed")))
}

pub async fn btcusdt_rules(client: &Client) -> Result<(f64, f64, f64)> {
    // returns: (step_size, tick_size, min_notional)
    let info = fetch_exchange_info(client).await?;

    let sym = info
        .symbols
        .into_iter()
        .find(|s| s.symbol == "BTCUSDT" && s.status == "TRADING")
        .ok_or_else(|| anyhow!("BTCUSDT not found or not trading"))?;

    let mut step_size: Option<f64> = None;
    let mut tick_size: Option<f64> = None;
    let mut min_notional: Option<f64> = None;

    for f in sym.filters {
        match f {
            Filter::LOT_SIZE { step_size: ss, .. } => step_size = Some(ss.parse()?),
            Filter::PRICE_FILTER { tick_size: ts, .. } => tick_size = Some(ts.parse()?),
            Filter::MIN_NOTIONAL { min_notional: mn } => min_notional = Some(mn.parse()?),
            Filter::NOTIONAL { min_notional: mn, .. } => min_notional = Some(mn.parse()?),
            _ => {}
        }
    }

    Ok((
        step_size.ok_or_else(|| anyhow!("Missing LOT_SIZE.stepSize"))?,
        tick_size.ok_or_else(|| anyhow!("Missing PRICE_FILTER.tickSize"))?,
        min_notional.ok_or_else(|| anyhow!("Missing MIN_NOTIONAL/NOTIONAL.minNotional"))?,
    ))
}

pub async fn symbol_rules(client: &Client, symbol: &str) -> Result<(f64, f64, f64)> {
    symbol_rules_from_base(client, "https://api.binance.com", symbol).await
}

pub async fn symbol_rules_from_base(
    client: &Client,
    base_url: &str,
    symbol: &str,
) -> Result<(f64, f64, f64)> {
    let info = fetch_exchange_info_from_base(client, base_url).await?;

    let sym = info
        .symbols
        .into_iter()
        .find(|s| s.symbol == symbol && s.status == "TRADING")
        .ok_or_else(|| anyhow!("{symbol} not found or not trading"))?;

    let mut step_size: Option<f64> = None;
    let mut tick_size: Option<f64> = None;
    let mut min_notional: Option<f64> = None;

    for f in sym.filters {
        match f {
            Filter::LOT_SIZE { step_size: ss, .. } => step_size = Some(ss.parse()?),
            Filter::PRICE_FILTER { tick_size: ts, .. } => tick_size = Some(ts.parse()?),
            Filter::MIN_NOTIONAL { min_notional: mn } => min_notional = Some(mn.parse()?),
            Filter::NOTIONAL { min_notional: mn, .. } => min_notional = Some(mn.parse()?),
            _ => {}
        }
    }

    Ok((
        step_size.ok_or_else(|| anyhow!("Missing LOT_SIZE.stepSize"))?,
        tick_size.ok_or_else(|| anyhow!("Missing PRICE_FILTER.tickSize"))?,
        min_notional.ok_or_else(|| anyhow!("Missing MIN_NOTIONAL/NOTIONAL.minNotional"))?,
    ))
}

