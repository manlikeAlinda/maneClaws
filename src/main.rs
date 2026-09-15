use anyhow::{anyhow, Context, Result};
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

fn fingerprint_first_last_4(value: &str) -> String {
    let v = value.trim();
    if v.is_empty() {
        return "(missing)".to_string();
    }

    let bytes = v.as_bytes();
    let first_len = bytes.len().min(4);
    let last_start = bytes.len().saturating_sub(4);
    let first = String::from_utf8_lossy(&bytes[..first_len]);
    let last = String::from_utf8_lossy(&bytes[last_start..]);
    format!("{first}…{last}")
}

fn is_loopback_http_base_url(base_url: &str) -> bool {
    base_url.starts_with("http://127.0.0.1")
        || base_url.starts_with("http://localhost")
        || base_url.starts_with("http://[::1]")
}

fn validate_base_url(base_url: &str) -> Result<()> {
    let allow_insecure = matches!(
        std::env::var("BOT_ALLOW_INSECURE_BASE_URL").ok().as_deref(),
        Some("1")
    );

    if base_url.starts_with("https://") {
        return Ok(());
    }

    if base_url.starts_with("http://") {
        if allow_insecure || is_loopback_http_base_url(base_url) {
            return Ok(());
        }
        return Err(anyhow!(
            "BOT_BASE_URL must be https:// (set BOT_ALLOW_INSECURE_BASE_URL=1 to override)"
        ));
    }

    Err(anyhow!(
        "BOT_BASE_URL must start with https:// (recommended) or http:// (insecure)"
    ))
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
        .with_ansi(false)
        .init();

    // Offline validation mode: run the existing backtest/walk-forward engine against
    // cached candles and exit. No API keys, network calls, locks, or trading state are
    // touched, so this is safe to run at any time, including while a live bot is running.
    let cli_args: Vec<String> = std::env::args().collect();
    if cli_args.iter().any(|a| a == "--backtest") {
        return run_backtest_cli(&cli_args);
    }
    if cli_args.iter().any(|a| a == "--fetch-history") {
        return run_fetch_history_cli(&cli_args).await;
    }

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

    debug!(
        "API key fingerprint: {} (len={})",
        fingerprint_first_last_4(&api_key),
        api_key.len()
    );

    let client = Client::new();

    let base_url = env::var("BOT_BASE_URL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "https://api.binance.com".to_string());

    validate_base_url(&base_url)?;
    info!("Base URL: {}", base_url);
    if base_url.starts_with("http://") {
        info!("WARNING: Using insecure http:// base URL.");
    }

    let cfg = binance_survival_bot::app::AppConfig {
        base_url,
        symbol: "BTCUSDT".to_string(),
        state_path: rp.state_path.display().to_string(),
        data_dir: rp.data_dir.display().to_string(),
        candle_cache_max_age: Duration::from_secs(60),
        api_key,
        api_secret,
    };

    // Pre-flight check (P0): Validate API connectivity and permissions before starting the loop.
    if mode == binance_survival_bot::execution::Mode::Live {
        info!("Performing pre-flight connectivity check...");
        match binance_survival_bot::account::fetch_spot_balances(
            &client,
            &cfg.api_key,
            &cfg.api_secret,
            &cfg.base_url,
        )
        .await
        {
            Ok(_) => {
                info!("Pre-flight check passed. API connectivity and permissions validated.");
            }
            Err(e) => {
                error!("Pre-flight check failed: {}", e);
                if e.to_string().contains("-2015") {
                    error!("CRITICAL: Binance rejected authentication. Please check:");
                    error!("1. API Key permissions (must have 'Enable Spot & Margin Trading')");
                    error!("2. IP Whitelist settings on Binance");
                    error!("3. API Key and Secret are correct and not swapped");
                }
                return Err(e);
            }
        }
    }

    let args: Vec<String> = std::env::args().collect();
    let loop_mode = args.iter().any(|a| a == "--loop")
        || matches!(std::env::var("BOT_LOOP").ok().as_deref(), Some("1"));

    let snap = binance_survival_bot::dashboard::new_shared_snapshot();
    let control = binance_survival_bot::dashboard::control::ControlState::new(loop_mode);
    tokio::spawn(binance_survival_bot::dashboard::serve(
        snap.clone(),
        control.clone(),
    ));

    if !loop_mode {
        binance_survival_bot::app::run_once(&client, &cfg, &snap).await?;
        return Ok(());
    }

    let sleep_secs = std::env::var("BOT_LOOP_SLEEP_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(2);
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
        if control.paused.load(std::sync::atomic::Ordering::Relaxed) {
            tokio::time::sleep(Duration::from_secs(sleep_secs)).await;
            continue;
        }

        i = i.saturating_add(1);
        info!("Heartbeat: run #{}", i);
        match binance_survival_bot::app::run_once(&client, &cfg, &snap).await {
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

// ---------------------------------------------------------------------------
// Offline backtest CLI
// ---------------------------------------------------------------------------
//
// `--backtest` runs the existing (previously unwired) backtest/walk-forward
// engine in `binance_survival_bot::backtest` against cached candles under
// `data/` and writes an HTML report via `binance_survival_bot::report`.
// It never touches API keys, the network, the single-instance lock, or
// trading state, so it is safe to run at any time, independently of the
// live/practice bot.
//
// Usage:
//   binance_survival_bot --backtest [--symbol=BTCUSDT] [--out=report.html]
//   binance_survival_bot --backtest --walk-forward=5 [--test-fraction=0.3]

fn arg_value(args: &[String], key: &str) -> Option<String> {
    let prefix = format!("{key}=");
    args.iter()
        .find_map(|a| a.strip_prefix(prefix.as_str()).map(|v| v.to_string()))
}

fn run_backtest_cli(args: &[String]) -> Result<()> {
    use binance_survival_bot::{backtest, report};

    let symbol = arg_value(args, "--symbol").unwrap_or_else(|| "BTCUSDT".to_string());
    // Defaults to the live pipeline's own cache ("data") for a quick smoke test.
    // Pass --data-dir=backtest_data (the default --fetch-history output) for a
    // statistically meaningful run.
    let data_dir = arg_value(args, "--data-dir").unwrap_or_else(|| "data".to_string());

    // --start-frac/--end-frac restrict to a chronological slice of the cached data (e.g.
    // --start-frac=0.0 --end-frac=0.75 = the earliest 75%) — for a research/holdout
    // split. Applies to both the walk-forward and single-run paths below.
    let start_frac = arg_value(args, "--start-frac").and_then(|s| s.parse::<f64>().ok());
    let end_frac = arg_value(args, "--end-frac").and_then(|s| s.parse::<f64>().ok());
    let range_label = match (start_frac, end_frac) {
        (Some(s), Some(e)) => Some(format!("{s:.2}-{e:.2}")),
        (Some(s), None) => Some(format!("{s:.2}-1.00")),
        (None, Some(e)) => Some(format!("0.00-{e:.2}")),
        (None, None) => None,
    };

    if let Some(n_str) = arg_value(args, "--walk-forward") {
        let n_windows: usize = n_str
            .parse()
            .map_err(|_| anyhow!("--walk-forward expects an integer window count, got '{n_str}'"))?;
        let test_fraction = arg_value(args, "--test-fraction")
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.3);

        let windows = if let Some(label) = &range_label {
            let s = start_frac.unwrap_or(0.0);
            let e = end_frac.unwrap_or(1.0);
            println!("Range: {label} of {data_dir}/ (chronological fraction)");
            backtest::walk_forward_range_with_config(
                &symbol,
                &data_dir,
                s,
                e,
                n_windows,
                test_fraction,
                backtest::BacktestConfig::default(),
            )
            .with_context(|| {
                format!("Walk-forward backtest failed for {symbol} over range [{s}, {e}) in {data_dir}/")
            })?
        } else {
            backtest::walk_forward(&symbol, &data_dir, n_windows, test_fraction).with_context(|| {
                format!(
                    "Walk-forward backtest failed for {symbol}. Do you have cached candles under {data_dir}/?"
                )
            })?
        };

        println!(
            "Walk-forward validation: {symbol}, {n_windows} windows, {:.0}% out-of-sample per window\n",
            test_fraction * 100.0
        );
        println!(
            "{:<32} {:>7} {:>9} {:>10} {:>8} {:>8} {:>10}",
            "window", "trades", "win_rate", "return_%", "sharpe", "pf", "max_dd_%"
        );
        for (label, rep) in &windows {
            let m = &rep.metrics;
            let pf = if m.profit_factor.is_finite() {
                format!("{:.2}", m.profit_factor)
            } else {
                "inf".to_string()
            };
            println!(
                "{:<32} {:>7} {:>8.1}% {:>9.2}% {:>8.2} {:>8} {:>9.2}%",
                label,
                m.trade_count,
                m.win_rate * 100.0,
                m.total_return_pct,
                m.sharpe_ratio,
                pf,
                m.max_drawdown_pct,
            );
            let out_path = format!("backtest_{label}.html");
            match report::write_report(rep, &out_path) {
                Ok(()) => println!("  -> {out_path}"),
                Err(e) => eprintln!("  (failed writing {out_path}: {e})"),
            }
            if args.iter().any(|a| a == "--diagnose") {
                print_regime_strategy_counts(&rep.trades);
            }
        }

        println!(
            "\nCompare per-window win rate / return / Sharpe across windows: a strategy with a \
             real edge should look broadly similar window to window. Wild swings (e.g. one \
             highly profitable window carrying the whole result) are a sign of overfitting or a \
             regime-specific fluke rather than a durable edge."
        );
        return Ok(());
    }

    let rep = if range_label.is_some() {
        let s = start_frac.unwrap_or(0.0);
        let e = end_frac.unwrap_or(1.0);
        backtest::simulate_from_cache_range(&symbol, &data_dir, s, e, backtest::BacktestConfig::default())
            .with_context(|| {
                format!("Backtest failed for {symbol} over range [{s}, {e}) in {data_dir}/")
            })?
    } else {
        backtest::simulate_from_cache(&symbol, &data_dir).with_context(|| {
            format!(
                "Backtest failed for {symbol}. Do you have cached candles under {data_dir}/? Run \
                 `--fetch-history` first (or the bot once, for a tiny smoke test against data/)."
            )
        })?
    };
    if let Some(label) = &range_label {
        println!("Range: {label} of {data_dir}/ (chronological fraction)");
    }

    let m = &rep.metrics;
    let pf = if m.profit_factor.is_finite() {
        format!("{:.2}", m.profit_factor)
    } else {
        "inf".to_string()
    };
    println!("Backtest: {symbol}");
    println!("  Trades:          {}", m.trade_count);
    println!("  Win rate:        {:.1}%", m.win_rate * 100.0);
    println!("  Total return:    {:+.2}%", m.total_return_pct);
    println!("  Final equity:    ${:.2}", m.final_equity);
    println!("  Max drawdown:    {:.2}%", m.max_drawdown_pct);
    println!("  Sharpe ratio:    {:.2}", m.sharpe_ratio);
    println!("  Profit factor:   {pf}");
    println!("  Avg winner:      {:+.2}%", m.avg_winner_pct);
    println!("  Avg loser:       {:+.2}%", m.avg_loser_pct);
    println!("  Total fees:      ${:.2}", m.total_fees_usdt);
    println!("  Total slippage:  ${:.2}", m.total_slippage_usdt);

    let out_path = arg_value(args, "--out").unwrap_or_else(|| "backtest_report.html".to_string());
    report::write_report(&rep, &out_path)
        .with_context(|| format!("Failed writing HTML report to {out_path}"))?;
    println!("\nFull report written to {out_path}");

    if m.trade_count < 30 {
        if data_dir == "data" {
            println!(
                "\nWARNING: only {} trade(s) in this sample. {data_dir}/ is the live pipeline's \
                 own cache — it holds at most ~200 recent candles per timeframe (~16 hours of 5m \
                 data), which is far too little to draw statistical conclusions. Run \
                 `--fetch-history` to build a real historical dataset, then re-run with \
                 `--data-dir=backtest_data`.",
                m.trade_count
            );
        } else {
            println!(
                "\nWARNING: only {} trade(s) in this sample from {data_dir}/. That's too few to \
                 draw statistical conclusions (win rate / Sharpe / profit factor on <30 trades \
                 are noise-dominated). Fetch a longer history window (`--fetch-history --days=N` \
                 with a larger N) before trusting these numbers for a go-live decision.",
                m.trade_count
            );
        }
    }

    if args.iter().any(|a| a == "--diagnose") {
        println!("\n=== Bar-level regime tally (all simulated 5m bars, not just ones with trades) ===");
        let total_bars: usize = rep.regime_bar_counts.iter().map(|(_, n)| n).sum();
        println!("{:<12} {:>8} {:>8}", "regime", "bars", "pct");
        for (regime, n) in &rep.regime_bar_counts {
            let pct = if total_bars == 0 { 0.0 } else { *n as f64 / total_bars as f64 * 100.0 };
            println!("{:<12} {:>8} {:>7.1}%", regime, n, pct);
        }
        print_regime_strategy_counts(&rep.trades);
        print_trade_diagnostics(&rep.trades);
        print_feature_threshold_analysis(
            &rep.trades,
            "mean_reversion",
            "velocity_1m",
            &[
                (f64::NEG_INFINITY, -0.002, "< -0.002"),
                (-0.002, -0.001, "-0.002..-0.001"),
                (-0.001, 0.0, "-0.001..0"),
                (0.0, 0.001, "0..0.001"),
                (0.001, 0.002, "0.001..0.002"),
                (0.002, f64::INFINITY, ">= 0.002"),
            ],
            |f| f.velocity_1m,
        );
        print_feature_threshold_analysis(
            &rep.trades,
            "mean_reversion",
            "volume_z",
            &[
                (f64::NEG_INFINITY, -1.5, "< -1.5"),
                (-1.5, -1.0, "-1.5..-1.0"),
                (-1.0, -0.5, "-1.0..-0.5"),
                (-0.5, 0.0, "-0.5..0"),
                (0.0, 0.5, "0..0.5"),
                (0.5, 1.0, "0.5..1.0"),
                (1.0, f64::INFINITY, ">= 1.0"),
            ],
            |f| f.volume_z,
        );
        print_feature_threshold_analysis(
            &rep.trades,
            "mean_reversion",
            "velocity_5m",
            &[
                (f64::NEG_INFINITY, -0.006, "< -0.006"),
                (-0.006, -0.003, "-0.006..-0.003"),
                (-0.003, 0.0, "-0.003..0"),
                (0.0, 0.003, "0..0.003"),
                (0.003, 0.006, "0.003..0.006"),
                (0.006, f64::INFINITY, ">= 0.006"),
            ],
            |f| f.velocity_5m,
        );

        // --- trend_breakout: same falsification approach, different candidate signals ---
        // trend_breakout's own confirmation gate already checks `volume_z > 0.0 ||
        // vol_ratio > 1.1`, and its context multiplier bumps score when `volume_z > 0.5`
        // and `htf_bullish` — so volume_z and HTF strength are the two signals the
        // strategy itself already leans on, making them the natural first candidates to
        // check for real separation before building anything.
        print_exit_reason_by_strategy(&rep.trades, "trend_breakout");
        print_feature_threshold_analysis(
            &rep.trades,
            "trend_breakout",
            "volume_z",
            &[
                (f64::NEG_INFINITY, -0.5, "< -0.5"),
                (-0.5, 0.0, "-0.5..0"),
                (0.0, 0.5, "0..0.5"),
                (0.5, 1.0, "0.5..1.0"),
                (1.0, 2.0, "1.0..2.0"),
                (2.0, f64::INFINITY, ">= 2.0"),
            ],
            |f| f.volume_z,
        );
        // HTF bias strength proxy: no purpose-built continuous field exists (only the
        // boolean htf_bullish/bearish_bias), so this approximates "how strongly is the
        // 1h EMA stack tilted bullish" as the EMA20/EMA50 spread on 1h, normalized to %.
        print_feature_threshold_analysis(
            &rep.trades,
            "trend_breakout",
            "htf_ema_spread_pct",
            &[
                (f64::NEG_INFINITY, 0.0, "< 0"),
                (0.0, 0.1, "0..0.1"),
                (0.1, 0.2, "0.1..0.2"),
                (0.2, 0.4, "0.2..0.4"),
                (0.4, f64::INFINITY, ">= 0.4"),
            ],
            |f| {
                if f.ema50_1h.abs() > 1e-9 {
                    (f.ema20_1h - f.ema50_1h) / f.ema50_1h * 100.0
                } else {
                    0.0
                }
            },
        );
        // Regime-classification confidence: `trend_strength` is the actual named cutoff
        // variable in detect_regime's Trending gate (`trend_strength >= trend_strength_min`,
        // default 0.8) — the 5m EMA20/EMA50 spread normalized by ATR. Every trend_breakout
        // trade satisfies >= 0.8 by construction; this checks whether trades entered just
        // past the cutoff ("barely Trending") perform differently from ones entered well
        // past it ("strongly Trending").
        print_feature_threshold_analysis(
            &rep.trades,
            "trend_breakout",
            "trend_strength",
            &[
                (f64::NEG_INFINITY, 0.8, "< 0.8 (below cutoff — shouldn't occur)"),
                (0.8, 1.2, "0.8..1.2"),
                (1.2, 1.6, "1.2..1.6"),
                (1.6, 2.2, "1.6..2.2"),
                (2.2, f64::INFINITY, ">= 2.2"),
            ],
            |f| f.trend_strength,
        );

        // Before picking one of volume_z / htf_ema_spread_pct as the gate to test: are
        // they two independent signals, or two proxies for the same "clean breakout"
        // effect? A high correlation means a positive result on one doesn't add
        // information beyond the other.
        print_feature_correlation(
            &rep.trades,
            "trend_breakout",
            "volume_z",
            "htf_ema_spread_pct",
            |f| f.volume_z,
            |f| {
                if f.ema50_1h.abs() > 1e-9 {
                    (f.ema20_1h - f.ema50_1h) / f.ema50_1h * 100.0
                } else {
                    0.0
                }
            },
        );
    }

    Ok(())
}

/// Pearson correlation coefficient between two entry-time feature values, computed over
/// one strategy's trades. Cheap sanity check before choosing which of two candidate
/// signals to test with a walk-forward A/B — a high |r| means they're likely measuring
/// the same underlying effect rather than independent confirmations.
fn print_feature_correlation<FA, FB>(
    trades: &[binance_survival_bot::telemetry::TradeRecord],
    strategy: &str,
    label_a: &str,
    label_b: &str,
    extract_a: FA,
    extract_b: FB,
) where
    FA: Fn(&binance_survival_bot::telemetry::FeatureSnapshot) -> f64,
    FB: Fn(&binance_survival_bot::telemetry::FeatureSnapshot) -> f64,
{
    let mut xs = Vec::new();
    let mut ys = Vec::new();
    for t in trades {
        if t.strategy != strategy {
            continue;
        }
        xs.push(extract_a(&t.features));
        ys.push(extract_b(&t.features));
    }
    let n = xs.len();
    if n < 2 {
        println!("\n=== correlation({label_a}, {label_b}), {strategy} only: n={n}, too few to compute ===");
        return;
    }

    let mean_x = xs.iter().sum::<f64>() / n as f64;
    let mean_y = ys.iter().sum::<f64>() / n as f64;
    let mut cov = 0.0;
    let mut var_x = 0.0;
    let mut var_y = 0.0;
    for i in 0..n {
        let dx = xs[i] - mean_x;
        let dy = ys[i] - mean_y;
        cov += dx * dy;
        var_x += dx * dx;
        var_y += dy * dy;
    }
    let r = if var_x > 1e-12 && var_y > 1e-12 {
        cov / (var_x.sqrt() * var_y.sqrt())
    } else {
        0.0
    };

    println!("\n=== correlation({label_a}, {label_b}), {strategy} only (n={n}) ===");
    println!("r = {r:.3}");
}

/// Exit-reason breakdown scoped to a single strategy (the general one in
/// `print_trade_diagnostics` mixes all strategies together, which can hide a
/// strategy-specific pattern the same way the combined view initially masked the
/// range_tp mislabeling bug).
fn print_exit_reason_by_strategy(trades: &[binance_survival_bot::telemetry::TradeRecord], strategy: &str) {
    use std::collections::HashMap;

    #[derive(Default)]
    struct Bucket {
        n: usize,
        sum_mfe_pct: f64,
        sum_mae_pct: f64,
        sum_dur_min: f64,
        sum_ret_pct: f64,
    }
    impl Bucket {
        fn add(&mut self, mfe_pct: f64, mae_pct: f64, dur_min: f64, ret_pct: f64) {
            self.n += 1;
            self.sum_mfe_pct += mfe_pct;
            self.sum_mae_pct += mae_pct;
            self.sum_dur_min += dur_min;
            self.sum_ret_pct += ret_pct;
        }
        fn avg_mfe(&self) -> f64 { if self.n == 0 { 0.0 } else { self.sum_mfe_pct / self.n as f64 } }
        fn avg_mae(&self) -> f64 { if self.n == 0 { 0.0 } else { self.sum_mae_pct / self.n as f64 } }
        fn avg_dur(&self) -> f64 { if self.n == 0 { 0.0 } else { self.sum_dur_min / self.n as f64 } }
        fn avg_ret(&self) -> f64 { if self.n == 0 { 0.0 } else { self.sum_ret_pct / self.n as f64 } }
    }

    let mut by_reason: HashMap<String, Bucket> = HashMap::new();
    let mut n_total = 0usize;
    for t in trades {
        if t.strategy != strategy {
            continue;
        }
        let Some(net) = t.net_pnl_usdt else { continue };
        if t.position_value_usdt <= 0.0 {
            continue;
        }
        n_total += 1;
        let pv = t.position_value_usdt;
        let ret_pct = net / pv * 100.0;
        let mfe_pct = t.mfe_usdt.map(|v| v / pv * 100.0).unwrap_or(0.0);
        let mae_pct = t.mae_usdt.map(|v| v / pv * 100.0).unwrap_or(0.0);
        let dur_min = t.duration_ms.unwrap_or(0) as f64 / 60_000.0;
        let reason = t.exit_reason.clone().unwrap_or_else(|| "unknown".to_string());
        by_reason.entry(reason).or_default().add(mfe_pct, mae_pct, dur_min, ret_pct);
    }

    println!("\n=== By exit reason, {strategy} only (n={n_total}) ===");
    println!(
        "{:<45} {:>6} {:>10} {:>10} {:>11} {:>10}",
        "exit_reason", "n", "avg_mfe%", "avg_mae%", "avg_dur_min", "avg_ret%"
    );
    let mut reasons: Vec<_> = by_reason.keys().cloned().collect();
    reasons.sort();
    for reason in &reasons {
        let b = &by_reason[reason];
        println!(
            "{:<45} {:>6} {:>10.2} {:>10.2} {:>11.1} {:>10.2}",
            reason, b.n, b.avg_mfe(), b.avg_mae(), b.avg_dur(), b.avg_ret()
        );
    }
}

/// Trade-count sanity check by (regime, strategy), printed before any deeper diagnostic.
/// Win-rate/PF/Sharpe on a low-n slice (e.g. a handful of Trending trades in a mostly
/// Ranging sample) are noise, not evidence — this answers "is there even enough data to
/// draw a conclusion" before spending time on MFE/MAE or threshold-sensitivity analysis.
fn print_regime_strategy_counts(trades: &[binance_survival_bot::telemetry::TradeRecord]) {
    use std::collections::HashMap;

    let mut counts: HashMap<(String, String), (usize, usize)> = HashMap::new(); // (regime, strategy) -> (n, wins)
    for t in trades {
        let Some(net) = t.net_pnl_usdt else { continue };
        let entry = counts.entry((t.regime.clone(), t.strategy.clone())).or_insert((0, 0));
        entry.0 += 1;
        if net > 0.0 {
            entry.1 += 1;
        }
    }

    println!("\n=== Trade counts by regime x strategy (n={}) ===", trades.len());
    println!("{:<12} {:<20} {:>6} {:>10}", "regime", "strategy", "n", "win_rate");
    let mut keys: Vec<_> = counts.keys().cloned().collect();
    keys.sort();
    for key @ (regime, strategy) in &keys {
        let (n, wins) = counts[key];
        let wr = if n == 0 { 0.0 } else { wins as f64 / n as f64 * 100.0 };
        println!("{:<12} {:<20} {:>6} {:>9.1}%", regime, strategy, n, wr);
    }
}

/// Threshold-sensitivity check for a candidate mean-reversion entry-confirmation signal
/// (e.g. `velocity_1m`, `volume_z`). Every trade's `features` snapshot already records
/// these at entry regardless of whether any gate using them was enabled for the run, so
/// this answers "does this signal separate winners from losers at all, and where" from a
/// single control (gate-off) run — no need to re-run per candidate signal or cutoff.
fn print_feature_threshold_analysis<F>(
    trades: &[binance_survival_bot::telemetry::TradeRecord],
    strategy: &str,
    label: &str,
    edges: &[(f64, f64, &str)],
    extract: F,
) where
    F: Fn(&binance_survival_bot::telemetry::FeatureSnapshot) -> f64,
{
    #[derive(Default)]
    struct Bucket {
        n: usize,
        wins: usize,
        sum_ret_pct: f64,
    }
    impl Bucket {
        fn add(&mut self, is_win: bool, ret_pct: f64) {
            self.n += 1;
            if is_win {
                self.wins += 1;
            }
            self.sum_ret_pct += ret_pct;
        }
        fn win_rate(&self) -> f64 { if self.n == 0 { 0.0 } else { self.wins as f64 / self.n as f64 * 100.0 } }
        fn avg_ret(&self) -> f64 { if self.n == 0 { 0.0 } else { self.sum_ret_pct / self.n as f64 } }
    }

    let mut buckets: Vec<Bucket> = (0..edges.len()).map(|_| Bucket::default()).collect();
    let mut split_le0 = Bucket::default();
    let mut split_gt0 = Bucket::default();
    let mut n_total = 0usize;

    for t in trades {
        if t.strategy != strategy {
            continue;
        }
        let Some(net) = t.net_pnl_usdt else { continue };
        if t.position_value_usdt <= 0.0 {
            continue;
        }
        let ret_pct = net / t.position_value_usdt * 100.0;
        let v = extract(&t.features);
        let is_win = net > 0.0;
        n_total += 1;

        for (i, (lo, hi, _)) in edges.iter().enumerate() {
            if v >= *lo && v < *hi {
                buckets[i].add(is_win, ret_pct);
                break;
            }
        }

        if v <= 0.0 {
            split_le0.add(is_win, ret_pct);
        } else {
            split_gt0.add(is_win, ret_pct);
        }
    }

    println!("\n=== {label} at entry, {strategy} only (n={n_total}) ===");
    println!("{:<16} {:>6} {:>10} {:>10}", label, "n", "win_rate", "avg_ret%");
    for (i, (_, _, blabel)) in edges.iter().enumerate() {
        let b = &buckets[i];
        println!("{:<16} {:>6} {:>9.1}% {:>10.2}", blabel, b.n, b.win_rate(), b.avg_ret());
    }

    println!("\n--- simple split at 0.0 ---");
    println!("{:<16} {:>6} {:>10} {:>10}", label, "n", "win_rate", "avg_ret%");
    println!("{:<16} {:>6} {:>9.1}% {:>10.2}", "<= 0.0", split_le0.n, split_le0.win_rate(), split_le0.avg_ret());
    println!("{:<16} {:>6} {:>9.1}% {:>10.2}", "> 0.0", split_gt0.n, split_gt0.win_rate(), split_gt0.avg_ret());
}

/// Breaks the full trade ledger down by outcome, strategy, and exit reason, using the
/// MFE/MAE excursion data the backtest already tracks. This is meant to answer one
/// question before touching any parameter: is a negative-expectancy result caused by
/// entries pointing the wrong way (small MFE, large immediate MAE — "falling knife"),
/// by exits giving back a favorable move (large MFE, exit near/below breakeven), or by
/// stops too tight for the regime's noise floor (both MFE and MAE small, quick stop-outs)?
fn print_trade_diagnostics(trades: &[binance_survival_bot::telemetry::TradeRecord]) {
    use std::collections::HashMap;

    #[derive(Default)]
    struct Bucket {
        n: usize,
        sum_mfe_pct: f64,
        sum_mae_pct: f64,
        sum_dur_min: f64,
        sum_ret_pct: f64,
    }
    impl Bucket {
        fn add(&mut self, mfe_pct: f64, mae_pct: f64, dur_min: f64, ret_pct: f64) {
            self.n += 1;
            self.sum_mfe_pct += mfe_pct;
            self.sum_mae_pct += mae_pct;
            self.sum_dur_min += dur_min;
            self.sum_ret_pct += ret_pct;
        }
        fn avg_mfe(&self) -> f64 { if self.n == 0 { 0.0 } else { self.sum_mfe_pct / self.n as f64 } }
        fn avg_mae(&self) -> f64 { if self.n == 0 { 0.0 } else { self.sum_mae_pct / self.n as f64 } }
        fn avg_dur(&self) -> f64 { if self.n == 0 { 0.0 } else { self.sum_dur_min / self.n as f64 } }
        fn avg_ret(&self) -> f64 { if self.n == 0 { 0.0 } else { self.sum_ret_pct / self.n as f64 } }
    }

    let mut winners = Bucket::default();
    let mut losers = Bucket::default();
    let mut by_strategy: HashMap<(String, bool), Bucket> = HashMap::new();
    let mut by_exit_reason: HashMap<String, Bucket> = HashMap::new();

    for t in trades {
        let Some(net) = t.net_pnl_usdt else { continue };
        if t.position_value_usdt <= 0.0 {
            continue;
        }
        let pv = t.position_value_usdt;
        let ret_pct = net / pv * 100.0;
        let mfe_pct = t.mfe_usdt.map(|v| v / pv * 100.0).unwrap_or(0.0);
        let mae_pct = t.mae_usdt.map(|v| v / pv * 100.0).unwrap_or(0.0);
        let dur_min = t.duration_ms.unwrap_or(0) as f64 / 60_000.0;
        let is_winner = net > 0.0;

        if is_winner {
            winners.add(mfe_pct, mae_pct, dur_min, ret_pct);
        } else {
            losers.add(mfe_pct, mae_pct, dur_min, ret_pct);
        }

        by_strategy
            .entry((t.strategy.clone(), is_winner))
            .or_default()
            .add(mfe_pct, mae_pct, dur_min, ret_pct);

        let reason = t.exit_reason.clone().unwrap_or_else(|| "unknown".to_string());
        by_exit_reason.entry(reason).or_default().add(mfe_pct, mae_pct, dur_min, ret_pct);
    }

    println!("\n=== MFE/MAE diagnostic (% of position value; n={}) ===", trades.len());
    println!(
        "{:<10} {:>6} {:>10} {:>10} {:>11} {:>10}",
        "outcome", "n", "avg_mfe%", "avg_mae%", "avg_dur_min", "avg_ret%"
    );
    println!(
        "{:<10} {:>6} {:>10.2} {:>10.2} {:>11.1} {:>10.2}",
        "winners", winners.n, winners.avg_mfe(), winners.avg_mae(), winners.avg_dur(), winners.avg_ret()
    );
    println!(
        "{:<10} {:>6} {:>10.2} {:>10.2} {:>11.1} {:>10.2}",
        "losers", losers.n, losers.avg_mfe(), losers.avg_mae(), losers.avg_dur(), losers.avg_ret()
    );

    println!("\n=== By strategy x outcome ===");
    println!(
        "{:<20} {:<6} {:>6} {:>10} {:>10} {:>11} {:>10}",
        "strategy", "result", "n", "avg_mfe%", "avg_mae%", "avg_dur_min", "avg_ret%"
    );
    let mut keys: Vec<_> = by_strategy.keys().cloned().collect();
    keys.sort();
    for key @ (strat, is_win) in &keys {
        let b = &by_strategy[key];
        println!(
            "{:<20} {:<6} {:>6} {:>10.2} {:>10.2} {:>11.1} {:>10.2}",
            strat,
            if *is_win { "win" } else { "loss" },
            b.n,
            b.avg_mfe(),
            b.avg_mae(),
            b.avg_dur(),
            b.avg_ret()
        );
    }

    println!("\n=== By exit reason ===");
    println!(
        "{:<45} {:>6} {:>10} {:>10} {:>11} {:>10}",
        "exit_reason", "n", "avg_mfe%", "avg_mae%", "avg_dur_min", "avg_ret%"
    );
    let mut reasons: Vec<_> = by_exit_reason.keys().cloned().collect();
    reasons.sort();
    for reason in &reasons {
        let b = &by_exit_reason[reason];
        println!(
            "{:<45} {:>6} {:>10.2} {:>10.2} {:>11.1} {:>10.2}",
            reason, b.n, b.avg_mfe(), b.avg_mae(), b.avg_dur(), b.avg_ret()
        );
    }
}

// ---------------------------------------------------------------------------
// Historical data fetcher CLI
// ---------------------------------------------------------------------------
//
// `--fetch-history` pulls a large window of BTCUSDT candles from Binance's
// public klines endpoint (no API key needed) into its own directory, so
// `--backtest`/`--walk-forward` have something statistically meaningful to
// run against instead of the live pipeline's ~200-candle rolling cache.
//
// Usage:
//   binance_survival_bot --fetch-history [--symbol=BTCUSDT] [--days=90] [--out-dir=backtest_data]

async fn run_fetch_history_cli(args: &[String]) -> Result<()> {
    use binance_survival_bot::candles::Interval;
    use binance_survival_bot::history;

    let symbol = arg_value(args, "--symbol").unwrap_or_else(|| "BTCUSDT".to_string());
    let days: i64 = arg_value(args, "--days")
        .and_then(|s| s.parse().ok())
        .unwrap_or(90);
    let out_dir = arg_value(args, "--out-dir").unwrap_or_else(|| "backtest_data".to_string());
    let base_url =
        arg_value(args, "--base-url").unwrap_or_else(|| "https://api.binance.com".to_string());

    if days <= 0 || days > 720 {
        return Err(anyhow!("--days must be between 1 and 720, got {days}"));
    }

    let end_ms = binance_survival_bot::state::now_ms() as i64;
    let start_ms = end_ms - days * 24 * 60 * 60 * 1000;

    let client = Client::new();

    for interval in [Interval::OneHour, Interval::FiveMinutes, Interval::OneMinute] {
        println!("Fetching {symbol} {} history: {days} day(s)...", interval.as_str());
        let candles =
            history::fetch_history_range(&client, &base_url, &symbol, interval, start_ms, end_ms)
                .await
                .with_context(|| format!("Failed fetching {} history", interval.as_str()))?;
        println!("  got {} candles", candles.len());
        history::save_history(std::path::Path::new(&out_dir), &symbol, interval, &candles)
            .with_context(|| format!("Failed saving {} history to {out_dir}/", interval.as_str()))?;
        println!("  saved to {out_dir}/{symbol}_{}.json", interval.as_str());
    }

    println!(
        "\nDone. Run `--backtest --data-dir={out_dir}` or \
         `--backtest --walk-forward=8 --data-dir={out_dir}` to validate against this dataset."
    );
    Ok(())
}
