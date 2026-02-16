use anyhow::{anyhow, Result};
use reqwest::Client;
use std::env;
use std::path::Path;
use std::time::Duration;
use tracing::{debug, error, info};
use tracing_subscriber::EnvFilter;

use binance_survival_bot::paths;

fn env_present_len(name: &str) -> (bool, usize) {
    match env::var(name) {
        Ok(v) => (!v.is_empty(), v.len()),
        Err(_) => (false, 0),
    }
}

fn load_trimmed_env(name: &str, allow_missing: bool) -> Result<(String, usize, usize, bool)> {
    match env::var(name) {
        Ok(raw) => {
            let before = raw.len();
            let trimmed = raw.trim().to_string();
            let after = trimmed.len();
            let whitespace_removed = raw != trimmed;
            Ok((trimmed, before, after, whitespace_removed))
        }
        Err(_) if allow_missing => Ok((String::new(), 0, 0, false)),
        Err(_) => Err(anyhow!("Missing {name}")),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    // Simple log level switch:
    // - BOT_LOG_LEVEL=debug -> show debug details
    // - anything else / missing -> IMPORTANT only (info)
    let bot_level = env::var("BOT_LOG_LEVEL")
        .ok()
        .unwrap_or_else(|| "important".to_string());

    // IMPORTANT mode: show only the grandmother-friendly lines plus warnings/errors.
    // DEBUG mode: show our crate debug, but keep noisy deps quiet.
    let filter_str = if bot_level.eq_ignore_ascii_case("debug") {
        "binance_survival_bot=debug,reqwest=warn,hyper=warn,hyper_util=warn"
    } else {
        "binance_survival_bot::log_say=info,binance_survival_bot=warn,reqwest=warn,hyper=warn,hyper_util=warn"
    };

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(filter_str))
        .init();

    let rp = paths::resolve_paths()?;
    info!("Base dir: {}", rp.base_dir.display());
    info!("State path: {}", rp.state_path.display());
    info!("Cache dir: {}", rp.data_dir.display());
    info!("Lock path: {}", rp.lock_path.display());

    // Single-instance guard (P0 safety): prevent two bots from trading at once.
    let _instance_lock = match binance_survival_bot::single_instance::SingleInstanceLock::acquire(
        &rp.lock_path,
    ) {
        Ok(l) => {
            debug!("Single-instance lock acquired: {}", rp.lock_path.display());
            l
        }
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("lock busy") {
                info!(
                    "Another bot instance is already running (lock busy). Exiting cleanly."
                );
                return Ok(());
            }
            return Err(e);
        }
    };

    let (key_before_present, _key_before_len) = env_present_len("BINANCE_API_KEY");
    let (sec_before_present, _sec_before_len) = env_present_len("BINANCE_API_SECRET");

    let env_file_exists = Path::new(".env").exists();
    let dotenv_result = dotenvy::dotenv();
    let dotenv_loaded = dotenv_result.is_ok();
    let dotenv_path = dotenv_result.ok();

    let (key_after_present, key_after_len) = env_present_len("BINANCE_API_KEY");
    let (sec_after_present, sec_after_len) = env_present_len("BINANCE_API_SECRET");

    debug!(
        "Env source: dotenv loaded = {}, .env exists = {}, dotenv path = {}",
        dotenv_loaded,
        env_file_exists,
        dotenv_path
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "(none)".to_string())
    );

    let key_source = if key_before_present {
        "process_env"
    } else if key_after_present {
        "dotenv"
    } else {
        "missing"
    };
    let sec_source = if sec_before_present {
        "process_env"
    } else if sec_after_present {
        "dotenv"
    } else {
        "missing"
    };

    debug!(
        "BINANCE_API_KEY present = {}, raw_length = {}, source = {}",
        key_after_present,
        key_after_len,
        key_source
    );
    debug!(
        "BINANCE_API_SECRET present = {}, raw_length = {}, source = {}",
        sec_after_present,
        sec_after_len,
        sec_source
    );

    let mode = binance_survival_bot::execution::mode_from_env();

    let allow_missing = mode == binance_survival_bot::execution::Mode::Practice;
    let (api_key, key_len_before, key_len_after, key_ws_removed) =
        load_trimmed_env("BINANCE_API_KEY", allow_missing)?;
    let (api_secret, sec_len_before, sec_len_after, sec_ws_removed) =
        load_trimmed_env("BINANCE_API_SECRET", allow_missing)?;
    let any_ws_removed = key_ws_removed || sec_ws_removed;

    debug!(
        "Env trim diagnostics: key_len_before={} key_len_after={} secret_len_before={} secret_len_after={} whitespace_removed_any={} key_whitespace_removed={} secret_whitespace_removed={}",
        key_len_before,
        key_len_after,
        sec_len_before,
        sec_len_after,
        any_ws_removed,
        key_ws_removed,
        sec_ws_removed
    );

    let client = Client::new();

    let cfg = binance_survival_bot::app::AppConfig {
        base_url: "https://api.binance.com".to_string(),
        symbol: "BTCUSDT".to_string(),
        state_path: rp.state_path.display().to_string(),
        data_dir: rp.data_dir.display().to_string(),
        candle_cache_max_age: Duration::from_secs(60),
        api_key,
        api_secret,
    };

    let args: Vec<String> = std::env::args().collect();
    let loop_mode = args.iter().any(|a| a == "--loop")
        || matches!(std::env::var("BOT_LOOP").ok().as_deref(), Some("1"));

    if !loop_mode {
        binance_survival_bot::app::run_once(&client, &cfg).await?;
        return Ok(());
    }

    let sleep_secs = std::env::var("BOT_LOOP_SLEEP_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(300);
    let duration_secs = std::env::var("BOT_LOOP_DURATION_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(60 * 60);
    let stop_on_error = matches!(std::env::var("BOT_LOOP_STOP_ON_ERROR").ok().as_deref(), Some("1"));

    info!(
        "Loop mode enabled. Every {}s for {}s.",
        sleep_secs, duration_secs
    );
    let mut i: u64 = 0;
    let start = std::time::Instant::now();
    loop {
        if start.elapsed() >= Duration::from_secs(duration_secs) {
            info!("Loop finished.");
            return Ok(());
        }
        i = i.saturating_add(1);
        info!("Heartbeat: run #{}", i);
        match binance_survival_bot::app::run_once(&client, &cfg).await {
            Ok(_out) => {}
            Err(e) => {
                error!("Run failed: {}", e);
                if stop_on_error {
                    return Err(e);
                }
            }
        }

        tokio::time::sleep(Duration::from_secs(sleep_secs)).await;
    }
}
