use anyhow::{anyhow, Result};
use reqwest::Client;
use serde::Deserialize;

use crate::http_policy::{send_with_retry, HttpPolicy};

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
#[allow(non_camel_case_types)]
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

pub async fn fetch_exchange_info_from_base(client: &Client, base_url: &str) -> Result<ExchangeInfo> {
    let url = format!("{base_url}/api/v3/exchangeInfo");

    let policy = HttpPolicy::from_env();
    let resp = send_with_retry(client, &policy, || client.get(&url)).await?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();

    if !status.is_success() {
        return Err(anyhow!("exchangeInfo returned {status}: {text}"));
    }

    Ok(serde_json::from_str::<ExchangeInfo>(&text)?)
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

