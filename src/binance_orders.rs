use anyhow::{anyhow, Result};
use hmac::{Hmac, Mac};
use reqwest::Client;
use sha2::Sha256;
use std::time::{SystemTime, UNIX_EPOCH};
use serde::Deserialize;

type HmacSha256 = Hmac<Sha256>;

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time went backwards")
        .as_millis() as u64
}

fn sign(secret: &str, payload: &str) -> String {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC can take key of any size");
    mac.update(payload.as_bytes());
    let sig = mac.finalize().into_bytes();
    hex::encode(sig)
}

pub async fn test_order(
    client: &Client,
    api_key: &str,
    api_secret: &str,
    base: &str,
    query: &str,
) -> Result<()> {
    let ts = now_ms();
    let full_query = format!("{query}&timestamp={ts}");
    let sig = sign(api_secret, &full_query);
    let url = format!("{base}/api/v3/order/test?{full_query}&signature={sig}");

    let resp = client
        .post(url)
        .header("X-MBX-APIKEY", api_key)
        .send()
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
    pub orderId: u64,
    pub status: String,
}

pub async fn place_order(
    client: &Client,
    api_key: &str,
    api_secret: &str,
    base: &str,
    query: &str,
) -> Result<OrderAck> {
    let ts = now_ms();
    let full_query = format!("{query}&timestamp={ts}");
    let sig = sign(api_secret, &full_query);
    let url = format!("{base}/api/v3/order?{full_query}&signature={sig}");

    let resp = client
        .post(url)
        .header("X-MBX-APIKEY", api_key)
        .send()
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
    pub orderId: u64,
    pub status: String,
    #[serde(rename = "executedQty")]
    pub executed_qty: String,
    #[serde(rename = "cummulativeQuoteQty")]
    pub cummulative_quote_qty: String,
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
    let ts = now_ms();
    let query = format!("symbol={symbol}&orderId={order_id}&timestamp={ts}");
    let sig = sign(api_secret, &query);
    let url = format!("{base}/api/v3/order?{query}&signature={sig}");

    let resp = client
        .get(url)
        .header("X-MBX-APIKEY", api_key)
        .send()
        .await?;

    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        return Err(anyhow!("Order status failed: status={status}, body={text}"));
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
    let ts = now_ms();
    // myTrades supports filtering by orderId.
    let query = format!("symbol={symbol}&orderId={order_id}&timestamp={ts}");
    let sig = sign(api_secret, &query);
    let url = format!("{base}/api/v3/myTrades?{query}&signature={sig}");

    let resp = client
        .get(url)
        .header("X-MBX-APIKEY", api_key)
        .send()
        .await?;

    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        return Err(anyhow!("myTrades failed: status={status}, body={text}"));
    }

    Ok(serde_json::from_str::<Vec<MyTrade>>(&text)?)
}
