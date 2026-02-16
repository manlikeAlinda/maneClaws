use anyhow::{anyhow, Result};
use reqwest::Client;
use serde::Deserialize;

use crate::binance_auth::{ensure_time_synced, now_ms_with_offset, sign_hmac_sha256_hex};
use crate::http_policy::{send_with_retry, HttpPolicy};

pub async fn test_order(
    client: &Client,
    api_key: &str,
    api_secret: &str,
    base: &str,
    query: &str,
) -> Result<()> {
    ensure_time_synced(client, base).await?;
    let policy = HttpPolicy::from_env();
    let resp = send_with_retry(client, &policy, || {
        let ts = now_ms_with_offset();
        let full_query = format!("{query}&timestamp={ts}");
        let sig = sign_hmac_sha256_hex(api_secret, &full_query);
        let url = format!("{base}/api/v3/order/test?{full_query}&signature={sig}");
        client.post(url).header("X-MBX-APIKEY", api_key)
    })
    .await?;

    let status = resp.status();
    let text = resp.text().await?;

    if !status.is_success() {
        return Err(anyhow!("Order test failed: status={status}, body={text}"));
    }

    Ok(())
}

#[derive(Debug, Deserialize)]
pub struct OrderAck {
    pub symbol: String,
    #[serde(rename = "orderId")]
    pub order_id: u64,
    pub status: String,
}

pub async fn place_order(
    client: &Client,
    api_key: &str,
    api_secret: &str,
    base: &str,
    query: &str,
) -> Result<OrderAck> {
    ensure_time_synced(client, base).await?;
    let policy = HttpPolicy::from_env();
    let resp = send_with_retry(client, &policy, || {
        let ts = now_ms_with_offset();
        let full_query = format!("{query}&timestamp={ts}");
        let sig = sign_hmac_sha256_hex(api_secret, &full_query);
        let url = format!("{base}/api/v3/order?{full_query}&signature={sig}");
        client.post(url).header("X-MBX-APIKEY", api_key)
    })
    .await?;

    let status = resp.status();
    let text = resp.text().await?;

    if !status.is_success() {
        return Err(anyhow!("REAL order failed: status={status}, body={text}"));
    }

    Ok(serde_json::from_str::<OrderAck>(&text)?)
}

#[derive(Debug, Deserialize, Clone)]
pub struct OrderStatus {
    pub symbol: String,
    #[serde(rename = "orderId")]
    pub order_id: u64,
    pub status: String,
    #[serde(rename = "executedQty")]
    pub executed_qty: String,
    #[serde(rename = "cummulativeQuoteQty")]
    pub cummulative_quote_qty: String,
    // Optional fields (present on many endpoints) used for stop-loss reconciliation.
    pub price: Option<String>,
    #[serde(rename = "stopPrice")]
    pub stop_price: Option<String>,
    pub side: Option<String>,
    #[serde(rename = "type")]
    pub order_type: Option<String>,
}

pub async fn get_order(
    client: &Client,
    api_key: &str,
    api_secret: &str,
    base: &str,
    symbol: &str,
    order_id: u64,
) -> Result<OrderStatus> {
    ensure_time_synced(client, base).await?;
    let policy = HttpPolicy::from_env();
    let resp = send_with_retry(client, &policy, || {
        let ts = now_ms_with_offset();
        let query = format!("symbol={symbol}&orderId={order_id}&timestamp={ts}");
        let sig = sign_hmac_sha256_hex(api_secret, &query);
        let url = format!("{base}/api/v3/order?{query}&signature={sig}");
        client.get(url).header("X-MBX-APIKEY", api_key)
    })
    .await?;

    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        return Err(anyhow!("Order status failed: status={status}, body={text}"));
    }

    Ok(serde_json::from_str::<OrderStatus>(&text)?)
}

pub async fn cancel_order(
    client: &Client,
    api_key: &str,
    api_secret: &str,
    base: &str,
    symbol: &str,
    order_id: u64,
) -> Result<OrderStatus> {
    ensure_time_synced(client, base).await?;
    let policy = HttpPolicy::from_env();
    let resp = send_with_retry(client, &policy, || {
        let ts = now_ms_with_offset();
        let query = format!("symbol={symbol}&orderId={order_id}&timestamp={ts}");
        let sig = sign_hmac_sha256_hex(api_secret, &query);
        let url = format!("{base}/api/v3/order?{query}&signature={sig}");
        client.delete(url).header("X-MBX-APIKEY", api_key)
    })
    .await?;

    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        return Err(anyhow!("Order cancel failed: status={status}, body={text}"));
    }

    Ok(serde_json::from_str::<OrderStatus>(&text)?)
}

#[derive(Debug, Deserialize, Clone)]
pub struct MyTrade {
    #[serde(rename = "orderId")]
    pub order_id: u64,
    pub commission: String,
    #[serde(rename = "commissionAsset")]
    pub commission_asset: String,
}

pub async fn my_trades_for_order(
    client: &Client,
    api_key: &str,
    api_secret: &str,
    base: &str,
    symbol: &str,
    order_id: u64,
) -> Result<Vec<MyTrade>> {
    ensure_time_synced(client, base).await?;
    let policy = HttpPolicy::from_env();
    let resp = send_with_retry(client, &policy, || {
        let ts = now_ms_with_offset();
        // myTrades supports filtering by orderId.
        let query = format!("symbol={symbol}&orderId={order_id}&timestamp={ts}");
        let sig = sign_hmac_sha256_hex(api_secret, &query);
        let url = format!("{base}/api/v3/myTrades?{query}&signature={sig}");
        client.get(url).header("X-MBX-APIKEY", api_key)
    })
    .await?;

    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        return Err(anyhow!("myTrades failed: status={status}, body={text}"));
    }

    Ok(serde_json::from_str::<Vec<MyTrade>>(&text)?)
}
