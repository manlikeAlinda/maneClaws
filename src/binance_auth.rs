use anyhow::{anyhow, Result};
use hmac::{Hmac, Mac};
use reqwest::Client;
use serde::Deserialize;
use sha2::Sha256;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::http_policy::{send_with_retry, HttpPolicy};

type HmacSha256 = Hmac<Sha256>;

static TIME_OFFSET_MS: AtomicI64 = AtomicI64::new(0);

pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("time went backwards")
        .as_millis() as u64
}

pub fn now_ms_with_offset() -> u64 {
    let local = now_ms() as i64;
    let off = TIME_OFFSET_MS.load(Ordering::Relaxed);
    let adjusted = local.saturating_add(off);
    adjusted.max(0) as u64
}

pub fn time_offset_ms() -> i64 {
    TIME_OFFSET_MS.load(Ordering::Relaxed)
}

pub fn set_time_offset_ms(offset_ms: i64) {
    TIME_OFFSET_MS.store(offset_ms, Ordering::Relaxed);
}

pub fn sign_hmac_sha256_hex(secret: &str, payload: &str) -> String {
    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).expect("HMAC can take key of any size");
    mac.update(payload.as_bytes());
    let sig = mac.finalize().into_bytes();
    hex::encode(sig)
}

pub async fn server_time_ms(client: &Client, base: &str) -> Result<u64> {
    #[derive(Debug, Deserialize)]
    struct TimeResp {
        #[serde(rename = "serverTime")]
        server_time: u64,
    }

    let url = format!("{base}/api/v3/time");
    let policy = HttpPolicy::from_env();
    let resp = send_with_retry(client, &policy, || client.get(&url)).await?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(anyhow!("Binance time failed: status={status}, body={text}"));
    }

    let parsed: TimeResp = serde_json::from_str(&text)?;
    Ok(parsed.server_time)
}

pub async fn sync_time_offset_ms(client: &Client, base: &str) -> Result<i64> {
    let server_ms = server_time_ms(client, base).await? as i64;
    let local_ms = now_ms() as i64;
    let new_offset = server_ms.saturating_sub(local_ms);
    set_time_offset_ms(new_offset);
    Ok(new_offset)
}
