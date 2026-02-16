use anyhow::{anyhow, Result};
use reqwest::{Client, RequestBuilder, Response, StatusCode};
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct HttpPolicy {
    pub timeout: Duration,
    pub max_retries: usize,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
}

impl Default for HttpPolicy {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(10),
            max_retries: 3,
            initial_backoff: Duration::from_millis(250),
            max_backoff: Duration::from_secs(5),
        }
    }
}

impl HttpPolicy {
    pub fn from_env() -> Self {
        let mut p = Self::default();

        if let Ok(v) = std::env::var("BOT_HTTP_TIMEOUT_SECS") {
            if let Ok(secs) = v.trim().parse::<u64>() {
                if secs > 0 {
                    p.timeout = Duration::from_secs(secs);
                }
            }
        }
        let retries = std::env::var("BOT_HTTP_MAX_RETRIES")
            .ok()
            .or_else(|| std::env::var("BOT_HTTP_RETRIES").ok());
        if let Some(v) = retries {
            if let Ok(n) = v.trim().parse::<usize>() {
                p.max_retries = n;
            }
        }
        if let Ok(v) = std::env::var("BOT_HTTP_BACKOFF_MS") {
            if let Ok(ms) = v.trim().parse::<u64>() {
                p.initial_backoff = Duration::from_millis(ms);
            }
        }
        if let Ok(v) = std::env::var("BOT_HTTP_BACKOFF_MAX_MS") {
            if let Ok(ms) = v.trim().parse::<u64>() {
                p.max_backoff = Duration::from_millis(ms);
            }
        }

        p
    }
}

fn is_retryable_status(status: StatusCode) -> bool {
    // Conservative: retry only on explicit rate limit and server errors.
    status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
}

fn is_retryable_error(e: &reqwest::Error) -> bool {
    e.is_timeout() || e.is_connect() || e.is_request()
}

fn backoff_for_attempt(policy: &HttpPolicy, attempt: usize) -> Duration {
    // Exponential backoff, clamped.
    let base_ms = policy.initial_backoff.as_millis() as u64;
    let ms = base_ms.saturating_mul(1u64.saturating_shl(attempt.min(30) as u32));
    Duration::from_millis(ms.min(policy.max_backoff.as_millis() as u64))
}

pub async fn send_with_retry(
    _client: &Client,
    policy: &HttpPolicy,
    mut build: impl FnMut() -> RequestBuilder,
) -> Result<Response> {
    let mut last_err: Option<anyhow::Error> = None;

    for attempt in 0..=policy.max_retries {
        let req = build().timeout(policy.timeout);
        match req.send().await {
            Ok(resp) => {
                if is_retryable_status(resp.status()) && attempt < policy.max_retries {
                    let d = backoff_for_attempt(policy, attempt);
                    tokio::time::sleep(d).await;
                    continue;
                }
                return Ok(resp);
            }
            Err(e) => {
                if is_retryable_error(&e) && attempt < policy.max_retries {
                    last_err = Some(e.into());
                    let d = backoff_for_attempt(policy, attempt);
                    tokio::time::sleep(d).await;
                    continue;
                }
                return Err(e.into());
            }
        }
    }

    Err(anyhow!(
        "HTTP request failed after retries: {}",
        last_err
            .map(|e| e.to_string())
            .unwrap_or_else(|| "unknown error".to_string())
    ))
}
