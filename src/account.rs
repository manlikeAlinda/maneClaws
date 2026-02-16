use anyhow::{anyhow, Result};
use reqwest::Client;
use serde::Deserialize;
use tracing::info;

use crate::binance_auth;
use crate::http_policy::{send_with_retry, HttpPolicy};

#[derive(Debug, Deserialize)]
struct AccountInfo {
    balances: Vec<Balance>,
}

#[derive(Debug, Deserialize)]
struct Balance {
    asset: String,
    free: String,
    #[allow(dead_code)]
    locked: String,
}

#[derive(Debug, Clone)]
pub struct SpotBalances {
    pub usdt_free: f64,
    pub btc_free: f64,
}

#[derive(Debug, Deserialize, Clone)]
struct BinanceErrorPayload {
    code: i64,
    msg: String,
}

#[derive(Debug)]
pub struct BinanceApiError {
    pub status: u16,
    pub code: Option<i64>,
    pub msg: Option<String>,
    pub body_trunc: String,
}

impl std::fmt::Display for BinanceApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Binance API error: status={} code={:?} msg={:?} body_trunc={}",
            self.status, self.code, self.msg, self.body_trunc
        )
    }
}

impl std::error::Error for BinanceApiError {}

pub async fn fetch_spot_balances(
    client: &Client,
    api_key: &str,
    api_secret: &str,
    base: &str,
) -> Result<SpotBalances> {
    fn sign(secret: &str, payload: &str) -> String {
        binance_auth::sign_hmac_sha256_hex(secret, payload)
    }

    // Signed request instrumentation (no secrets): endpoint + timestamps + query/sig lengths.
    let utc = time::OffsetDateTime::now_utc();
    let local_now_ms = binance_auth::now_ms();
    let offset = binance_auth::time_offset_ms();
    let ts = binance_auth::now_ms_with_offset();
    let recv_window = 5000_u64;
    let query = format!("recvWindow={recv_window}&timestamp={ts}");
    let sig = sign(api_secret, &query);
    let url = format!("{base}/api/v3/account?{query}&signature={sig}");

    info!(
        "Signed request: GET /api/v3/account (base={}) key_present={} key_len={} secret_present={} secret_len={}",
        base,
        !api_key.is_empty(),
        api_key.len(),
        !api_secret.is_empty(),
        api_secret.len()
    );
    info!(
        "Signing details: utc={} local_now_ms={} offset_ms={} timestamp_ms={} query_len={} signature_len={}",
        utc,
        local_now_ms,
        offset,
        ts,
        query.len(),
        sig.len()
    );
    info!("Query (no signature): {}", query);

    let policy = HttpPolicy::from_env();
    let resp = send_with_retry(client, &policy, || client.get(&url).header("X-MBX-APIKEY", api_key)).await?;

    let status = resp.status();
    let text = resp.text().await?;
    let body_trunc: String = text.chars().take(500).collect();

    if !status.is_success() {
        let parsed = serde_json::from_str::<BinanceErrorPayload>(&text).ok();
        let code = parsed.as_ref().map(|p| p.code);
        let msg = parsed.as_ref().map(|p| p.msg.clone());

        info!(
            "Account response: http_status={} binance_code={:?} binance_msg={:?} body_trunc={}",
            status.as_u16(),
            code,
            msg,
            body_trunc
        );

        // Time sync fallback only when Binance tells us our timestamp is wrong.
        if code == Some(-1021) {
            let local_ms = local_now_ms as i64;
            let server_ms = binance_auth::server_time_ms(client, base).await? as i64;
            let new_offset = server_ms.saturating_sub(local_ms);
            binance_auth::set_time_offset_ms(new_offset);
            info!(
                "Clock difference detected: local={} server={} offset_ms={} (positive means local is behind)",
                local_ms,
                server_ms,
                new_offset
            );

            // Retry once with corrected timestamp.
            let ts2 = binance_auth::now_ms_with_offset();
            let query2 = format!("recvWindow={recv_window}&timestamp={ts2}");
            let sig2 = sign(api_secret, &query2);
            let url2 = format!("{base}/api/v3/account?{query2}&signature={sig2}");
            info!(
                "Retry signed request after time sync: timestamp_ms={} query_len={} signature_len={}",
                ts2,
                query2.len(),
                sig2.len()
            );

            let resp2 = send_with_retry(client, &policy, || {
                client.get(&url2).header("X-MBX-APIKEY", api_key)
            })
            .await?;
            let status2 = resp2.status();
            let text2 = resp2.text().await?;
            let body_trunc2: String = text2.chars().take(500).collect();
            if !status2.is_success() {
                let parsed2 = serde_json::from_str::<BinanceErrorPayload>(&text2).ok();
                let code2 = parsed2.as_ref().map(|p| p.code);
                let msg2 = parsed2.as_ref().map(|p| p.msg.clone());
                info!(
                    "Account retry response: http_status={} binance_code={:?} binance_msg={:?} body_trunc={}",
                    status2.as_u16(),
                    code2,
                    msg2,
                    body_trunc2
                );
                return Err(anyhow!(BinanceApiError {
                    status: status2.as_u16(),
                    code: code2,
                    msg: msg2,
                    body_trunc: body_trunc2,
                }));
            }

            // Success on retry.
            let acct: AccountInfo = serde_json::from_str(&text2)?;
            let mut usdt_free = 0.0_f64;
            let mut btc_free = 0.0_f64;
            for b in acct.balances {
                match b.asset.as_str() {
                    "USDT" => usdt_free = b.free.parse()?,
                    "BTC" => btc_free = b.free.parse()?,
                    _ => {}
                }
            }
            return Ok(SpotBalances { usdt_free, btc_free });
        }

        return Err(anyhow!(BinanceApiError {
            status: status.as_u16(),
            code,
            msg,
            body_trunc,
        }));
    }

    let acct: AccountInfo = serde_json::from_str(&text)?;

    let mut usdt_free = 0.0_f64;
    let mut btc_free = 0.0_f64;

    for b in acct.balances {
        match b.asset.as_str() {
            "USDT" => usdt_free = b.free.parse()?,
            "BTC" => btc_free = b.free.parse()?,
            _ => {}
        }
    }

    Ok(SpotBalances { usdt_free, btc_free })
}
