#![allow(clippy::collapsible_if)]

use anyhow::{anyhow, Result};
use reqwest::{Client, RequestBuilder, Response, StatusCode};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

static REQUEST_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub struct HttpPolicy {
    pub timeout: Duration,
    pub max_retries: usize,
    pub initial_backoff: Duration,
    pub max_backoff: Duration,
    pub jitter_pct: f64,
    pub jitter_seed: u64,
}

impl Default for HttpPolicy {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(10),
            max_retries: 3,
            initial_backoff: Duration::from_millis(250),
            max_backoff: Duration::from_secs(5),
            jitter_pct: 0.20,
            jitter_seed: 0,
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

        if let Ok(v) = std::env::var("BOT_HTTP_JITTER_PCT")
            .or_else(|_| std::env::var("BOT_HTTP_JITTER_FRAC"))
        {
            if let Ok(x) = v.trim().parse::<f64>() {
                if x.is_finite() && x >= 0.0 {
                    p.jitter_pct = x;
                }
            }
        }

        if let Ok(v) = std::env::var("BOT_HTTP_JITTER_SEED") {
            if let Ok(seed) = v.trim().parse::<u64>() {
                p.jitter_seed = seed;
            }
        } else {
            // Non-deterministic by default, but stable within a given `from_env()` call.
            p.jitter_seed = (crate::state::now_ms() << 1) ^ (std::process::id() as u64);
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
    let shift = attempt.min(30) as u32;
    let factor = 1u64.checked_shl(shift).unwrap_or(u64::MAX);
    let ms = base_ms.saturating_mul(factor);
    Duration::from_millis(ms.min(policy.max_backoff.as_millis() as u64))
}

fn parse_retry_after(resp: &Response) -> Option<Duration> {
    // Retry-After is either seconds or an HTTP-date. We only support seconds to avoid extra deps.
    let v = resp.headers().get(reqwest::header::RETRY_AFTER)?;
    let s = v.to_str().ok()?.trim();
    let secs = s.parse::<u64>().ok()?;
    Some(Duration::from_secs(secs))
}

fn splitmix64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E3779B97F4A7C15);
    let mut z = x;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

fn jitter_add(policy: &HttpPolicy, request_id: u64, attempt: usize, base: Duration) -> Duration {
    if policy.jitter_pct <= 0.0 {
        return base;
    }
    let base_ms = base.as_millis() as u64;
    if base_ms == 0 {
        return base;
    }

    let max_jitter_ms = ((base_ms as f64) * policy.jitter_pct).round() as u64;
    if max_jitter_ms == 0 {
        return base;
    }

    let x = policy.jitter_seed ^ request_id ^ (attempt as u64).wrapping_mul(0xD6E8FEB86659FD93);
    let r = splitmix64(x);
    let jitter_ms = r % (max_jitter_ms + 1);
    Duration::from_millis(base_ms.saturating_add(jitter_ms))
}

pub async fn send_with_retry(
    _client: &Client,
    policy: &HttpPolicy,
    mut build: impl FnMut() -> RequestBuilder,
) -> Result<Response> {
    let mut last_err: Option<anyhow::Error> = None;
    let request_id = REQUEST_SEQ.fetch_add(1, Ordering::Relaxed);

    for attempt in 0..=policy.max_retries {
        let req = build().timeout(policy.timeout);
        match req.send().await {
            Ok(resp) => {
                if is_retryable_status(resp.status()) && attempt < policy.max_retries {
                    let mut d = backoff_for_attempt(policy, attempt);

                    if resp.status() == StatusCode::TOO_MANY_REQUESTS {
                        if let Some(ra) = parse_retry_after(&resp) {
                            d = d.max(ra);
                        }
                    }

                    let d = jitter_add(policy, request_id, attempt, d);
                    tokio::time::sleep(d).await;
                    continue;
                }
                return Ok(resp);
            }
            Err(e) => {
                if is_retryable_error(&e) && attempt < policy.max_retries {
                    last_err = Some(e.into());
                    let d = jitter_add(policy, request_id, attempt, backoff_for_attempt(policy, attempt));
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

pub async fn send_text_with_retry(
    _client: &Client,
    policy: &HttpPolicy,
    context: &str,
    mut build: impl FnMut() -> RequestBuilder,
) -> Result<(StatusCode, String)> {
    let mut last_err: Option<anyhow::Error> = None;
    let request_id = REQUEST_SEQ.fetch_add(1, Ordering::Relaxed);

    for attempt in 0..=policy.max_retries {
        let req = build().timeout(policy.timeout);
        match req.send().await {
            Ok(resp) => {
                let status = resp.status();
                let retry_after = if status == StatusCode::TOO_MANY_REQUESTS {
                    parse_retry_after(&resp)
                } else {
                    None
                };

                let text = resp.text().await.unwrap_or_default();

                // If the server claims success but returns an empty body, treat as transient.
                if status.is_success() && text.trim().is_empty() {
                    if attempt < policy.max_retries {
                        last_err = Some(anyhow!("{context}: HTTP {status} returned empty body"));
                        let d = jitter_add(policy, request_id, attempt, backoff_for_attempt(policy, attempt));
                        tokio::time::sleep(d).await;
                        continue;
                    }
                    return Err(anyhow!("{context}: HTTP {status} returned empty body after retries"));
                }

                if is_retryable_status(status) && attempt < policy.max_retries {
                    let mut d = backoff_for_attempt(policy, attempt);
                    if let Some(ra) = retry_after {
                        d = d.max(ra);
                    }
                    let d = jitter_add(policy, request_id, attempt, d);
                    tokio::time::sleep(d).await;
                    continue;
                }

                return Ok((status, text));
            }
            Err(e) => {
                if is_retryable_error(&e) && attempt < policy.max_retries {
                    last_err = Some(e.into());
                    let d = jitter_add(policy, request_id, attempt, backoff_for_attempt(policy, attempt));
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jitter_is_deterministic_given_seed() {
        let mut p = HttpPolicy::default();
        p.jitter_seed = 123;
        p.jitter_pct = 0.50;
        let base = Duration::from_millis(1000);

        let a = jitter_add(&p, 1, 0, base);
        let b = jitter_add(&p, 1, 0, base);
        assert_eq!(a, b);

        let c = jitter_add(&p, 1, 1, base);
        assert_ne!(a, c);
    }
}
