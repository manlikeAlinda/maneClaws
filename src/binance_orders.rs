use anyhow::{anyhow, Result};
use reqwest::Client;
use serde::Deserialize;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::binance_auth::{ensure_time_synced, now_ms_with_offset, sign_hmac_sha256_hex};
use crate::http_policy::{send_with_retry, HttpPolicy};

static CLIENT_ORDER_SEQ: AtomicU64 = AtomicU64::new(0);

/// A fresh, unique id embedded as `newClientOrderId` on every order placement,
/// so a retried or ambiguously-failed placement can be reconciled against
/// Binance by this id instead of blindly resubmitting (which would risk a
/// real double order if the first attempt actually succeeded but its response
/// was lost).
fn generate_client_order_id() -> String {
    let seq = CLIENT_ORDER_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("bot{}-{}", crate::state::now_ms(), seq)
}

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
    #[serde(default)]
    pub status: Option<String>,
}

pub async fn place_order(
    client: &Client,
    api_key: &str,
    api_secret: &str,
    base: &str,
    symbol: &str,
    query: &str,
) -> Result<OrderAck> {
    let client_order_id = generate_client_order_id();
    let query_with_id = format!("{query}&newClientOrderId={client_order_id}");

    ensure_time_synced(client, base).await?;
    let policy = HttpPolicy::from_env();
    let send_result = send_with_retry(client, &policy, || {
        let ts = now_ms_with_offset();
        let full_query = format!("{query_with_id}&timestamp={ts}");
        let sig = sign_hmac_sha256_hex(api_secret, &full_query);
        let url = format!("{base}/api/v3/order?{full_query}&signature={sig}");
        client.post(url).header("X-MBX-APIKEY", api_key)
    })
    .await;

    let placement_err = match send_result {
        Ok(resp) => {
            let status = resp.status();
            let text = resp.text().await?;
            if status.is_success() {
                return Ok(serde_json::from_str::<OrderAck>(&text)?);
            }
            anyhow!("REAL order failed: status={status}, body={text}")
        }
        Err(e) => e,
    };

    // The placement attempt failed or errored, but we don't know whether Binance
    // actually received and executed it before the failure (e.g. a timeout after
    // the request reached Binance). Check by client order id before giving up,
    // so we don't report failure - and let a caller retry - on an order that was
    // actually placed.
    match get_order_by_client_id(client, api_key, api_secret, base, symbol, &client_order_id).await
    {
        Ok(found) => {
            tracing::warn!(
                "Order placement appeared to fail ({}), but order_id={} (clientOrderId={}) was \
                 found on the exchange - treating as placed, not retrying.",
                placement_err,
                found.order_id,
                client_order_id
            );
            Ok(OrderAck {
                symbol: found.symbol,
                order_id: found.order_id,
                status: Some(found.status),
            })
        }
        Err(_) => Err(placement_err),
    }
}

/// Look up an order by the `newClientOrderId` it was placed with, rather than
/// Binance's own `orderId` - used to reconcile an ambiguous placement failure.
pub async fn get_order_by_client_id(
    client: &Client,
    api_key: &str,
    api_secret: &str,
    base: &str,
    symbol: &str,
    client_order_id: &str,
) -> Result<OrderStatus> {
    ensure_time_synced(client, base).await?;
    let policy = HttpPolicy::from_env();
    let resp = send_with_retry(client, &policy, || {
        let ts = now_ms_with_offset();
        let query = format!("symbol={symbol}&origClientOrderId={client_order_id}&timestamp={ts}");
        let sig = sign_hmac_sha256_hex(api_secret, &query);
        let url = format!("{base}/api/v3/order?{query}&signature={sig}");
        client.get(url).header("X-MBX-APIKEY", api_key)
    })
    .await?;

    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        return Err(anyhow!("Order lookup by client id failed: status={status}, body={text}"));
    }

    Ok(serde_json::from_str::<OrderStatus>(&text)?)
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
pub struct OpenOrder {
    pub symbol: Option<String>,
    #[serde(rename = "orderId")]
    pub order_id: u64,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(rename = "type")]
    #[serde(default)]
    pub order_type: Option<String>,
    #[serde(default)]
    pub side: Option<String>,
    #[serde(rename = "stopPrice")]
    #[serde(default)]
    pub stop_price: Option<String>,
    #[serde(rename = "origQty")]
    #[serde(default)]
    pub orig_qty: Option<String>,
}

pub async fn open_orders(
    client: &Client,
    api_key: &str,
    api_secret: &str,
    base: &str,
    symbol: &str,
) -> Result<Vec<OpenOrder>> {
    ensure_time_synced(client, base).await?;
    let policy = HttpPolicy::from_env();
    let resp = send_with_retry(client, &policy, || {
        let ts = now_ms_with_offset();
        let query = format!("symbol={symbol}&timestamp={ts}");
        let sig = sign_hmac_sha256_hex(api_secret, &query);
        let url = format!("{base}/api/v3/openOrders?{query}&signature={sig}");
        client.get(url).header("X-MBX-APIKEY", api_key)
    })
    .await?;

    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        return Err(anyhow!(
            "openOrders failed: status={status}, body={}",
            text.chars().take(500).collect::<String>()
        ));
    }

    Ok(serde_json::from_str::<Vec<OpenOrder>>(&text)?)
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
