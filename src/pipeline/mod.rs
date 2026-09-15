#![allow(clippy::collapsible_if)]

use crate::candles::{self, Interval};
use crate::execution::{self, Mode};
use crate::http_policy::{send_text_with_retry, HttpPolicy};
use crate::regime::Regime;
use crate::{account, binance_orders, exchange_info, features, log_say, regime, risk, signals, sizing, state, telemetry};
use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use std::io::Write;
use std::path::Path;
use std::time::Duration;
use tracing::{debug, info};

#[derive(Debug, Clone)]
pub struct CoreOutcome {
    pub did_place_order: bool,
    pub mode: Mode,
    pub regime: Regime,
    pub decision: String,
    pub reason: String,
    pub end: String,
}

fn is_auth_error(err: &anyhow::Error) -> bool {
    let s = err.to_string();
    // Binance returns code -2015 for invalid key/permissions.
    s.contains("401 Unauthorized")
        || s.contains("\"code\":-2015")
        || s.contains("\"code\":-2014")
        || s.contains("\"code\":-1021")
        || s.contains("Invalid API-key")
}

fn is_binance_code(err: &anyhow::Error, code: i64) -> bool {
    // Many call sites bubble up raw Binance JSON bodies like {"code":-2015,"msg":"..."}.
    // Avoid parsing here; just do a robust substring check.
    let s = err.to_string();
    let needle = format!("\"code\":{code}");
    s.contains(&needle)
}

fn fmt_usdt(v: f64) -> String {
    format!("{:.2}", v)
}

fn fmt_btc(v: f64) -> String {
    format!("{:.8}", v)
}

fn candle_lag_ms(candles: &[candles::Candle], now_ms: u64) -> Option<u64> {
    let last = candles.last()?;
    let last_ms = last.open_time.max(0) as u64;
    Some(now_ms.saturating_sub(last_ms))
}

fn env_flag(name: &str) -> bool {
    matches!(
        std::env::var(name).ok().as_deref(),
        Some("1") | Some("true") | Some("TRUE") | Some("yes") | Some("YES")
    )
}

fn env_f64(name: &str) -> Option<f64> {
    std::env::var(name).ok().and_then(|v| v.parse::<f64>().ok())
}

fn env_u32(name: &str) -> Option<u32> {
    std::env::var(name).ok().and_then(|v| v.parse::<u32>().ok())
}

fn parse_f64_field(label: &str, s: &str) -> Result<f64> {
    s.parse::<f64>()
        .map_err(|e| anyhow!("Bad number for {label}: {s} ({e})"))
}

fn avg_price_from_order_status(st: &binance_orders::OrderStatus) -> Result<Option<f64>> {
    let exec_qty = parse_f64_field("executedQty", &st.executed_qty)?;
    if exec_qty <= 0.0 {
        return Ok(None);
    }
    let quote = parse_f64_field("cummulativeQuoteQty", &st.cummulative_quote_qty)?;
    if quote <= 0.0 {
        return Ok(None);
    }
    Ok(Some(quote / exec_qty))
}

fn fee_usdt_from_trades(trades: &[binance_orders::MyTrade], price_usdt: f64) -> Result<f64> {
    let mut fee_usdt = 0.0;
    for t in trades {
        let c = parse_f64_field("commission", &t.commission)?;
        match t.commission_asset.as_str() {
            "USDT" => fee_usdt += c,
            "BTC" => fee_usdt += c * price_usdt,
            // Unknown asset; ignore for now.
            _ => {}
        }
    }
    Ok(fee_usdt)
}

fn pct(v: f64) -> f64 {
    v * 100.0
}

fn daily_loss_fraction(daily_start_equity: f64, equity: f64) -> f64 {
    if daily_start_equity <= 0.0 {
        return 0.0;
    }
    ((daily_start_equity - equity) / daily_start_equity).max(0.0)
}

#[allow(clippy::too_many_arguments)]
fn log_decision_summary(
    run_id: &str,
    mode: Mode,
    regime: Regime,
    position: &str,
    action: &str,
    reason: &str,
    trades_today: Option<(u32, u32)>,
    drawdown_frac: Option<f64>,
    daily_loss_frac: Option<f64>,
    throttled: Option<bool>,
) {
    let t = trades_today
        .map(|(a, b)| format!("{a}/{b}"))
        .unwrap_or_else(|| "?/?".to_string());
    let dd = drawdown_frac.map(pct).unwrap_or(0.0);
    let dl = daily_loss_frac.map(pct).unwrap_or(0.0);
    let th = throttled.map(|x| if x { "yes" } else { "no" }).unwrap_or("?");

    debug!(
        "Decision summary: run_id={} mode={:?} regime={:?} position={} action={} trades_today={} dd={:.2}% daily_loss={:.2}% throttle={}",
        run_id,
        mode,
        regime,
        position,
        action,
        t,
        dd,
        dl,
        th
    );
    if !reason.is_empty() {
        debug!("Decision reason: run_id={} {}", run_id, reason);
    }
}

fn json_audit_enabled() -> bool {
    env_flag("BOT_JSON_AUDIT")
}

fn audit_log_path() -> Option<String> {
    std::env::var("BOT_AUDIT_LOG_PATH")
        .ok()
        .filter(|s| !s.trim().is_empty())
}

fn audit_key_allowed(key: &str) -> bool {
    matches!(
        key,
        "run_id"
            | "event"
            | "mode"
            | "regime"
            | "position"
            | "action"
            | "decision"
            | "reason"
            | "price"
            | "stop"
            | "qty"
            | "notional"
            | "entry_price"
            | "atr14_5m"
            | "stop_dist"
            | "risk_frac_base"
            | "risk_frac_effective"
            | "cap_usdt"
            | "was_capped"
            | "min_notional"
            | "equity_usdt"
            | "peak_equity_usdt"
            | "daily_loss_start_equity_usdt"
            | "drawdown_frac"
            | "daily_loss_frac"
            | "trades_today"
            | "max_trades_per_day"
    )
}

fn audit_sanitize_scalar(v: &serde_json::Value) -> Option<serde_json::Value> {
    match v {
        serde_json::Value::Null => Some(serde_json::Value::Null),
        serde_json::Value::Bool(b) => Some(serde_json::Value::Bool(*b)),
        serde_json::Value::Number(n) => Some(serde_json::Value::Number(n.clone())),
        serde_json::Value::String(s) => {
            const MAX: usize = 200;
            if s.len() <= MAX {
                Some(serde_json::Value::String(s.clone()))
            } else {
                Some(serde_json::Value::String(s.chars().take(MAX).collect()))
            }
        }
        serde_json::Value::Array(_) | serde_json::Value::Object(_) => None,
    }
}

fn audit_sanitize(value: serde_json::Value) -> Option<serde_json::Value> {
    let serde_json::Value::Object(map) = value else {
        return None;
    };

    let mut out = serde_json::Map::new();
    for (k, v) in map {
        if !audit_key_allowed(&k) {
            continue;
        }
        if let Some(sv) = audit_sanitize_scalar(&v) {
            out.insert(k, sv);
        }
    }
    Some(serde_json::Value::Object(out))
}

fn audit_emit_json(value: serde_json::Value) {
    if !json_audit_enabled() {
        return;
    }

    let Some(value) = audit_sanitize(value) else {
        return;
    };

    let line = match serde_json::to_string(&value) {
        Ok(s) => s,
        Err(_) => return,
    };

    debug!("AUDIT_JSON {}", line);

    if let Some(path) = audit_log_path() {
        if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
            let _ = writeln!(f, "{}", line);
        }
    }
}

fn audit_emit_json_with_run_id(run_id: &str, mut value: serde_json::Value) {
    if let serde_json::Value::Object(ref mut map) = value {
        map.insert(
            "run_id".to_string(),
            serde_json::Value::String(run_id.to_string()),
        );
    }
    audit_emit_json(value);
}

pub async fn fetch_price(client: &Client, base_url: &str, symbol: &str) -> Result<f64> {
    #[derive(serde::Deserialize)]
    struct PriceResp {
        price: String,
    }

    let url = format!("{base_url}/api/v3/ticker/price?symbol={symbol}");

    let policy = HttpPolicy::from_env();
    let (status, text) = send_text_with_retry(client, &policy, &url, || client.get(&url)).await?;

    if !status.is_success() {
        return Err(anyhow!("Price returned {status}: {text}"));
    }

    let body = serde_json::from_str::<PriceResp>(&text)?;
    Ok(body.price.parse::<f64>()?)
}

pub async fn detect_public_ip(client: &Client) -> Option<String> {
    let enabled = matches!(
        std::env::var("BOT_PUBLIC_IP_LOOKUP").ok().as_deref(),
        Some("1")
    );
    if !enabled {
        return None;
    }

    let url = "https://api.ipify.org?format=json";

    let mut policy = HttpPolicy::from_env();
    policy.timeout = Duration::from_secs(5);
    policy.max_retries = policy.max_retries.min(2);
    let (_status, text) = send_text_with_retry(client, &policy, url, || client.get(url)).await.ok()?;

    // ipify returns either {"ip":"x.x.x.x"} or plain text if format is changed.
    let ip = match serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|v| v.get("ip").and_then(|x| x.as_str()).map(|s| s.to_string()))
    {
        Some(ip) => ip,
        None => text.trim().to_string(),
    };

    if ip.is_empty() {
        return None;
    }

    info!("Public IP detected: {}", ip);
    Some(ip)
}

fn log_actions_for_2015() {
    info!(
        "Actions:\n- Confirm key+secret are from the same API key entry\n- Enable Read + Spot & Margin Trading\n- Check IP whitelist"
    );
}

fn log_ip_whitelist_hint(_public_ip: &Option<String>) {
    // The exact IP is logged by detect_public_ip(); keep this line stable for copy/paste guidance.
    if matches!(
        std::env::var("BOT_PUBLIC_IP_LOOKUP").ok().as_deref(),
        Some("1")
    ) {
        info!("If your Binance API key uses IP restriction, whitelist this IP.");
    } else {
        info!("Public IP lookup is disabled (set BOT_PUBLIC_IP_LOOKUP=1). If your Binance API key uses IP restriction, whitelist this machine's public IP.");
    }
}

fn qty_precision_from_step(step_size: f64) -> usize {
    // For BTCUSDT step is usually 0.00001 => precision 5
    if step_size <= 0.0 {
        return 8;
    }
    let s = format!("{step_size:.16}");
    if let Some(dot) = s.find('.') {
        let frac = &s[dot + 1..];
        let trimmed = frac.trim_end_matches('0');
        trimmed.len().min(10)
    } else {
        0
    }
}

fn price_precision_from_tick(tick_size: f64) -> usize {
    if tick_size <= 0.0 {
        return 2;
    }
    let s = format!("{tick_size:.16}");
    if let Some(dot) = s.find('.') {
        let frac = &s[dot + 1..];
        let trimmed = frac.trim_end_matches('0');
        trimmed.len().min(10)
    } else {
        0
    }
}

fn position_label(p: &state::Position) -> &'static str {
    match p {
        state::Position::Flat => "Flat",
        state::Position::ExternalInventory { .. } => "ExternalInventory",
        state::Position::Long { .. } => "Long",
    }
}

pub struct MarketData {
    pub price: f64,
    pub candles_1m: Vec<candles::Candle>,
    pub candles_5m: Vec<candles::Candle>,
    pub candles_1h: Vec<candles::Candle>,
    pub step_size: f64,
    pub tick_size: f64,
    pub min_notional: f64,
    pub stale_any: bool,
    pub lag_1m_ms: u64,
    pub lag_5m_ms: u64,
    pub lag_1h_ms: u64,
    pub now_ms: u64,
}

pub async fn fetch_market_data(
    client: &Client,
    base_url: &str,
    data_dir: &Path,
    symbol: &str,
    candle_cache_max_age: Duration,
    have_wallet: bool,
) -> Result<MarketData> {
    let price = fetch_price(client, base_url, symbol).await?;
    log_say::say_price(price);

    let (step_size, tick_size, min_notional) = if have_wallet {
        exchange_info::symbol_rules_from_base(client, base_url, symbol).await?
    } else {
        (0.00001, 0.01, 5.0)
    };

    let candles_1m = candles::fetch_klines_cached_from_base(
        client,
        base_url,
        data_dir,
        symbol,
        Interval::OneMinute,
        200,
        candle_cache_max_age,
    )
    .await
    .context("Failed fetching 1m candles")?;

    let candles_5m = candles::fetch_klines_cached_from_base(
        client,
        base_url,
        data_dir,
        symbol,
        Interval::FiveMinutes,
        200,
        candle_cache_max_age,
    )
    .await
    .context("Failed fetching 5m candles")?;

    let candles_1h = candles::fetch_klines_cached_from_base(
        client,
        base_url,
        data_dir,
        symbol,
        Interval::OneHour,
        200,
        candle_cache_max_age,
    )
    .await
    .context("Failed fetching 1h candles")?;

    let now_ms = state::now_ms();
    let lag_1m_ms = candle_lag_ms(&candles_1m, now_ms).unwrap_or(u64::MAX);
    let lag_5m_ms = candle_lag_ms(&candles_5m, now_ms).unwrap_or(u64::MAX);
    let lag_1h_ms = candle_lag_ms(&candles_1h, now_ms).unwrap_or(u64::MAX);
    let max_lag_1m_min = env_u32("BOT_MAX_CANDLE_LAG_1M_MIN").unwrap_or(5) as u64;
    let max_lag_5m_min = env_u32("BOT_MAX_CANDLE_LAG_5M_MIN")
        .or_else(|| env_u32("BOT_MAX_CANDLE_LAG_MIN"))
        .unwrap_or(15) as u64;
    let max_lag_1h_min = env_u32("BOT_MAX_CANDLE_LAG_1H_MIN").unwrap_or(180) as u64;
    let stale_1m = lag_1m_ms > max_lag_1m_min * 60_000;
    let stale_5m = lag_5m_ms > max_lag_5m_min * 60_000;
    let stale_1h = lag_1h_ms > max_lag_1h_min * 60_000;
    let stale_any = stale_1m || stale_5m || stale_1h;
    debug!(
        "Candle freshness: lag_1m_min={} lag_5m_min={} lag_1h_min={} stale_any={}",
        lag_1m_ms / 60_000,
        lag_5m_ms / 60_000,
        lag_1h_ms / 60_000,
        stale_any
    );

    Ok(MarketData {
        price,
        candles_1m,
        candles_5m,
        candles_1h,
        step_size,
        tick_size,
        min_notional,
        stale_any,
        lag_1m_ms,
        lag_5m_ms,
        lag_1h_ms,
        now_ms,
    })
}

pub fn compute_features(
    candles_1m: &[candles::Candle],
    candles_5m: &[candles::Candle],
    candles_1h: &[candles::Candle],
) -> Result<features::Features> {
    features::compute_features(candles_1m, candles_5m, candles_1h)
}

pub fn persist_state(path: &str, st: &state::BotState) -> Result<()> {
    state::save(path, st)
}

pub async fn run_once_core(
    client: &Client,
    cfg: &crate::app::AppConfig,
    run_id: &str,
    mode: Mode,
    snap: &crate::dashboard::SharedSnapshot,
) -> Result<CoreOutcome> {
    let symbol = cfg.symbol.as_str();

    let now_ms = crate::state::now_ms();
    let st_load = state::load_or_init(&cfg.state_path, 0.0).unwrap_or_default();
    let in_auth_cooldown = st_load.in_auth_cooldown(now_ms);

    if in_auth_cooldown {
        let remaining = st_load.last_auth_error_ms.saturating_add(st_load.auth_error_cooldown_ms).saturating_sub(now_ms);
        info!("Private API calls disabled due to authentication cooldown ({}m remaining).", remaining / 60_000);
    }

    let have_keys = !cfg.api_key.trim().is_empty() && !cfg.api_secret.trim().is_empty() && !in_auth_cooldown;

    let public_ip = if have_keys {
        detect_public_ip(client).await
    } else {
        None
    };

    let bals = if !have_keys {
        None
    } else {
        match account::fetch_spot_balances(client, &cfg.api_key, &cfg.api_secret, &cfg.base_url)
            .await
        {
            Ok(b) => Some(b),
            Err(e)
                if e.downcast_ref::<account::BinanceApiError>().is_some() || is_auth_error(&e) =>
            {
                let mut is_2015 = false;
                if let Some(be) = e.downcast_ref::<account::BinanceApiError>() {
                    is_2015 = matches!(be.code, Some(-2015));
                    if is_2015 {
                        log_actions_for_2015();
                        log_ip_whitelist_hint(&public_ip);
                    }
                }

                if !is_2015 {
                    debug!("Check BINANCE_API_KEY settings, permissions, and IP whitelist.");
                }

                // Soft Circuit Breaker: Update state with cooldown instead of a hard flag file.
                let mut st_err = state::load_or_init(&cfg.state_path, 0.0).unwrap_or_default();
                st_err.last_auth_error_ms = now_ms;
                st_err.auth_error_cooldown_ms = 60 * 60 * 1000; // 60 minutes
                let _ = state::save(&cfg.state_path, &st_err);

                // Log to critical.log for permanent record
                if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open("critical.log") {
                    let ts = time::OffsetDateTime::now_utc();
                    let _ = writeln!(f, "{} - CRITICAL: Binance API auth rejected: {}", ts, e);
                }

                return Ok(CoreOutcome {
                    did_place_order: false,
                    mode,
                    regime: Regime::Ranging,
                    decision: "WAIT".to_string(),
                    reason: "Binance rejected the API key. Cooling down for 60m.".to_string(),
                    end: "no money moved".to_string(),
                });
            }
            Err(e) => return Err(e),
        }
    };

    let md = fetch_market_data(
        client,
        &cfg.base_url,
        Path::new(&cfg.data_dir),
        symbol,
        cfg.candle_cache_max_age,
        bals.is_some(),
    )
    .await?;

    let f = match compute_features(&md.candles_1m, &md.candles_5m, &md.candles_1h) {
        Ok(x) => x,
        Err(e) => {
            info!("Not enough candle history yet. We wait. ({})", e);
            log_decision_summary(
                run_id,
                mode,
                Regime::Ranging,
                "NoState",
                "WAIT",
                "not_enough_candle_history",
                None,
                None,
                None,
                None,
            );
            return Ok(CoreOutcome {
                did_place_order: false,
                mode,
                regime: Regime::Ranging,
                decision: "WAIT".to_string(),
                reason: "We wait. Not enough candle history yet.".to_string(),
                end: "no money moved".to_string(),
            });
        }
    };

    let r = regime::detect_regime(&f);
    log_say::say_action(&format!("Road condition: {:?}.", r.regime));
    debug!("Road reason: {}", r.reason);

    if bals.is_none() {
        log_decision_summary(
            run_id,
            mode,
            r.regime,
            "WatchOnly",
            "WAIT",
            "no_wallet_access",
            None,
            None,
            None,
            None,
        );
        return Ok(CoreOutcome {
            did_place_order: false,
            mode,
            regime: r.regime,
            decision: "WAIT".to_string(),
            reason: "I cannot see your wallet. We only watch.".to_string(),
            end: "no money moved".to_string(),
        });
    }

    let bals = bals.expect("checked above");
    let bals_initial = bals.clone();
    let equity_usdt = bals.usdt_total() + (bals.btc_total() * md.price);
    log_say::say_wallet(bals.usdt_free, bals.btc_free, equity_usdt);

    let mut st = state::load_or_init(&cfg.state_path, equity_usdt)?;
    st.sync_equity_and_day(equity_usdt);

    // Optional recovery latch: if `is_dead` was set previously but equity is no longer below the
    // -20% threshold, allow a manual revive via env. This is intentionally explicit.
    if st.is_dead
        && env_flag("BOT_REVIVE_DEAD")
        && st.start_equity_usdt > 0.0
        && st.equity_usdt > st.start_equity_usdt * 0.80
    {
        info!(
            "Revive enabled: clearing is_dead (equity {} >= 80% of start {}).",
            fmt_usdt(st.equity_usdt),
            fmt_usdt(st.start_equity_usdt)
        );
        st.is_dead = false;
        persist_state(&cfg.state_path, &st)?;
    }

    debug!(
        "Equity anchors: start_equity_usdt={} equity_usdt={}",
        fmt_usdt(st.start_equity_usdt),
        fmt_usdt(st.equity_usdt)
    );

    let (died_now, unlocked_now) = st.eval_death_and_unlock(st.equity_usdt, md.now_ms);
    if died_now || st.is_dead {
        persist_state(&cfg.state_path, &st)?;
        return Ok(CoreOutcome {
            did_place_order: false,
            mode,
            regime: r.regime,
            decision: "WAIT".to_string(),
            reason: "We lost 20%. Bot is dead. No more trading.".to_string(),
            end: "no money moved".to_string(),
        });
    }

    if unlocked_now {
        // Profit milestone behavior: bump the anchor up and reduce risk for a while.
        let reduce_hours = env_u32("BOT_RISK_REDUCE_HOURS").unwrap_or(24) as u64;
        let reduce_mult = env_f64("BOT_RISK_REDUCE_MULT").unwrap_or(0.5);
        st.start_equity_usdt = st.equity_usdt;
        st.anchor_bumps = st.anchor_bumps.saturating_add(1);
        st.risk_reduction_until_ms = md.now_ms + reduce_hours * 60 * 60 * 1000;
        debug!(
            "Profit milestone: anchor bumped to {:.2}, risk reduced x{:.2} for {}h",
            st.start_equity_usdt,
            reduce_mult,
            reduce_hours
        );
    }

    persist_state(&cfg.state_path, &st)?;
    debug!("Bot state saved.");

    // Feature flags: unlocked at +20%.
    let max_trades_per_day: u32 = if st.profit_unlocked { 5 } else { 3 };
    let mut risk_fraction_multiplier: f64 = if st.profit_unlocked { 1.25 } else { 1.0 };
    if md.now_ms < st.risk_reduction_until_ms {
        let reduce_mult = env_f64("BOT_RISK_REDUCE_MULT").unwrap_or(0.5);
        if reduce_mult.is_finite() && reduce_mult > 0.0 {
            risk_fraction_multiplier *= reduce_mult;
        }
    }
    // Volatility-adjusted position sizing: scale down when ATR is elevated vs its own history.
    risk_fraction_multiplier *= risk::atr_size_multiplier(f.atr_ratio_5m);
    debug!(
        "risk_fraction_multiplier after ATR vol-adj: {:.4} (atr_ratio_5m={:.3})",
        risk_fraction_multiplier, f.atr_ratio_5m
    );

    // Reconciliation on startup (truth: wallet). Keep it explicit and safe.
    // Manual balance changes can happen; we always choose the safest state.
    const BTC_DUST: f64 = 0.00001;
    let btc_has_dust = bals.btc_total() > BTC_DUST;

    // P0: Wallet-vs-memory reconciliation.
    // If wallet has BTC but memory says Flat, treat as external inventory and block new entries.
    let mut wallet_memory_mismatch = false;

    // 2) Long -> Flat if wallet BTC is basically empty.
    if matches!(st.position, state::Position::Long { .. }) && !btc_has_dust {
        // If the exchange stop was filled (or the position was closed externally), we may still
        // have an on-exchange stop order id to clean up later.
        if let state::Position::Long {
            stop_order_id: Some(oid),
            ..
        } = &st.position
        {
            st.last_stop_order_id = Some(*oid);
        }
        st.exit_to_flat_with_cooldown(md.now_ms);
        persist_state(&cfg.state_path, &st)?;
        debug!("Reconcile: Long->Flat because wallet BTC is empty.");
    }

    // Exchange stop cleanup: if we are Flat but still remember a stop id, attempt to cancel it.
    // This prevents orphaned stops from lingering after we flatten.
    if have_keys
        && mode == Mode::Live
        && matches!(st.position, state::Position::Flat)
        && st.last_stop_order_id.is_some()
    {
        if let Some(oid) = st.last_stop_order_id {
            debug!("Reconciling orphan stop order: order_id={}", oid);
            match execution::cancel_order(
                client,
                mode,
                &cfg.api_key,
                &cfg.api_secret,
                &cfg.base_url,
                symbol,
                oid,
            )
            .await
            {
                Ok(()) => {
                    st.clear_last_stop_order_id();
                    persist_state(&cfg.state_path, &st)?;
                    debug!("Orphan stop canceled and cleared.");
                }
                Err(e) => {
                    // Unknown order (-2011) means it may already be filled/canceled; clear to avoid loops.
                    if is_binance_code(&e, -2011) {
                        st.clear_last_stop_order_id();
                        persist_state(&cfg.state_path, &st)?;
                        debug!("Orphan stop was unknown; cleared tracking.");
                    } else if is_binance_code(&e, -2015) {
                        debug!("Cannot cancel orphan stop (auth rejected). Will retry later.");
                    } else {
                        debug!("Failed to cancel orphan stop: {}", e);
                    }
                }
            }
        }
    }

    // 3) Flat + BTC in wallet => ExternalInventory (or adopt it into a managed Long in LIVE).
    if matches!(st.position, state::Position::Flat) && btc_has_dust {
        let allow_takeover = mode == Mode::Live && env_flag("BOT_ALLOW_EXTERNAL_INVENTORY");
        if allow_takeover {
            let take_qty = sizing::round_down_to_step(bals.btc_total(), md.step_size);
            let atr = f.atr14_5m.max(1e-12);
            let mut stop = md.price - 1.8 * atr;
            if !(stop.is_finite() && stop > 0.0) {
                stop = (md.price - md.tick_size).max(md.tick_size);
            }
            if stop >= md.price {
                stop = (md.price - md.tick_size).max(md.tick_size);
            }

            if take_qty > 0.0
                && sizing::ensure_min_notional(md.price, take_qty, md.min_notional).is_ok()
                && md.price > md.tick_size
            {
                info!(
                    "Takeover enabled: adopting external BTC into managed position: qty={} entry={} stop={}.",
                    fmt_btc(take_qty),
                    fmt_usdt(md.price),
                    fmt_usdt(stop)
                );
                st.enter_long(md.price, take_qty, stop, None, md.now_ms);
                persist_state(&cfg.state_path, &st)?;
            } else {
                st.position = state::Position::ExternalInventory {
                    btc_qty: bals.btc_total(),
                    detected_time_ms: md.now_ms,
                };
                persist_state(&cfg.state_path, &st)?;
                wallet_memory_mismatch = true;
            }
        } else {
            st.position = state::Position::ExternalInventory {
                btc_qty: bals.btc_total(),
                detected_time_ms: md.now_ms,
            };
            persist_state(&cfg.state_path, &st)?;
            wallet_memory_mismatch = true;
        }
    }

    // If ExternalInventory is present but BTC is gone, clear it.
    if matches!(st.position, state::Position::ExternalInventory { .. }) && !btc_has_dust {
        st.position = state::Position::Flat;
        persist_state(&cfg.state_path, &st)?;
        debug!("Reconcile: ExternalInventory->Flat because wallet BTC is empty.");
    }

    log_say::say_state(&st.position);

    if wallet_memory_mismatch {
        log_decision_summary(
            run_id,
            mode,
            r.regime,
            position_label(&st.position),
            "WAIT",
            "wallet_memory_mismatch",
            Some((st.trades_today, max_trades_per_day)),
            Some(risk::drawdown_fraction(st.peak_equity_usdt, st.equity_usdt)),
            Some(daily_loss_fraction(st.daily_loss_start_equity_usdt, st.equity_usdt)),
            Some(risk::drawdown_fraction(st.peak_equity_usdt, st.equity_usdt) >= 0.10),
        );
        return Ok(CoreOutcome {
            did_place_order: false,
            mode,
            regime: r.regime,
            decision: "WAIT".to_string(),
            reason: "Wallet and memory did not match. I will not trade until fixed.".to_string(),
            end: "no money moved".to_string(),
        });
    }

    // Trading rules when external inventory is present.
    if let state::Position::ExternalInventory { .. } = &st.position {
        match mode {
            Mode::Practice => {
                log_decision_summary(
                    run_id,
                    mode,
                    r.regime,
                    position_label(&st.position),
                    "WAIT",
                    "external_inventory_practice",
                    Some((st.trades_today, max_trades_per_day)),
                    Some(risk::drawdown_fraction(st.peak_equity_usdt, st.equity_usdt)),
                    Some(daily_loss_fraction(st.daily_loss_start_equity_usdt, st.equity_usdt)),
                    Some(risk::drawdown_fraction(st.peak_equity_usdt, st.equity_usdt) >= 0.10),
                );
                return Ok(CoreOutcome {
                    did_place_order: false,
                    mode,
                    regime: r.regime,
                    decision: "WAIT".to_string(),
                    reason: "This BTC was not bought by the bot. We only watch.".to_string(),
                    end: "no money moved".to_string(),
                });
            }
            Mode::Live => {
                let allow = env_flag("BOT_ALLOW_EXTERNAL_INVENTORY");
                if !allow {
                    log_decision_summary(
                        run_id,
                        mode,
                        r.regime,
                        position_label(&st.position),
                        "WAIT",
                        "external_inventory_blocked",
                        Some((st.trades_today, max_trades_per_day)),
                        Some(risk::drawdown_fraction(st.peak_equity_usdt, st.equity_usdt)),
                        Some(daily_loss_fraction(st.daily_loss_start_equity_usdt, st.equity_usdt)),
                        Some(risk::drawdown_fraction(st.peak_equity_usdt, st.equity_usdt) >= 0.10),
                    );
                    return Ok(CoreOutcome {
                        did_place_order: false,
                        mode,
                        regime: r.regime,
                        decision: "WAIT".to_string(),
                        reason: "I will not touch BTC I did not buy.".to_string(),
                        end: "no money moved".to_string(),
                    });
                }

                // Explicit emergency sell for external inventory (LIVE): only when BOTH switches are enabled.
                if env_flag("BOT_PANIC_FLATTEN") {
                    let sell_qty = sizing::round_down_to_step(bals_initial.btc_free, md.step_size);
                    let qty_precision = qty_precision_from_step(md.step_size);
                    if sell_qty <= 0.0 {
                        return Ok(CoreOutcome {
                            did_place_order: false,
                            mode,
                            regime: r.regime,
                            decision: "WAIT".to_string(),
                            reason: "The BTC amount is too small to sell.".to_string(),
                            end: "no money moved".to_string(),
                        });
                    }
                    if sizing::ensure_min_notional(md.price, sell_qty, md.min_notional).is_err() {
                        return Ok(CoreOutcome {
                            did_place_order: false,
                            mode,
                            regime: r.regime,
                            decision: "WAIT".to_string(),
                            reason: "We cannot sell yet. It is below Binance minimum.".to_string(),
                            end: "no money moved".to_string(),
                        });
                    }

                    if let Err(e) = execution::execute_sell_market(
                        client,
                        mode,
                        &cfg.api_key,
                        &cfg.api_secret,
                        &cfg.base_url,
                        symbol,
                        sell_qty,
                        qty_precision,
                    )
                    .await
                    {
                        if is_binance_code(&e, -2015) {
                            log_say::say_reason("Binance rejected the signed order request.");
                            log_actions_for_2015();
                            log_ip_whitelist_hint(&public_ip);
                        }
                        return Err(e);
                    }

                    st.exit_to_flat_with_cooldown(md.now_ms);
                    persist_state(&cfg.state_path, &st)?;
                    log_decision_summary(
                        run_id,
                        mode,
                        r.regime,
                        position_label(&st.position),
                        "PANIC_FLATTEN",
                        "external_inventory_emergency_sell",
                        Some((st.trades_today, max_trades_per_day)),
                        Some(risk::drawdown_fraction(st.peak_equity_usdt, st.equity_usdt)),
                        Some(daily_loss_fraction(st.daily_loss_start_equity_usdt, st.equity_usdt)),
                        Some(risk::drawdown_fraction(st.peak_equity_usdt, st.equity_usdt) >= 0.10),
                    );
                    return Ok(CoreOutcome {
                        did_place_order: true,
                        mode,
                        regime: r.regime,
                        decision: "SELL".to_string(),
                        reason: "Emergency sell. We flatten.".to_string(),
                        end: "real order sent".to_string(),
                    });
                }

                // Takeover path: adopt external BTC into a managed Long position.
                // This prevents a permanent WAIT loop and enables the normal stop/exit logic.
                let take_qty = sizing::round_down_to_step(bals.btc_total(), md.step_size);
                if take_qty <= 0.0 {
                    return Ok(CoreOutcome {
                        did_place_order: false,
                        mode,
                        regime: r.regime,
                        decision: "WAIT".to_string(),
                        reason: "External BTC is too small to manage.".to_string(),
                        end: "no money moved".to_string(),
                    });
                }
                if sizing::ensure_min_notional(md.price, take_qty, md.min_notional).is_err() {
                    return Ok(CoreOutcome {
                        did_place_order: false,
                        mode,
                        regime: r.regime,
                        decision: "WAIT".to_string(),
                        reason: "External BTC is below Binance minimum.".to_string(),
                        end: "no money moved".to_string(),
                    });
                }

                let atr = f.atr14_5m.max(1e-12);
                let mut stop = md.price - 1.8 * atr;
                if !(stop.is_finite() && stop > 0.0) {
                    stop = (md.price - md.tick_size).max(md.tick_size);
                }
                if stop >= md.price {
                    stop = (md.price - md.tick_size).max(md.tick_size);
                }
                info!(
                    "Takeover enabled: adopting external BTC into managed position: qty={} entry={} stop={}.",
                    fmt_btc(take_qty),
                    fmt_usdt(md.price),
                    fmt_usdt(stop)
                );
                st.enter_long(md.price, take_qty, stop, None, md.now_ms);
                persist_state(&cfg.state_path, &st)?;
            }
        }
    }

    // Execution safety caps (needed for summaries/guards).
    let max_notional_fraction = env_f64("BOT_MAX_TRADE_NOTIONAL_FRACTION").unwrap_or(0.20);
    let max_notional_usdt_abs = env_f64("BOT_MAX_TRADE_NOTIONAL_USDT");

    #[allow(unused_assignments)]
    let mut decision_action = "WAIT".to_string();
    #[allow(unused_assignments)]
    let mut decision_reason = String::new();

    if st.is_dead {
        log_say::say_action("We lost 20%. The bot is dead.");
        log_decision_summary(
            run_id,
            mode,
            r.regime,
            position_label(&st.position),
            "STOP",
            "bot_is_dead",
            Some((st.trades_today, max_trades_per_day)),
            Some(risk::drawdown_fraction(st.peak_equity_usdt, st.equity_usdt)),
            Some(daily_loss_fraction(st.daily_loss_start_equity_usdt, st.equity_usdt)),
            Some(risk::drawdown_fraction(st.peak_equity_usdt, st.equity_usdt) >= 0.10),
        );
        return Ok(CoreOutcome {
            did_place_order: false,
            mode,
            regime: r.regime,
            decision: "STOP".to_string(),
            reason: "We lost too much. Bot is dead.".to_string(),
            end: "no money moved".to_string(),
        });
    }

    if st.in_hibernation(md.now_ms) {
        info!("We are resting now. We wait.");
        log_decision_summary(
            run_id,
            mode,
            r.regime,
            position_label(&st.position),
            "WAIT",
            "hibernation",
            Some((st.trades_today, max_trades_per_day)),
            Some(risk::drawdown_fraction(st.peak_equity_usdt, st.equity_usdt)),
            Some(daily_loss_fraction(st.daily_loss_start_equity_usdt, st.equity_usdt)),
            Some(risk::drawdown_fraction(st.peak_equity_usdt, st.equity_usdt) >= 0.10),
        );
        return Ok(CoreOutcome {
            did_place_order: false,
            mode,
            regime: r.regime,
            decision: "WAIT".to_string(),
            reason: "We wait. Not time yet.".to_string(),
            end: "no money moved".to_string(),
        });
    }

    if st.in_cooldown(md.now_ms) {
        info!("We just finished a trip. We cool down first.");
        log_decision_summary(
            run_id,
            mode,
            r.regime,
            position_label(&st.position),
            "WAIT",
            "cooldown",
            Some((st.trades_today, max_trades_per_day)),
            Some(risk::drawdown_fraction(st.peak_equity_usdt, st.equity_usdt)),
            Some(daily_loss_fraction(st.daily_loss_start_equity_usdt, st.equity_usdt)),
            Some(risk::drawdown_fraction(st.peak_equity_usdt, st.equity_usdt) >= 0.10),
        );
        return Ok(CoreOutcome {
            did_place_order: false,
            mode,
            regime: r.regime,
            decision: "WAIT".to_string(),
            reason: "We wait. Not time yet.".to_string(),
            end: "no money moved".to_string(),
        });
    }

    let qty_precision = qty_precision_from_step(md.step_size);
    let price_precision = price_precision_from_tick(md.tick_size);
    let mut did_place_order = false;

    match st.position.clone() {
        state::Position::Flat => {
            if md.stale_any {
                info!("We skip trading because candle data looks stale.");
                log_decision_summary(
                    run_id,
                    mode,
                    r.regime,
                    position_label(&st.position),
                    "WAIT",
                    "stale_candles",
                    Some((st.trades_today, max_trades_per_day)),
                    Some(risk::drawdown_fraction(st.peak_equity_usdt, st.equity_usdt)),
                    Some(daily_loss_fraction(st.daily_loss_start_equity_usdt, st.equity_usdt)),
                    Some(risk::drawdown_fraction(st.peak_equity_usdt, st.equity_usdt) >= 0.10),
                );
                return Ok(CoreOutcome {
                    did_place_order: false,
                    mode,
                    regime: r.regime,
                    decision: "WAIT".to_string(),
                    reason: format!(
                        "Market data is delayed (5m: {}m, 1h: {}m). Waiting for fresh data.",
                        md.lag_5m_ms / 60_000,
                        md.lag_1h_ms / 60_000
                    ),
                    end: "no money moved".to_string(),
                });
            }

            if st.trades_today >= max_trades_per_day {
                info!("We already made enough trades today. We rest.");
                log_decision_summary(
                    run_id,
                    mode,
                    r.regime,
                    position_label(&st.position),
                    "WAIT",
                    "max_trades_per_day",
                    Some((st.trades_today, max_trades_per_day)),
                    Some(risk::drawdown_fraction(st.peak_equity_usdt, st.equity_usdt)),
                    Some(daily_loss_fraction(st.daily_loss_start_equity_usdt, st.equity_usdt)),
                    Some(risk::drawdown_fraction(st.peak_equity_usdt, st.equity_usdt) >= 0.10),
                );
                return Ok(CoreOutcome {
                    did_place_order: false,
                    mode,
                    regime: r.regime,
                    decision: "WAIT".to_string(),
                    reason: "Daily trade limit reached. Resting until the next session.".to_string(),
                    end: "no money moved".to_string(),
                });
            }

            let sig = signals::entry_long_signal(r.regime, &f, md.price, r.vol_ratio, r.vol_squeeze, r.htf_bullish, r.bearish_bias);
            if sig.action == signals::Action::EnterLong {
                let stop = sig.stop_price.ok_or_else(|| anyhow!("Signal had no stop"))?;

                // Max notional cap: do not allow a single entry to be too large.
                // Default: 20% of equity, optionally clamped by absolute cap.
                // Computed up front so it can be applied *inside* risk::size_entry_long,
                // before the min-notional/affordability gate. Capping only after that gate
                // (the previous behavior) rejected trades whenever the uncapped risk-parity
                // notional exceeded free USDT, even when the capped size the bot would have
                // used anyway was affordable. On small accounts this discarded the large
                // majority of otherwise-tradeable signals.
                let mut cap = (st.equity_usdt * max_notional_fraction).max(0.0);
                if let Some(abs) = max_notional_usdt_abs {
                    cap = cap.min(abs.max(0.0));
                }

                let sized = risk::size_entry_long(
                    r.regime,
                    st.equity_usdt,
                    bals.usdt_free,
                    st.peak_equity_usdt,
                    st.daily_loss_start_equity_usdt,
                    st.session_pnl_usdt,
                    md.price,
                    stop,
                    md.step_size,
                    md.min_notional,
                    risk_fraction_multiplier,
                    cap,
                );

                match sized {
                    risk::RiskDecision::Block { reason, hibernate } => {
                        info!("We skip this trade. {}", reason);
                        decision_action = "BLOCK_ENTRY".to_string();
                        decision_reason = reason.clone();
                        if hibernate {
                            st.hibernation_until_ms = md.now_ms + 24 * 60 * 60 * 1000;
                            persist_state(&cfg.state_path, &st)?;
                            info!("We rest to survive.");
                            decision_action = "HIBERNATE".to_string();
                        }
                    }
                    risk::RiskDecision::Allow {
                        qty,
                        notional,
                        was_capped,
                        ..
                    } => {
                        let taker_fee_bps = env_f64("BOT_TAKER_FEE_BPS").unwrap_or(10.0);
                        let slippage_bps = env_f64("BOT_SLIPPAGE_BPS").unwrap_or(20.0);
                        let dd = risk::drawdown_fraction(st.peak_equity_usdt, st.equity_usdt);
                        let dl = daily_loss_fraction(st.daily_loss_start_equity_usdt, st.equity_usdt);
                        let throttled = dd >= 0.10;
                        let base_risk_frac =
                            risk::risk_fraction_for_regime(r.regime) * risk_fraction_multiplier;
                        let effective_risk_frac = if throttled {
                            base_risk_frac * 0.25
                        } else {
                            base_risk_frac
                        };
                        let stop_dist = (md.price - stop).max(0.0);

                        debug!(
                            "Audit entry: run_id={} atr14_5m={} stop_dist={} risk_frac_base={:.4}% risk_frac_eff={:.4}% qty={} notional={} cap={} capped={} min_notional={} dd={:.2}% daily_loss={:.2}% trades_today={}/{}",
                            run_id,
                            fmt_usdt(f.atr14_5m),
                            fmt_usdt(stop_dist),
                            pct(base_risk_frac),
                            pct(effective_risk_frac),
                            fmt_btc(qty),
                            fmt_usdt(notional),
                            fmt_usdt(cap),
                            if was_capped { "yes" } else { "no" },
                            fmt_usdt(md.min_notional),
                            pct(dd),
                            pct(dl),
                            st.trades_today,
                            max_trades_per_day
                        );
                        audit_emit_json_with_run_id(
                            run_id,
                            serde_json::json!({
                                "event": "entry_sizing",
                                "mode": format!("{:?}", mode),
                                "regime": format!("{:?}", r.regime),
                                "atr14_5m": f.atr14_5m,
                                "stop_dist": stop_dist,
                                "price": md.price,
                                "stop": stop,
                                "risk_frac_base": base_risk_frac,
                                "risk_frac_effective": effective_risk_frac,
                                "qty": qty,
                                "notional": notional,
                                "cap_usdt": cap,
                                "was_capped": was_capped,
                                "min_notional": md.min_notional,
                                "drawdown_frac": dd,
                                "daily_loss_frac": dl,
                                "trades_today": st.trades_today,
                                "max_trades_per_day": max_trades_per_day
                            }),
                        );

                        if sizing::ensure_min_notional(md.price, qty, md.min_notional).is_err() {
                            info!("We skip this trade. The capped size is too small for Binance.");
                            log_decision_summary(
                                run_id,
                                mode,
                                r.regime,
                                position_label(&st.position),
                                "BLOCK_ENTRY",
                                "capped_size_below_min_notional",
                                Some((st.trades_today, max_trades_per_day)),
                                Some(dd),
                                Some(dl),
                                Some(throttled),
                            );
                            return Ok(CoreOutcome {
                                did_place_order: false,
                                mode,
                                regime: r.regime,
                                decision: "WAIT".to_string(),
                                reason: "We wait. This trade would be too small for Binance."
                                    .to_string(),
                                end: "no money moved".to_string(),
                            });
                        }

                        info!(
                            "We plan to buy about {} USDT worth ({} BTC).",
                            fmt_usdt(notional),
                            fmt_btc(qty)
                        );
                        info!("Safety line (stop) is around {} USDT.", fmt_usdt(stop));

                        let order_id = match execution::execute_buy_market(
                            client,
                            mode,
                            &cfg.api_key,
                            &cfg.api_secret,
                            &cfg.base_url,
                            symbol,
                            qty,
                            qty_precision,
                        )
                        .await
                        {
                            Ok(oid) => oid,
                            Err(e) => {
                                if is_binance_code(&e, -2015) {
                                    info!("Reason: Binance rejected a signed order request (code -2015).");
                                    log_actions_for_2015();
                                    log_ip_whitelist_hint(&public_ip);
                                }
                                return Err(e);
                            }
                        };

                        did_place_order = true;
                        decision_action = "ENTER_LONG".to_string();
                        decision_reason = format!(
                            "{} | plan qty={} notional={} minNotional=PASS fee~{}bps slip~{}bps",
                            sig.reason.trim(),
                            fmt_btc(qty),
                            fmt_usdt(notional),
                            taker_fee_bps,
                            slippage_bps
                        );
                        if mode == Mode::Practice {
                            info!("This was just practice, no money moved.");
                        } else {
                            info!("We bought a small piece.");
                        }

                        // Update state from exchange truth when LIVE.
                        let mut entry_price = md.price;
                        let mut entry_qty = qty;
                        if mode == Mode::Live {
                            if let Some(oid) = order_id {
                                let st_order = binance_orders::get_order(
                                    client,
                                    &cfg.api_key,
                                    &cfg.api_secret,
                                    &cfg.base_url,
                                    symbol,
                                    oid,
                                )
                                .await
                                .inspect_err(|e| {
                                    if is_binance_code(e, -2015) {
                                        info!("Reason: Binance rejected a signed order request (code -2015).");
                                        log_actions_for_2015();
                                        log_ip_whitelist_hint(&public_ip);
                                    }
                                })?;
                                if let Some(avg) = avg_price_from_order_status(&st_order)? {
                                    entry_price = avg;
                                }
                                let exec_qty =
                                    parse_f64_field("executedQty", &st_order.executed_qty)?;
                                if exec_qty > 0.0 {
                                    entry_qty = exec_qty;
                                }

                                let trades = binance_orders::my_trades_for_order(
                                    client,
                                    &cfg.api_key,
                                    &cfg.api_secret,
                                    &cfg.base_url,
                                    symbol,
                                    oid,
                                )
                                .await
                                .unwrap_or_else(|e| {
                                    if is_binance_code(&e, -2015) {
                                        info!("Reason: Binance rejected a signed order request (code -2015).");
                                        log_actions_for_2015();
                                        log_ip_whitelist_hint(&public_ip);
                                    }
                                    Vec::new()
                                });
                                if !trades.is_empty() {
                                    if let Ok(fee) = fee_usdt_from_trades(&trades, entry_price) {
                                        if fee > 0.0 {
                                            st.apply_fee(fee);
                                        }
                                    }
                                }
                            }
                        }

                        // Update local state position.
                        // Implement hard take-profit at 3:1 reward-to-risk
                        let tp = md.price + 3.0 * (md.price - stop);
                        st.enter_long(entry_price, entry_qty, stop, Some(tp), md.now_ms);
                        st.bump_trades_today();

                        // Telemetry: record the entry event.
                        {
                            let snap = telemetry::FeatureSnapshot::from_features_and_regime(&f, &r);
                            let telem = telemetry::TelemetryWriter::from_env();
                            let rec = telemetry::TradeRecord::open(
                                symbol,
                                &format!("{:?}", r.regime),
                                &sig.strategy,
                                sig.score,
                                md.now_ms,
                                entry_price,
                                entry_qty,
                                stop,
                                Some(tp),
                                0.0, // live fee recorded separately via apply_fee
                                snap,
                            );
                            telem.emit_trade(&rec);
                        }

                        // Exchange-side protection (LIVE): place a stop-loss limit sell right away.
                        // If we cannot place it, we immediately flatten to reduce catastrophic risk.
                        if mode == Mode::Live {
                            // Set limit a bit below stop to help it fill when triggered.
                            let limit_price = (stop * 0.998).max(0.01);
                            match execution::place_stop_loss_limit_sell(
                                client,
                                mode,
                                &cfg.api_key,
                                &cfg.api_secret,
                                &cfg.base_url,
                                symbol,
                                entry_qty,
                                qty_precision,
                                stop,
                                limit_price,
                                price_precision,
                            )
                            .await
                            {
                                Ok(Some(stop_order_id)) => {
                                    st.set_stop_order(stop_order_id, stop);
                                    info!("We placed a real safety stop on the exchange.");
                                }
                                Ok(None) => {
                                    info!("Exchange protection returned no order id; panic flattening.");

                                    // Best-effort panic flatten: sell what we believe we bought.
                                    let sell_qty = sizing::round_down_to_step(entry_qty, md.step_size);
                                    if sell_qty > 0.0 {
                                        let sell_res = execution::execute_sell_market(
                                            client,
                                            mode,
                                            &cfg.api_key,
                                            &cfg.api_secret,
                                            &cfg.base_url,
                                            symbol,
                                            sell_qty,
                                            qty_precision,
                                        )
                                        .await;
                                        if let Err(se) = sell_res {
                                            if is_binance_code(&se, -2015) {
                                                info!("Reason: Binance rejected a signed order request (code -2015).");
                                                log_actions_for_2015();
                                                log_ip_whitelist_hint(&public_ip);
                                            }
                                        }
                                    }
                                    st.exit_to_flat_with_cooldown(md.now_ms);
                                    persist_state(&cfg.state_path, &st)?;
                                    log_decision_summary(
                                        run_id,
                                        mode,
                                        r.regime,
                                        position_label(&st.position),
                                        "PANIC_FLATTEN",
                                        "failed_to_place_exchange_stop",
                                        Some((st.trades_today, max_trades_per_day)),
                                        Some(risk::drawdown_fraction(st.peak_equity_usdt, st.equity_usdt)),
                                        Some(daily_loss_fraction(st.daily_loss_start_equity_usdt, st.equity_usdt)),
                                        Some(risk::drawdown_fraction(st.peak_equity_usdt, st.equity_usdt) >= 0.10),
                                    );
                                    return Ok(CoreOutcome {
                                        did_place_order: true,
                                        mode,
                                        regime: r.regime,
                                        decision: "SELL".to_string(),
                                        reason: "I failed to place protection. I sold to stay safe."
                                            .to_string(),
                                        end: "real order sent".to_string(),
                                    });
                                }
                                Err(e) => {
                                    info!("Exchange protection failed; panic flattening.");

                                    if is_binance_code(&e, -2015) {
                                        info!("Reason: Binance rejected a signed order request (code -2015).");
                                        log_actions_for_2015();
                                        log_ip_whitelist_hint(&public_ip);
                                    }

                                    // Best-effort panic flatten: sell what we believe we bought.
                                    let sell_qty = sizing::round_down_to_step(entry_qty, md.step_size);
                                    if sell_qty > 0.0 {
                                        let sell_res = execution::execute_sell_market(
                                            client,
                                            mode,
                                            &cfg.api_key,
                                            &cfg.api_secret,
                                            &cfg.base_url,
                                            symbol,
                                            sell_qty,
                                            qty_precision,
                                        )
                                        .await;
                                        if let Err(se) = sell_res {
                                            if is_binance_code(&se, -2015) {
                                                info!("Reason: Binance rejected a signed order request (code -2015).");
                                                log_actions_for_2015();
                                                log_ip_whitelist_hint(&public_ip);
                                            }
                                        }
                                    }
                                    st.exit_to_flat_with_cooldown(md.now_ms);
                                    persist_state(&cfg.state_path, &st)?;
                                    log_decision_summary(
                                        run_id,
                                        mode,
                                        r.regime,
                                        position_label(&st.position),
                                        "PANIC_FLATTEN",
                                        "failed_to_place_exchange_stop",
                                        Some((st.trades_today, max_trades_per_day)),
                                        Some(risk::drawdown_fraction(st.peak_equity_usdt, st.equity_usdt)),
                                        Some(daily_loss_fraction(st.daily_loss_start_equity_usdt, st.equity_usdt)),
                                        Some(risk::drawdown_fraction(st.peak_equity_usdt, st.equity_usdt) >= 0.10),
                                    );
                                    return Ok(CoreOutcome {
                                        did_place_order: true,
                                        mode,
                                        regime: r.regime,
                                        decision: "SELL".to_string(),
                                        reason: "I failed to place protection. I sold to stay safe."
                                            .to_string(),
                                        end: "real order sent".to_string(),
                                    });
                                }
                            }
                        }

                        persist_state(&cfg.state_path, &st)?;
                    }
                }
            } else {
                info!("No trade: {}. We wait.", sig.reason);
                decision_action = "WAIT".to_string();
                decision_reason = sig.reason;
            }
        }
        state::Position::ExternalInventory { .. } => {
            // Should be handled earlier by explicit gating.
            info!("We wait. No money moved.");
            decision_action = "WAIT".to_string();
            decision_reason = "external_inventory".to_string();
        }
        state::Position::Long {
            qty,
            entry_price,
            stop_order_id,
            ..
        } => {
            // Stop-loss lifecycle reconciliation (LIVE only):
            // - If the stop was filled externally, flatten memory and set cooldown.
            // - If the stop was canceled/expired, clear id so we can re-place.
            // - If trailing stop moved up, replace the stop order.
            let mut existing_stop_price: Option<f64> = None;
            if have_keys && mode == Mode::Live {
                if let Some(oid) = stop_order_id {
                    match binance_orders::get_order(
                        client,
                        &cfg.api_key,
                        &cfg.api_secret,
                        &cfg.base_url,
                        symbol,
                        oid,
                    )
                    .await
                    {
                        Ok(st_order) => {
                            if st_order.status == "FILLED" {
                                info!("Exchange stop filled. We are flat now.");
                                if let state::Position::Long { entry_time_ms: et, entry_price: ep, qty: q, stop_price: sp, .. } = st.position {
                                    telemetry::TelemetryWriter::from_env().emit_exit(
                                        symbol, et, ep, md.now_ms, sp, "exchange_stop_filled", q,
                                    );
                                }
                                st.exit_to_flat_with_cooldown(md.now_ms);
                                st.clear_last_stop_order_id();
                                persist_state(&cfg.state_path, &st)?;
                                log_decision_summary(
                                    run_id,
                                    mode,
                                    r.regime,
                                    position_label(&st.position),
                                    "WAIT",
                                    "exchange_stop_filled",
                                    Some((st.trades_today, max_trades_per_day)),
                                    Some(risk::drawdown_fraction(st.peak_equity_usdt, st.equity_usdt)),
                                    Some(daily_loss_fraction(st.daily_loss_start_equity_usdt, st.equity_usdt)),
                                    Some(risk::drawdown_fraction(st.peak_equity_usdt, st.equity_usdt) >= 0.10),
                                );
                                return Ok(CoreOutcome {
                                    did_place_order: false,
                                    mode,
                                    regime: r.regime,
                                    decision: "WAIT".to_string(),
                                    reason: "The exchange stop already sold the position. We rest.".to_string(),
                                    end: "no money moved".to_string(),
                                });
                            }

                            if matches!(st_order.status.as_str(), "CANCELED" | "EXPIRED" | "REJECTED") {
                                st.clear_stop_order_id_in_position();
                            }

                            if let Some(sp) = st_order.stop_price.as_deref() {
                                if let Ok(p) = parse_f64_field("stopPrice", sp) {
                                    existing_stop_price = Some(p);
                                    st.set_exchange_stop_price_from_exchange(p);
                                }
                            }
                        }
                        Err(e) => {
                            if is_binance_code(&e, -2015) {
                                debug!("Cannot reconcile stop order (auth rejected).");
                            } else {
                                debug!("Stop order status check failed: {}", e);
                            }
                        }
                    }
                }
            }

            if existing_stop_price.is_none() {
                existing_stop_price = match &st.position {
                    state::Position::Long {
                        exchange_stop_price: Some(p),
                        ..
                    } => Some(*p),
                    _ => None,
                };
            }

            // Defined ranging exit for mean reversion: take profit at mid-band with RSI
            // recovery — but only once price has actually cleared entry plus round-trip
            // costs. Bug fix: bb_mid20_5m is a live 20-period SMA that drifts with price,
            // so "price reclaimed the mid-band" does not imply "price recovered above what
            // we paid" — a 90-day backtest showed this exit firing on ~45% of all trades
            // at an average of -0.10% net, i.e. it was routinely realizing a loss under a
            // "take profit" label. Gate it on the higher of the mid-band and a cost-aware
            // breakeven line instead.
            let taker_fee_bps = env_f64("BOT_TAKER_FEE_BPS").unwrap_or(10.0);
            let slippage_bps = env_f64("BOT_SLIPPAGE_BPS").unwrap_or(20.0);
            let round_trip_cost_frac = (2.0 * taker_fee_bps + 2.0 * slippage_bps) / 10_000.0;
            let breakeven_price = entry_price * (1.0 + round_trip_cost_frac);
            let range_tp = r.regime == Regime::Ranging
                && md.price >= f.bb_mid20_5m.max(breakeven_price)
                && f.rsi14_5m >= 50.0;

            let (decision, why) = if range_tp {
                (
                    state::PositionDecision::ExitLong,
                    "Mean reversion done. We take profit.".to_string(),
                )
            } else {
                st.manage_open_position(md.price, f.atr14_5m, md.now_ms, r.bearish_bias)
            };
            if decision == state::PositionDecision::ExitLong {
                info!("{}", &why);
                debug!(
                    "Audit exit: run_id={} qty={} entry={} now={} atr14_5m={} reason={}",
                    run_id,
                    fmt_btc(qty),
                    fmt_usdt(entry_price),
                    fmt_usdt(md.price),
                    fmt_usdt(f.atr14_5m),
                    &why
                );
                audit_emit_json_with_run_id(
                    run_id,
                    serde_json::json!({
                        "event": "exit_decision",
                        "mode": format!("{:?}", mode),
                        "regime": format!("{:?}", r.regime),
                        "qty": qty,
                        "entry_price": entry_price,
                        "price": md.price,
                        "atr14_5m": f.atr14_5m,
                        "reason": &why
                    }),
                );

                // Use min(qty, btc_free) for safety
                let sell_qty = sizing::round_down_to_step(qty.min(bals.btc_total()), md.step_size);
                if sell_qty <= 0.0 {
                    info!("We cannot sell now. Your BTC is too small.");
                    decision_action = "WAIT".to_string();
                    decision_reason = "We want to sell, but the amount is too small.".to_string();
                } else {
                    // Ensure Binance min_notional is met; if not, do not trade.
                    if sizing::ensure_min_notional(md.price, sell_qty, md.min_notional).is_err() {
                        info!("We cannot sell now. Binance says it is too small.");
                        decision_action = "WAIT".to_string();
                        decision_reason =
                            "We want to sell, but Binance minimum blocks it.".to_string();
                    } else {
                        // If we have an exchange stop order, cancel it before selling market to avoid double-sell.
                        if have_keys && mode == Mode::Live {
                            let stop_oid_to_cancel = match &st.position {
                                state::Position::Long {
                                    stop_order_id: Some(oid),
                                    ..
                                } => Some(*oid),
                                _ => None,
                            };
                            if let Some(oid) = stop_oid_to_cancel {
                                let _ = execution::cancel_order(
                                    client,
                                    mode,
                                    &cfg.api_key,
                                    &cfg.api_secret,
                                    &cfg.base_url,
                                    symbol,
                                    oid,
                                )
                                .await;
                                st.clear_stop_order_id_in_position();
                                persist_state(&cfg.state_path, &st)?;
                            }
                        }

                        let sell_order_id = match execution::execute_sell_market(
                            client,
                            mode,
                            &cfg.api_key,
                            &cfg.api_secret,
                            &cfg.base_url,
                            symbol,
                            sell_qty,
                            qty_precision,
                        )
                        .await
                        {
                            Ok(oid) => oid,
                            Err(e) => {
                                if is_binance_code(&e, -2015) {
                                    info!("Reason: Binance rejected a signed order request (code -2015).");
                                    log_actions_for_2015();
                                    log_ip_whitelist_hint(&public_ip);
                                }
                                return Err(e);
                            }
                        };

                        did_place_order = true;
                        decision_action = "EXIT_LONG".to_string();
                        if why.contains("Stop hit") {
                            decision_reason = "Stop hit. We exit.".to_string();
                        } else {
                            decision_reason = why.clone();
                        }
                        if mode == Mode::Practice {
                            info!("This was just practice, no money moved.");
                        } else {
                            info!("We sold a small piece.");
                        }

                        // Truth update for LIVE: compute realized pnl/fees best-effort.
                        if mode == Mode::Live {
                            if let Some(oid) = sell_order_id {
                                if let Ok(st_order) = binance_orders::get_order(
                                    client,
                                    &cfg.api_key,
                                    &cfg.api_secret,
                                    &cfg.base_url,
                                    symbol,
                                    oid,
                                )
                                .await
                                {
                                    let exec_qty = parse_f64_field(
                                        "executedQty",
                                        &st_order.executed_qty,
                                    )
                                    .unwrap_or(0.0);
                                    let avg_sell = avg_price_from_order_status(&st_order)
                                        .ok()
                                        .flatten()
                                        .unwrap_or(md.price);

                                    let trades = binance_orders::my_trades_for_order(
                                        client,
                                        &cfg.api_key,
                                        &cfg.api_secret,
                                        &cfg.base_url,
                                        symbol,
                                        oid,
                                    )
                                    .await
                                    .unwrap_or_default();
                                    if let Ok(fee) = fee_usdt_from_trades(&trades, avg_sell) {
                                        if fee > 0.0 {
                                            st.apply_fee(fee);
                                        }
                                    }

                                    if exec_qty > 0.0 {
                                        let pnl = (avg_sell - entry_price) * exec_qty;
                                        st.realized_pnl_usdt += pnl;
                                    }
                                }
                            }
                        }

                        // Telemetry: record the exit event.
                        if let state::Position::Long { entry_time_ms: et, entry_price: ep, qty: q, .. } = st.position {
                            telemetry::TelemetryWriter::from_env().emit_exit(
                                symbol, et, ep, md.now_ms, md.price, &why, q,
                            );
                        }
                        st.exit_to_flat_with_cooldown(md.now_ms);
                        persist_state(&cfg.state_path, &st)?;
                    }
                }
            } else {
                info!("{}", &why);
                decision_action = "HOLD_LONG".to_string();
                decision_reason = why;

                // Ensure we have exchange-side protection while holding a LIVE position.
                if have_keys && mode == Mode::Live {
                    // If we have BTC locked but no tracked stop order id, try to discover an
                    // existing stop on the exchange so we don't fail with "insufficient balance".
                    if let state::Position::Long {
                        stop_order_id: None,
                        ..
                    } = &st.position
                    {
                        if bals.btc_locked > BTC_DUST {
                            if let Ok(orders) = binance_orders::open_orders(
                                client,
                                &cfg.api_key,
                                &cfg.api_secret,
                                &cfg.base_url,
                                symbol,
                            )
                            .await
                            {
                                let mut best: Option<binance_orders::OpenOrder> = None;
                                for o in orders {
                                    let is_sell = o.side.as_deref() == Some("SELL");
                                    let is_stop = matches!(
                                        o.order_type.as_deref(),
                                        Some("STOP_LOSS_LIMIT") | Some("STOP_LOSS")
                                    );
                                    let is_open = !matches!(
                                        o.status.as_deref(),
                                        Some("FILLED") | Some("CANCELED") | Some("EXPIRED") | Some("REJECTED")
                                    );
                                    if is_sell && is_stop && is_open {
                                        let take = match &best {
                                            None => true,
                                            Some(b) => o.order_id > b.order_id,
                                        };
                                        if take {
                                            best = Some(o);
                                        }
                                    }
                                }

                                if let Some(found) = best {
                                    if let Some(sp) = found.stop_price.as_deref() {
                                        if let Ok(p) = parse_f64_field("stopPrice", sp) {
                                            info!(
                                                "Discovered existing exchange stop order: order_id={} stopPrice={}",
                                                found.order_id,
                                                fmt_usdt(p)
                                            );
                                            st.set_stop_order(found.order_id, p);
                                            let _ = persist_state(&cfg.state_path, &st);
                                        }
                                    } else {
                                        info!(
                                            "Discovered existing exchange stop order: order_id={} (stopPrice missing)",
                                            found.order_id
                                        );
                                        if let state::Position::Long { stop_order_id, .. } =
                                            &mut st.position
                                        {
                                            *stop_order_id = Some(found.order_id);
                                        }
                                        let _ = persist_state(&cfg.state_path, &st);
                                    }
                                }
                            }
                        }
                    }

                    let (st_qty, desired_stop, current_oid) = match &st.position {
                        state::Position::Long {
                            qty,
                            stop_price,
                            stop_order_id,
                            ..
                        } => (*qty, *stop_price, *stop_order_id),
                        _ => (0.0, 0.0, None),
                    };

                    if st_qty > 0.0 && desired_stop > 0.0 {
                        let protect_qty =
                            sizing::round_down_to_step(st_qty.min(bals.btc_total()), md.step_size);
                        if protect_qty > 0.0 {
                            let limit_price = (desired_stop * 0.998).max(0.01);

                            let should_replace = existing_stop_price
                                .map(|old| desired_stop > old + md.tick_size)
                                .unwrap_or(false);

                            if current_oid.is_none() || should_replace {
                                if let Some(old_oid) = current_oid {
                                    let _ = execution::cancel_order(
                                        client,
                                        mode,
                                        &cfg.api_key,
                                        &cfg.api_secret,
                                        &cfg.base_url,
                                        symbol,
                                        old_oid,
                                    )
                                    .await;
                                    st.clear_stop_order_id_in_position();
                                }

                                match execution::place_stop_loss_limit_sell(
                                    client,
                                    mode,
                                    &cfg.api_key,
                                    &cfg.api_secret,
                                    &cfg.base_url,
                                    symbol,
                                    protect_qty,
                                    qty_precision,
                                    desired_stop,
                                    limit_price,
                                    price_precision,
                                )
                                .await
                                {
                                    Ok(Some(new_oid)) => {
                                        st.set_stop_order(new_oid, desired_stop);
                                        persist_state(&cfg.state_path, &st)?;
                                        debug!("Exchange stop reconciled: order_id={}", new_oid);
                                    }
                                    Ok(None) => {
                                        debug!("Stop placement returned no order id.");
                                    }
                                    Err(e) => {
                                        if is_binance_code(&e, -2015) {
                                            debug!("Stop placement rejected (auth).");
                                        } else {
                                            debug!("Stop placement failed: {}", e);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // End-of-run wallet refresh: only when LIVE and we actually placed a real order.
    // In PRACTICE we never fetch balances twice.
    if have_keys && mode == Mode::Live && did_place_order {
        match account::fetch_spot_balances(client, &cfg.api_key, &cfg.api_secret, &cfg.base_url)
            .await
        {
            Ok(bals2) => {
                let equity2 = bals2.usdt_total() + (bals2.btc_total() * md.price);
                st.sync_equity_and_day(equity2);
                persist_state(&cfg.state_path, &st)?;
                log_say::say_wallet(bals2.usdt_free, bals2.btc_free, st.equity_usdt);
            }
            Err(e) if is_auth_error(&e) => {
                log_say::say_reason(
                    "I cannot re-check your wallet at the end. Binance rejected the key.",
                );
            }
            Err(e) => return Err(e),
        }
    } else {
        // Keep state synced to what we already saw.
        st.sync_equity_and_day(equity_usdt);
        let _ = persist_state(&cfg.state_path, &st);
    }

    // Keep IMPORTANT logs short; any extra hints belong in debug.
    if (decision_action == "WAIT" || decision_action == "BLOCK_ENTRY")
        && decision_reason == "no_signal"
    {
        debug!("No signal under {:?}.", r.regime);
    }

    // Advanced stats stay behind debug.
    log_decision_summary(
        run_id,
        mode,
        r.regime,
        position_label(&st.position),
        &decision_action,
        &decision_reason,
        Some((st.trades_today, max_trades_per_day)),
        Some(risk::drawdown_fraction(st.peak_equity_usdt, st.equity_usdt)),
        Some(daily_loss_fraction(
            st.daily_loss_start_equity_usdt,
            st.equity_usdt,
        )),
        Some(risk::drawdown_fraction(st.peak_equity_usdt, st.equity_usdt) >= 0.10),
    );

    audit_emit_json_with_run_id(
        run_id,
        serde_json::json!({
            "event": "decision_summary",
            "mode": format!("{:?}", mode),
            "regime": format!("{:?}", r.regime),
            "position": position_label(&st.position),
            "action": decision_action,
            "reason": decision_reason,
            "equity_usdt": st.equity_usdt,
            "peak_equity_usdt": st.peak_equity_usdt,
            "daily_loss_start_equity_usdt": st.daily_loss_start_equity_usdt,
            "drawdown_frac": risk::drawdown_fraction(st.peak_equity_usdt, st.equity_usdt),
            "daily_loss_frac": daily_loss_fraction(st.daily_loss_start_equity_usdt, st.equity_usdt),
            "trades_today": st.trades_today,
            "max_trades_per_day": max_trades_per_day
        }),
    );

    let end_msg = if mode == Mode::Practice {
        "no money moved".to_string()
    } else if did_place_order {
        "real order sent".to_string()
    } else {
        "no money moved".to_string()
    };

    // If debugging is enabled, emit a full indicator snapshot when we are WAITing.
    if (decision_action == "WAIT" || decision_action == "BLOCK_ENTRY") && env_flag("BOT_DEBUG") {
        let ts = format!("{:?}", std::time::SystemTime::now());
        debug!("BOT_DEBUG snapshot: run_id={} price={} features={:?}", run_id, md.price, f);
        if let Ok(mut fh) = std::fs::OpenOptions::new().create(true).append(true).open("debug_snapshots.log") {
            let _ = writeln!(fh, "{} - DEBUG_SNAPSHOT run_id={} price={} features={:?}", ts, run_id, md.price, f);
        }
    }

    // ── dashboard snapshot ────────────────────────────────────────────────────
    {
        let (pos_label, entry_p, qty_b, stop_p, tp_p, unrealised) = match &st.position {
            state::Position::Long {
                entry_price,
                qty,
                stop_price,
                tp_price,
                ..
            } => {
                let upnl = (md.price - entry_price) * qty;
                (
                    "Long".to_string(),
                    *entry_price,
                    *qty,
                    *stop_price,
                    tp_price.unwrap_or(0.0),
                    upnl,
                )
            }
            state::Position::ExternalInventory { .. } => {
                ("ExternalInventory".to_string(), 0.0, 0.0, 0.0, 0.0, 0.0)
            }
            state::Position::Flat => ("Flat".to_string(), 0.0, 0.0, 0.0, 0.0, 0.0),
        };
        let dd = if st.peak_equity_usdt > 0.0 {
            (st.peak_equity_usdt - st.equity_usdt) / st.peak_equity_usdt
        } else {
            0.0
        };

        // Read previous snapshot to carry forward trade history and detect closures.
        // The read guard is dropped before we write, so no deadlock.
        let (prev_pos, prev_entry, prev_qty, prev_rpnl, mut trades) =
            if let Ok(g) = snap.read() {
                match g.as_ref() {
                    Some(s) => (
                        s.position.clone(),
                        s.entry_price,
                        s.qty_btc,
                        s.realized_pnl_usdt,
                        s.recent_trades.clone(),
                    ),
                    None => ("Flat".to_string(), 0.0, 0.0, 0.0, vec![]),
                }
            } else {
                ("Flat".to_string(), 0.0, 0.0, 0.0, vec![])
            };

        // Detect Long → Flat: a trade just closed this tick.
        if prev_pos == "Long" && pos_label == "Flat" && prev_qty > 0.0 {
            let pnl = st.realized_pnl_usdt - prev_rpnl;
            trades.push(crate::dashboard::TradeRecord {
                closed_at_ms: md.now_ms,
                entry_price: prev_entry,
                exit_price: md.price,
                qty_btc: prev_qty,
                pnl_usdt: pnl,
                result: if pnl >= 0.0 {
                    "WIN".to_string()
                } else {
                    "LOSS".to_string()
                },
            });
            if trades.len() > 20 {
                trades.remove(0);
            }
        }

        let new_snap = crate::dashboard::DashboardSnapshot {
            captured_at_ms: md.now_ms,
            price_usdt: md.price,
            regime: format!("{:?}", r.regime),
            regime_reason: r.reason.clone(),
            position: pos_label,
            entry_price: entry_p,
            qty_btc: qty_b,
            stop_price: stop_p,
            tp_price: tp_p,
            unrealised_pnl_usdt: unrealised,
            usdt_free: bals.usdt_free,
            btc_free: bals.btc_free,
            equity_usdt: st.equity_usdt,
            session_pnl_usdt: st.session_pnl_usdt,
            peak_equity_usdt: st.peak_equity_usdt,
            drawdown_frac: dd,
            is_dead: st.is_dead,
            trades_today: st.trades_today,
            max_trades_per_day,
            in_cooldown: md.now_ms < st.cooldown_until_ms,
            in_hibernation: md.now_ms < st.hibernation_until_ms,
            atr14_5m: f.atr14_5m,
            rsi14_5m: f.rsi14_5m,
            rsi14_1m: f.rsi14_1m,
            bb_width_5m: f.bb_width_5m,
            volume_z: f.volume_z,
            velocity_1m: f.velocity_1m,
            velocity_5m: f.velocity_5m,
            ema20_1h: f.ema20_1h,
            ema50_1h: f.ema50_1h,
            ema200_1h: f.ema200_1h,
            atr_ratio_5m: f.atr_ratio_5m,
            htf_bullish: r.htf_bullish,
            bearish_bias: r.bearish_bias,
            vol_squeeze: r.vol_squeeze,
            last_action: decision_action.clone(),
            last_reason: decision_reason.clone(),
            mode: format!("{:?}", mode),
            realized_pnl_usdt: st.realized_pnl_usdt,
            recent_trades: trades,
        };
        if let Ok(mut guard) = snap.write() {
            *guard = Some(new_snap);
        }
    }

    Ok(CoreOutcome {
        did_place_order,
        mode,
        regime: r.regime,
        decision: decision_action,
        reason: decision_reason,
        end: end_msg,
    })
}
