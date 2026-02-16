use crate::execution::{self, Mode};
use crate::http_policy::{send_with_retry, HttpPolicy};
use crate::regime::Regime;
use crate::{log_say, pipeline, state};
use anyhow::{anyhow, Result};
use reqwest::Client;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tracing::debug;

static RUN_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub base_url: String,
    pub symbol: String,
    pub state_path: String,
    pub data_dir: String,
    pub candle_cache_max_age: Duration,
    pub api_key: String,
    pub api_secret: String,
}

#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub did_place_order: bool,
    pub mode: Mode,
    pub regime: Regime,
}

struct RunGuard {
    decision: String,
    reason: String,
    end: String,
    mode: Mode,
    emit_decision: bool,
}

impl RunGuard {
    fn new(mode: Mode) -> Self {
        Self {
            decision: "WAIT".to_string(),
            reason: "".to_string(),
            end: "no money moved".to_string(),
            mode,
            emit_decision: false,
        }
    }

    fn enable_decision(&mut self) {
        self.emit_decision = true;
    }

    fn set_decision(&mut self, decision: &str) {
        self.decision = decision.trim().to_string();
    }

    fn set_reason(&mut self, reason: &str) {
        self.reason = reason.trim().to_string();
    }

    fn set_end(&mut self, end: &str) {
        self.end = end.trim().to_string();
    }
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        if self.emit_decision {
            log_say::say_decision_with_reason(self.mode, &self.decision, &self.reason);
        }
        log_say::say_end(&self.end);
    }
}

fn new_run_id() -> String {
    let ms = state::now_ms();
    let seq = RUN_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("run-{ms}-{seq}")
}

async fn ping(client: &Client, base_url: &str) -> Result<()> {
    let url = format!("{base_url}/api/v3/ping");

    let policy = HttpPolicy::from_env();
    let resp = send_with_retry(client, &policy, || client.get(&url)).await?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        return Err(anyhow!("Ping returned {status}: {text}"));
    }

    Ok(())
}

pub async fn run_once(client: &Client, cfg: &AppConfig) -> Result<RunOutcome> {
    let mode = execution::mode_from_env();
    let run_id = new_run_id();
    log_say::say_start(mode, &run_id);

    let mut guard = RunGuard::new(mode);

    // P0: Connectivity gating. If ping fails, do not emit a Decision line.
    match ping(client, &cfg.base_url).await {
        Ok(_) => {
            log_say::say_connected(true);
            guard.enable_decision();
        }
        Err(e) => {
            log_say::say_connected(false);
            log_say::say_action("No network to Binance. I stop this round.");
            guard.set_end("no money moved");
            debug!("Ping failed: {e}");
            return Ok(RunOutcome {
                did_place_order: false,
                mode,
                regime: Regime::Ranging,
            });
        }
    }

    let out = pipeline::run_once_core(client, cfg, &run_id, mode).await?;
    guard.set_decision(&out.decision);
    guard.set_reason(&out.reason);
    guard.set_end(&out.end);

    Ok(RunOutcome {
        did_place_order: out.did_place_order,
        mode: out.mode,
        regime: out.regime,
    })
}
