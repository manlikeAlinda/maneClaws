use crate::candles::{self, Interval};
use crate::execution::{self, Mode};
use crate::regime::Regime;
use crate::{account, binance_orders, exchange_info, features, log_say, regime, risk, signals, sizing, state};
use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tracing::{debug, info};

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

fn regime_label(r: Regime) -> &'static str {
    match r {
        Regime::Trending => "Trending",
        Regime::Ranging => "Ranging",
        Regime::Volatile => "Volatile",
        Regime::Illiquid => "Illiquid",
    }
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
            end: if mode == Mode::Practice {
                "no money moved".to_string()
            } else {
                "no money moved".to_string()
            },
            mode,
            emit_decision: false,
        }
    }

    fn enable_decision(&mut self) {
        self.emit_decision = true;
    }

    fn suppress_decision(&mut self) {
        self.emit_decision = false;
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
    matches!(std::env::var(name).ok().as_deref(), Some("1"))
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

fn new_run_id() -> String {
    let ms = state::now_ms();
    let seq = RUN_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("run-{ms}-{seq}")
}

fn position_label(p: &state::Position) -> &'static str {
    match p {
        state::Position::Flat => "Flat",
        state::Position::ExternalInventory { .. } => "ExternalInventory",
        state::Position::Long { .. } => "Long",
    }
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
    std::env::var("BOT_AUDIT_LOG_PATH").ok().filter(|s| !s.trim().is_empty())
}

fn audit_emit_json(value: serde_json::Value) {
    if !json_audit_enabled() {
        return;
    }
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

    let retries = std::env::var("BOT_HTTP_RETRIES")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(3);
    let base_backoff_ms = std::env::var("BOT_HTTP_BACKOFF_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(250);
    let max_backoff_ms = std::env::var("BOT_HTTP_BACKOFF_MAX_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(2_000);

    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 0..=retries {
        let resp = match client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                last_err = Some(anyhow!("Price request failed: {e}"));
                if attempt < retries {
                    let backoff = (base_backoff_ms.saturating_mul(1u64 << attempt)).min(max_backoff_ms);
                    tokio::time::sleep(std::time::Duration::from_millis(backoff)).await;
                    continue;
                }
                break;
            }
        };

        let status = resp.status();
        if status.as_u16() == 429 || status.is_server_error() {
            let text = resp.text().await.unwrap_or_default();
            last_err = Some(anyhow!("Price returned {status}: {text}"));
            if attempt < retries {
                let backoff = (base_backoff_ms.saturating_mul(1u64 << attempt)).min(max_backoff_ms);
                tokio::time::sleep(std::time::Duration::from_millis(backoff)).await;
                continue;
            }
            break;
        }

        let resp = resp.error_for_status()?;
        let body = resp.json::<PriceResp>().await?;
        return Ok(body.price.parse::<f64>()?);
    }

    Err(last_err.unwrap_or_else(|| anyhow!("Price request failed")))
}

async fn ping(client: &Client, base_url: &str) -> Result<()> {
    let url = format!("{base_url}/api/v3/ping");

    let retries = std::env::var("BOT_HTTP_RETRIES")
        .ok()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(3);
    let base_backoff_ms = std::env::var("BOT_HTTP_BACKOFF_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(250);
    let max_backoff_ms = std::env::var("BOT_HTTP_BACKOFF_MAX_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(2_000);

    let mut last_err: Option<anyhow::Error> = None;
    for attempt in 0..=retries {
        let resp = match client.get(&url).send().await {
            Ok(r) => r,
            Err(e) => {
                last_err = Some(anyhow!("Ping failed: {e}"));
                if attempt < retries {
                    let backoff = (base_backoff_ms.saturating_mul(1u64 << attempt)).min(max_backoff_ms);
                    tokio::time::sleep(std::time::Duration::from_millis(backoff)).await;
                    continue;
                }
                break;
            }
        };

        let status = resp.status();
        if status.as_u16() == 429 || status.is_server_error() {
            let text = resp.text().await.unwrap_or_default();
            last_err = Some(anyhow!("Ping returned {status}: {text}"));
            if attempt < retries {
                let backoff = (base_backoff_ms.saturating_mul(1u64 << attempt)).min(max_backoff_ms);
                tokio::time::sleep(std::time::Duration::from_millis(backoff)).await;
                continue;
            }
            break;
        }

        resp.error_for_status()?;
        return Ok(());
    }

    Err(last_err.unwrap_or_else(|| anyhow!("Ping failed")))
}

async fn detect_public_ip(client: &Client) -> Option<String> {
    let url = "https://api.ipify.org?format=json";

    let resp = client
        .get(url)
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .ok()?;
    let text = resp.text().await.ok()?;

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
    info!("If your Binance API key uses IP restriction, whitelist this IP.");
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

    let symbol = cfg.symbol.as_str();

    let btcusdt_price = fetch_price(client, &cfg.base_url, symbol).await?;
    log_say::say_price(btcusdt_price);

    // If keys are missing or invalid, we can still run the public-data pipeline.
    // We simply refuse to trade and we do not touch private endpoints.
    let have_keys = !cfg.api_key.trim().is_empty() && !cfg.api_secret.trim().is_empty();
    let public_ip = if have_keys {
        detect_public_ip(client).await
    } else {
        None
    };
    let bals = if !have_keys {
        guard.set_decision("WAIT");
        guard.set_reason("I cannot see your wallet. We only watch.");
        None
    } else {
        match account::fetch_spot_balances(client, &cfg.api_key, &cfg.api_secret, &cfg.base_url)
            .await
        {
            Ok(b) => Some(b),
            Err(e) if e.downcast_ref::<account::BinanceApiError>().is_some() || is_auth_error(&e) => {
                let mut is_2015 = false;
                if let Some(be) = e.downcast_ref::<account::BinanceApiError>() {
                    is_2015 = matches!(be.code, Some(-2015));
                    guard.set_decision("WAIT");
                    guard.set_reason("I cannot see your wallet. Binance rejected the key.");
                    debug!("Wallet access failed: http_status={} binance_code={:?} binance_msg={:?}", be.status, be.code, be.msg);
                    match (be.code, be.msg.as_deref()) {
                        (Some(-2015), _) => {
                            log_actions_for_2015();
                            let _ = &public_ip;
                            debug!("If your Binance API key uses IP restriction, whitelist this IP.");
                        }
                        (Some(-2014), _) => {
                            guard.set_reason("I cannot see your wallet. API key format looks invalid.");
                        }
                        (Some(-1021), _) => {
                            guard.set_reason("I cannot see your wallet. Clock mismatch.");
                        }
                        (Some(code), Some(msg)) => {
                            guard.set_reason(&format!("I cannot see your wallet. Binance error {}: {}", code, msg));
                        }
                        (_, Some(msg)) => {
                            guard.set_reason(&format!("I cannot see your wallet. {}", msg));
                        }
                        _ => {
                            guard.set_reason("I cannot see your wallet. See debug logs for details.");
                        }
                    }
                } else {
                    guard.set_decision("WAIT");
                    guard.set_reason("I cannot see your wallet. Your API key is not accepted.");
                }
                // Keep guidance concise; -2015 prints the action list above.
                if !is_2015 {
                    debug!("Check BINANCE_API_KEY/BINANCE_API_SECRET, permissions, and IP whitelist restrictions.");
                }
                guard.set_end("no money moved");
                None
            }
            Err(e) => return Err(e),
        }
    };

    let (step_size, tick_size, min_notional) = match bals {
        Some(_) => exchange_info::symbol_rules_from_base(client, &cfg.base_url, symbol).await?,
        None => (0.00001, 0.01, 5.0),
    };

    // Candles (cached)
    let candles_5m = candles::fetch_klines_cached_from_base(
        client,
        &cfg.base_url,
        std::path::Path::new(&cfg.data_dir),
        symbol,
        Interval::FiveMinutes,
        200,
        cfg.candle_cache_max_age,
    )
    .await
    .context("Failed fetching 5m candles")?;

    let candles_1h = candles::fetch_klines_cached_from_base(
        client,
        &cfg.base_url,
        std::path::Path::new(&cfg.data_dir),
        symbol,
        Interval::OneHour,
        200,
        cfg.candle_cache_max_age,
    )
    .await
    .context("Failed fetching 1h candles")?;

    let now_ms = state::now_ms();
    let lag_5m_ms = candle_lag_ms(&candles_5m, now_ms).unwrap_or(u64::MAX);
    let lag_1h_ms = candle_lag_ms(&candles_1h, now_ms).unwrap_or(u64::MAX);
    let max_lag_5m_min = env_u32("BOT_MAX_CANDLE_LAG_5M_MIN")
        .or_else(|| env_u32("BOT_MAX_CANDLE_LAG_MIN"))
        .unwrap_or(15) as u64;
    let max_lag_1h_min = env_u32("BOT_MAX_CANDLE_LAG_1H_MIN").unwrap_or(180) as u64;
    let stale_5m = lag_5m_ms > max_lag_5m_min * 60_000;
    let stale_1h = lag_1h_ms > max_lag_1h_min * 60_000;
    let stale_any = stale_5m || stale_1h;
    debug!(
        "Candle freshness: lag_5m_min={} lag_1h_min={} stale_any={}",
        lag_5m_ms / 60_000,
        lag_1h_ms / 60_000,
        stale_any
    );

    let f = match features::compute_features(&candles_5m, &candles_1h) {
        Ok(x) => x,
        Err(e) => {
            info!("Not enough candle history yet. We wait. ({})", e);
            guard.set_decision("WAIT");
            guard.set_reason("We wait. Not enough candle history yet.");
            guard.set_end("no money moved");
            log_decision_summary(
                &run_id,
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
            return Ok(RunOutcome {
                did_place_order: false,
                mode,
                regime: Regime::Ranging,
            });
        }
    };

    let r = regime::detect_regime(&f);
    log_say::say_action(&format!("Road condition: {:?}.", r.regime));
    debug!("Road reason: {}", r.reason);
    guard.set_reason(&format!("Road condition: {}.", regime_label(r.regime)));

    if bals.is_none() {
        guard.set_decision("WAIT");
        guard.set_reason("I cannot see your wallet. We only watch.");
        guard.set_end("no money moved");
        log_decision_summary(
            &run_id,
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
        return Ok(RunOutcome { did_place_order: false, mode, regime: r.regime });
    }

    let bals = bals.expect("checked above");
    let bals_initial = bals.clone();
    let equity_usdt = bals.usdt_free + (bals.btc_free * btcusdt_price);
    log_say::say_wallet(bals.usdt_free, bals.btc_free, equity_usdt);

    let mut st = state::load_or_init(&cfg.state_path, equity_usdt)?;
    st.sync_equity_and_day(equity_usdt);

    debug!(
        "Equity anchors: start_equity_usdt={} equity_usdt={}",
        fmt_usdt(st.start_equity_usdt),
        fmt_usdt(st.equity_usdt)
    );

    let (died_now, unlocked_now) = st.eval_death_and_unlock(st.equity_usdt, now_ms);
    if died_now || st.is_dead {
        guard.set_decision("WAIT");
        guard.set_reason("We lost 20%. Bot is dead. No more trading.");
        guard.set_end("no money moved");
        state::save(&cfg.state_path, &st)?;
        return Ok(RunOutcome {
            did_place_order: false,
            mode,
            regime: r.regime,
        });
    }

    if unlocked_now {
        // Profit milestone behavior: bump the anchor up and reduce risk for a while.
        let reduce_hours = env_u32("BOT_RISK_REDUCE_HOURS").unwrap_or(24) as u64;
        let reduce_mult = env_f64("BOT_RISK_REDUCE_MULT").unwrap_or(0.5);
        st.start_equity_usdt = st.equity_usdt;
        st.anchor_bumps = st.anchor_bumps.saturating_add(1);
        st.risk_reduction_until_ms = now_ms + reduce_hours * 60 * 60 * 1000;
        debug!(
            "Profit milestone: anchor bumped to {:.2}, risk reduced x{:.2} for {}h",
            st.start_equity_usdt,
            reduce_mult,
            reduce_hours
        );
    }

    state::save(&cfg.state_path, &st)?;
    debug!("Bot state saved.");

    // Feature flags: unlocked at +20%.
    let max_trades_per_day: u32 = if st.profit_unlocked { 5 } else { 3 };
    let mut risk_fraction_multiplier: f64 = if st.profit_unlocked { 1.25 } else { 1.0 };
    if now_ms < st.risk_reduction_until_ms {
        let reduce_mult = env_f64("BOT_RISK_REDUCE_MULT").unwrap_or(0.5);
        if reduce_mult.is_finite() && reduce_mult > 0.0 {
            risk_fraction_multiplier *= reduce_mult;
        }
    }

    // Reconciliation on startup (truth: wallet). Keep it explicit and safe.
    // Manual balance changes can happen; we always choose the safest state.
    const BTC_DUST: f64 = 0.00001;
    let btc_has_dust = bals.btc_free > BTC_DUST;

    // P0: Wallet-vs-memory reconciliation.
    // If wallet has BTC but memory says Flat, treat as external inventory and block new entries.
    let mut wallet_memory_mismatch = false;

    // 2) Long -> Flat if wallet BTC is basically empty.
    if matches!(st.position, state::Position::Long { .. }) && !btc_has_dust {
        st.position = state::Position::Flat;
        state::save(&cfg.state_path, &st)?;
        debug!("Reconcile: Long->Flat because wallet BTC is empty.");
    }

    // 3) Flat + BTC in wallet => ExternalInventory.
    if matches!(st.position, state::Position::Flat) && btc_has_dust {
        st.position = state::Position::ExternalInventory {
            btc_qty: bals.btc_free,
            detected_time_ms: now_ms,
        };
        state::save(&cfg.state_path, &st)?;
        wallet_memory_mismatch = true;
    }

    // If ExternalInventory is present but BTC is gone, clear it.
    if matches!(st.position, state::Position::ExternalInventory { .. }) && !btc_has_dust {
        st.position = state::Position::Flat;
        state::save(&cfg.state_path, &st)?;
        debug!("Reconcile: ExternalInventory->Flat because wallet BTC is empty.");
    }

    log_say::say_state(&st.position);

    if wallet_memory_mismatch {
        guard.set_decision("WAIT");
        guard.set_reason("Wallet and memory did not match. I will not trade until fixed.");
        guard.set_end("no money moved");
        log_decision_summary(
            &run_id,
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
        return Ok(RunOutcome {
            did_place_order: false,
            mode,
            regime: r.regime,
        });
    }

    // Trading rules when external inventory is present.
    if let state::Position::ExternalInventory { .. } = &st.position {
        match mode {
            Mode::Practice => {
                guard.set_decision("WAIT");
                guard.set_reason("This BTC was not bought by the bot. We only watch.");
                guard.set_end("no money moved");
                log_decision_summary(
                    &run_id,
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
                return Ok(RunOutcome {
                    did_place_order: false,
                    mode,
                    regime: r.regime,
                });
            }
            Mode::Live => {
                let allow = env_flag("BOT_ALLOW_EXTERNAL_INVENTORY");
                if !allow {
                    guard.set_decision("WAIT");
                    guard.set_reason("I will not touch BTC I did not buy.");
                    guard.set_end("no money moved");
                    log_decision_summary(
                        &run_id,
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
                    return Ok(RunOutcome {
                        did_place_order: false,
                        mode,
                        regime: r.regime,
                    });
                }
                // Second safety switch for LIVE: never auto-sell external BTC unless panic flatten is explicitly enabled.
                if !env_flag("BOT_PANIC_FLATTEN") {
                    guard.set_decision("WAIT");
                    guard.set_reason("I will not touch BTC I did not buy.");
                    guard.set_end("no money moved");
                    log_decision_summary(
                        &run_id,
                        mode,
                        r.regime,
                        position_label(&st.position),
                        "WAIT",
                        "external_inventory_live_wait",
                        Some((st.trades_today, max_trades_per_day)),
                        Some(risk::drawdown_fraction(st.peak_equity_usdt, st.equity_usdt)),
                        Some(daily_loss_fraction(st.daily_loss_start_equity_usdt, st.equity_usdt)),
                        Some(risk::drawdown_fraction(st.peak_equity_usdt, st.equity_usdt) >= 0.10),
                    );
                    return Ok(RunOutcome {
                        did_place_order: false,
                        mode,
                        regime: r.regime,
                    });
                }

                // Emergency sell (LIVE) for external inventory: only when BOTH switches are enabled.
                let sell_qty = sizing::round_down_to_step(bals_initial.btc_free, step_size);
                let qty_precision = qty_precision_from_step(step_size);
                if sell_qty <= 0.0 {
                    guard.set_decision("WAIT");
                    guard.set_reason("The BTC amount is too small to sell.");
                    guard.set_end("no money moved");
                    return Ok(RunOutcome {
                        did_place_order: false,
                        mode,
                        regime: r.regime,
                    });
                }
                if sizing::ensure_min_notional(btcusdt_price, sell_qty, min_notional).is_err() {
                    guard.set_decision("WAIT");
                    guard.set_reason("We cannot sell yet. It is below Binance minimum.");
                    guard.set_end("no money moved");
                    return Ok(RunOutcome {
                        did_place_order: false,
                        mode,
                        regime: r.regime,
                    });
                }

                guard.set_decision("SELL");
                guard.set_reason("Emergency sell. We flatten.");
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

                guard.set_end("real order sent");

                st.exit_to_flat_with_cooldown(now_ms);
                state::save(&cfg.state_path, &st)?;
                log_decision_summary(
                    &run_id,
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
                return Ok(RunOutcome {
                    did_place_order: true,
                    mode,
                    regime: r.regime,
                });
            }
        }
    }

    // Execution safety caps (needed for summaries/guards).
    let max_notional_fraction = env_f64("BOT_MAX_TRADE_NOTIONAL_FRACTION").unwrap_or(0.20);
    let max_notional_usdt_abs = env_f64("BOT_MAX_TRADE_NOTIONAL_USDT");

    let mut decision_action = "WAIT".to_string();
    let mut decision_reason = String::new();

    if st.is_dead {
        log_say::say_action("We lost 20%. The bot is dead.");
        guard.set_decision("STOP");
        guard.set_reason("We lost too much. Bot is dead.");
        guard.set_end("no money moved");
        log_decision_summary(
            &run_id,
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
        return Ok(RunOutcome {
            did_place_order: false,
            mode,
            regime: r.regime,
        });
    }

    // (now_ms computed earlier)
    if st.in_hibernation(now_ms) {
        info!("We are resting now. We wait." );
        guard.set_decision("WAIT");
        guard.set_reason("We wait. Not time yet.");
        guard.set_end(if mode == Mode::Practice { "no money moved" } else { "no money moved" });
        log_decision_summary(
            &run_id,
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
        return Ok(RunOutcome { did_place_order: false, mode, regime: r.regime });
    }

    if st.in_cooldown(now_ms) {
        info!("We just finished a trip. We cool down first." );
        guard.set_decision("WAIT");
        guard.set_reason("We wait. Not time yet.");
        guard.set_end(if mode == Mode::Practice { "no money moved" } else { "no money moved" });
        log_decision_summary(
            &run_id,
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
        return Ok(RunOutcome { did_place_order: false, mode, regime: r.regime });
    }

    let qty_precision = qty_precision_from_step(step_size);
    let price_precision = price_precision_from_tick(tick_size);
    let mut did_place_order = false;

    match st.position.clone() {
        state::Position::Flat => {
            if stale_any {
                info!("We skip trading because candle data looks stale.");
                guard.set_decision("WAIT");
                guard.set_reason(&format!(
                    "Market data looks stale (5m {}m, 1h {}m). We wait.",
                    lag_5m_ms / 60_000,
                    lag_1h_ms / 60_000
                ));
                guard.set_end("no money moved");
                log_decision_summary(
                    &run_id,
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
                return Ok(RunOutcome { did_place_order: false, mode, regime: r.regime });
            }

            if st.trades_today >= max_trades_per_day {
                info!("We already made enough trades today. We rest.");
                guard.set_decision("WAIT");
                guard.set_reason("We wait. We already traded enough today.");
                guard.set_end("no money moved");
                log_decision_summary(
                    &run_id,
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
                return Ok(RunOutcome { did_place_order: false, mode, regime: r.regime });
            }

            let sig = signals::entry_long_signal(r.regime, &f, btcusdt_price, r.vol_ratio);
            if sig.action == signals::Action::EnterLong {
                let stop = sig.stop_price.ok_or_else(|| anyhow!("Signal had no stop"))?;
                let sized = risk::size_entry_long(
                    r.regime,
                    st.equity_usdt,
                    bals.usdt_free,
                    st.peak_equity_usdt,
                    st.daily_loss_start_equity_usdt,
                    btcusdt_price,
                    stop,
                    step_size,
                    min_notional,
                    risk_fraction_multiplier,
                );

                match sized {
                    risk::RiskDecision::Block { reason, hibernate } => {
                        info!("We skip this trade. {}", reason);
                        decision_action = "BLOCK_ENTRY".to_string();
                        decision_reason = reason.clone();
                        if hibernate {
                            st.hibernation_until_ms = now_ms + 24 * 60 * 60 * 1000;
                            state::save(&cfg.state_path, &st)?;
                            info!("We rest to survive." );
                            decision_action = "HIBERNATE".to_string();
                        }
                    }
                    risk::RiskDecision::Allow { mut qty, mut notional, .. } => {
                        let taker_fee_bps = env_f64("BOT_TAKER_FEE_BPS").unwrap_or(10.0);
                        let slippage_bps = env_f64("BOT_SLIPPAGE_BPS").unwrap_or(20.0);
                        let dd = risk::drawdown_fraction(st.peak_equity_usdt, st.equity_usdt);
                        let dl = daily_loss_fraction(st.daily_loss_start_equity_usdt, st.equity_usdt);
                        let throttled = dd >= 0.10;
                        let base_risk_frac = risk::risk_fraction_for_regime(r.regime) * risk_fraction_multiplier;
                        let effective_risk_frac = if throttled {
                            base_risk_frac * 0.25
                        } else {
                            base_risk_frac
                        };
                        let stop_dist = (btcusdt_price - stop).max(0.0);

                        // Max notional cap: do not allow a single entry to be too large.
                        // Default: 20% of equity, optionally clamped by absolute cap.
                        let mut cap = (st.equity_usdt * max_notional_fraction).max(0.0);
                        if let Some(abs) = max_notional_usdt_abs {
                            cap = cap.min(abs.max(0.0));
                        }
                        let mut was_capped = false;
                        if cap > 0.0 && notional > cap {
                            let capped_qty = sizing::round_down_to_step(cap / btcusdt_price, step_size);
                            if capped_qty > 0.0 {
                                qty = capped_qty;
                                notional = btcusdt_price * qty;
                                was_capped = true;
                            }
                        }

                        info!(
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
                            fmt_usdt(min_notional),
                            pct(dd),
                            pct(dl),
                            st.trades_today,
                            max_trades_per_day
                        );
                        audit_emit_json_with_run_id(&run_id, serde_json::json!({
                            "event": "entry_sizing",
                            "mode": format!("{:?}", mode),
                            "regime": format!("{:?}", r.regime),
                            "atr14_5m": f.atr14_5m,
                            "stop_dist": stop_dist,
                            "price": btcusdt_price,
                            "stop": stop,
                            "risk_frac_base": base_risk_frac,
                            "risk_frac_effective": effective_risk_frac,
                            "qty": qty,
                            "notional": notional,
                            "cap_usdt": cap,
                            "was_capped": was_capped,
                            "min_notional": min_notional,
                            "drawdown_frac": dd,
                            "daily_loss_frac": dl,
                            "trades_today": st.trades_today,
                            "max_trades_per_day": max_trades_per_day
                        }));

                        if let Err(_e) = sizing::ensure_min_notional(btcusdt_price, qty, min_notional) {
                            info!("We skip this trade. The capped size is too small for Binance." );
                            guard.set_decision("WAIT");
                            guard.set_reason("We wait. This trade would be too small for Binance.");
                            guard.set_end("no money moved");
                            log_decision_summary(
                                &run_id,
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
                            return Ok(RunOutcome { did_place_order: false, mode, regime: r.regime });
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
                                    info!(
                                        "Reason: Binance rejected a signed order request (code -2015)."
                                    );
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
                            info!("We bought a small piece." );
                        }

                        // Update state from exchange truth when LIVE.
                        let mut entry_price = btcusdt_price;
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
                                .map_err(|e| {
                                    if is_binance_code(&e, -2015) {
                                        info!("Reason: Binance rejected a signed order request (code -2015).");
                                        log_actions_for_2015();
                                        log_ip_whitelist_hint(&public_ip);
                                    }
                                    e
                                })?;
                                if let Some(avg) = avg_price_from_order_status(&st_order)? {
                                    entry_price = avg;
                                }
                                let exec_qty = parse_f64_field("executedQty", &st_order.executed_qty)?;
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
                        st.enter_long(entry_price, entry_qty, stop, now_ms);
                        st.bump_trades_today();

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
                                    st.set_stop_order_id(stop_order_id);
                                    info!("We placed a real safety stop on the exchange.");
                                }
                                Ok(None) => {
                                    info!("Exchange protection returned no order id; panic flattening.");

                                    // Best-effort panic flatten: sell what we believe we bought.
                                    let sell_qty = sizing::round_down_to_step(entry_qty, step_size);
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
                                    st.exit_to_flat_with_cooldown(now_ms);
                                    state::save(&cfg.state_path, &st)?;
                                    guard.set_decision("SELL");
                                    guard.set_reason("I failed to place protection. I sold to stay safe.");
                                    guard.set_end("real order sent");
                                    log_decision_summary(
                                        &run_id,
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
                                    return Ok(RunOutcome { did_place_order: true, mode, regime: r.regime });
                                }
                                Err(e) => {
                                    info!("Exchange protection failed; panic flattening.");

                                    if is_binance_code(&e, -2015) {
                                        info!("Reason: Binance rejected a signed order request (code -2015).");
                                        log_actions_for_2015();
                                        log_ip_whitelist_hint(&public_ip);
                                    }

                                    // Best-effort panic flatten: sell what we believe we bought.
                                    let sell_qty = sizing::round_down_to_step(entry_qty, step_size);
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
                                    st.exit_to_flat_with_cooldown(now_ms);
                                    state::save(&cfg.state_path, &st)?;
                                    guard.set_decision("SELL");
                                    guard.set_reason("I failed to place protection. I sold to stay safe.");
                                    guard.set_end("real order sent");
                                    log_decision_summary(
                                        &run_id,
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
                                    return Ok(RunOutcome { did_place_order: true, mode, regime: r.regime });
                                }
                            }
                        }

                        state::save(&cfg.state_path, &st)?;
                    }
                }
            } else {
                info!("No trade now. We wait." );
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
        state::Position::Long { qty, entry_price, .. } => {
            // Defined ranging exit for mean reversion: take profit at mid-band with RSI recovery.
            let range_tp = r.regime == Regime::Ranging
                && btcusdt_price >= f.bb_mid20_5m
                && f.rsi14_5m >= 50.0;

            let (decision, why) = if range_tp {
                (state::PositionDecision::ExitLong, "Mean reversion done. We take profit.".to_string())
            } else {
                st.manage_open_position(btcusdt_price, f.atr14_5m, now_ms)
            };
            if decision == state::PositionDecision::ExitLong {
                info!("{}", &why);
                info!(
                    "Audit exit: run_id={} qty={} entry={} now={} atr14_5m={} reason={}",
                    run_id,
                    fmt_btc(qty),
                    fmt_usdt(entry_price),
                    fmt_usdt(btcusdt_price),
                    fmt_usdt(f.atr14_5m),
                    &why
                );
                audit_emit_json_with_run_id(&run_id, serde_json::json!({
                    "event": "exit_decision",
                    "mode": format!("{:?}", mode),
                    "regime": format!("{:?}", r.regime),
                    "qty": qty,
                    "entry_price": entry_price,
                    "price": btcusdt_price,
                    "atr14_5m": f.atr14_5m,
                    "reason": &why
                }));

                // Use min(qty, btc_free) for safety
                let sell_qty = sizing::round_down_to_step(qty.min(bals.btc_free), step_size);
                if sell_qty <= 0.0 {
                    info!("We cannot sell now. Your BTC is too small." );
                    decision_action = "WAIT".to_string();
                    decision_reason = "We want to sell, but the amount is too small.".to_string();
                } else {
                    // Ensure Binance min_notional is met; if not, do not trade.
                    if let Err(_e) = sizing::ensure_min_notional(btcusdt_price, sell_qty, min_notional) {
                        info!("We cannot sell now. Binance says it is too small." );
                        decision_action = "WAIT".to_string();
                        decision_reason = "We want to sell, but Binance minimum blocks it.".to_string();
                    } else {
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
                            info!("We sold a small piece." );
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
                                    let exec_qty = parse_f64_field("executedQty", &st_order.executed_qty).unwrap_or(0.0);
                                    let avg_sell = avg_price_from_order_status(&st_order)
                                        .ok()
                                        .flatten()
                                        .unwrap_or(btcusdt_price);

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

                        st.exit_to_flat_with_cooldown(now_ms);
                        state::save(&cfg.state_path, &st)?;
                    }
                }
            } else {
                info!("{}", &why);
                decision_action = "HOLD_LONG".to_string();
                decision_reason = why;
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
                let equity2 = bals2.usdt_free + (bals2.btc_free * btcusdt_price);
                st.sync_equity_and_day(equity2);
                state::save(&cfg.state_path, &st)?;
                log_say::say_wallet(bals2.usdt_free, bals2.btc_free, st.equity_usdt);
            }
            Err(e) if is_auth_error(&e) => {
                log_say::say_reason("I cannot re-check your wallet at the end. Binance rejected the key.");
            }
            Err(e) => return Err(e),
        }
    } else {
        // Keep state synced to what we already saw.
        st.sync_equity_and_day(equity_usdt);
        let _ = state::save(&cfg.state_path, &st);
    }

    // Final outcome (always printed by the scope guard Drop).
    guard.set_decision(&decision_action);
    guard.set_reason(&decision_reason);
    let end_msg = if mode == Mode::Practice {
        "no money moved"
    } else if did_place_order {
        "real order sent"
    } else {
        "no money moved"
    };
    guard.set_end(end_msg);

    // Keep IMPORTANT logs short; any extra hints belong in debug.
    if (decision_action == "WAIT" || decision_action == "BLOCK_ENTRY") && decision_reason == "no_signal" {
        debug!("No signal under {:?}.", r.regime);
    }

    // Advanced stats stay behind debug.
    log_decision_summary(
        &run_id,
        mode,
        r.regime,
        position_label(&st.position),
        &decision_action,
        &decision_reason,
        Some((st.trades_today, max_trades_per_day)),
        Some(risk::drawdown_fraction(st.peak_equity_usdt, st.equity_usdt)),
        Some(daily_loss_fraction(st.daily_loss_start_equity_usdt, st.equity_usdt)),
        Some(risk::drawdown_fraction(st.peak_equity_usdt, st.equity_usdt) >= 0.10),
    );

    audit_emit_json_with_run_id(&run_id, serde_json::json!({
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
    }));

    Ok(RunOutcome {
        did_place_order,
        mode,
        regime: r.regime,
    })
}
