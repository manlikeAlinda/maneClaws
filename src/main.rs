use anyhow::{anyhow, Result};
use reqwest::Client;
use std::env;
use std::path::Path;
use std::time::Duration;
use tracing::{error, info, Level};
use tracing_subscriber::EnvFilter;

fn env_present_len(name: &str) -> (bool, usize) {
    match env::var(name) {
        Ok(v) => (!v.is_empty(), v.len()),
        Err(_) => (false, 0),
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_max_level(Level::INFO)
        .init();

    let (key_before_present, _key_before_len) = env_present_len("BINANCE_API_KEY");
    let (sec_before_present, _sec_before_len) = env_present_len("BINANCE_API_SECRET");

    let env_file_exists = Path::new(".env").exists();
    let dotenv_result = dotenvy::dotenv();
    let dotenv_loaded = dotenv_result.is_ok();
    let dotenv_path = dotenv_result.ok();

    let (key_after_present, key_after_len) = env_present_len("BINANCE_API_KEY");
    let (sec_after_present, sec_after_len) = env_present_len("BINANCE_API_SECRET");

    info!(
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

    info!(
        "BINANCE_API_KEY present = {}, length = {}, source = {}",
        key_after_present,
        key_after_len,
        key_source
    );
    info!(
        "BINANCE_API_SECRET present = {}, length = {}, source = {}",
        sec_after_present,
        sec_after_len,
        sec_source
    );

    let mode = binance_survival_bot::execution::mode_from_env();

    let api_key = match env::var("BINANCE_API_KEY") {
        Ok(v) => v,
        Err(_) if mode == binance_survival_bot::execution::Mode::Practice => String::new(),
        Err(_) => return Err(anyhow!("Missing BINANCE_API_KEY")),
    };

    let api_secret = match env::var("BINANCE_API_SECRET") {
        Ok(v) => v,
        Err(_) if mode == binance_survival_bot::execution::Mode::Practice => String::new(),
        Err(_) => return Err(anyhow!("Missing BINANCE_API_SECRET")),
    };

    let client = Client::new();

    let cfg = binance_survival_bot::app::AppConfig {
        base_url: "https://api.binance.com".to_string(),
        symbol: "BTCUSDT".to_string(),
        state_path: "bot_state.json".to_string(),
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
    let stop_on_error = matches!(std::env::var("BOT_LOOP_STOP_ON_ERROR").ok().as_deref(), Some("1"));

    info!("Loop mode enabled. Sleeping {} seconds between runs.", sleep_secs);
    let mut i: u64 = 0;
    loop {
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
