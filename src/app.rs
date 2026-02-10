use crate::candles::{self, Interval};
use crate::execution::{self, Mode};
use crate::regime::Regime;
use crate::{account, binance_orders, exchange_info, features, regime, risk, signals, sizing, state};
use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tracing::info;

static RUN_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub base_url: String,
    pub symbol: String,
    pub state_path: String,
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

fn is_auth_error(err: &anyhow::Error) -> bool {
    let s = err.to_string();
    // Binance returns code -2015 for invalid key/permissions.
    s.contains("401 Unauthorized")
        || s.contains("\"code\":-2015")
        || s.contains("\"code\":-2014")
        || s.contains("\"code\":-1021")
        || s.contains("Invalid API-key")
}

fn fmt_usdt(v: f64) -> String {
    format!("{:.2}", v)
}

fn fmt_btc(v: f64) -> String {
    format!("{:.8}", v)
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

    info!(
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
        info!("Decision reason: run_id={} {}", run_id, reason);
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

    info!("AUDIT_JSON {}", line);

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
    info!("{}", execution::mode_log_line(mode));
    let run_id = new_run_id();
    info!("Run start: run_id={}", run_id);

    ping(client, &cfg.base_url).await?;
    info!("We reached safely. Connected to Binance.");

    let symbol = cfg.symbol.as_str();

    let btcusdt_price = fetch_price(client, &cfg.base_url, symbol).await?;
    info!(
        "Today’s Bitcoin price is about {} USDT.",
        fmt_usdt(btcusdt_price)
    );

    // If keys are missing or invalid, we can still run the public-data pipeline.
    // We simply refuse to trade and we do not touch private endpoints.
    let have_keys = !cfg.api_key.trim().is_empty() && !cfg.api_secret.trim().is_empty();
    let bals = if !have_keys {
        info!("I do not have API keys, so I cannot see your wallet.");
        info!("We will only watch the road today. No trading.");
        None
    } else {
        match account::fetch_spot_balances(client, &cfg.api_key, &cfg.api_secret, &cfg.base_url)
            .await
        {
            Ok(b) => Some(b),
            Err(e) if e.downcast_ref::<account::BinanceApiError>().is_some() || is_auth_error(&e) => {
                if let Some(be) = e.downcast_ref::<account::BinanceApiError>() {
                    info!(
                        "I cannot see your wallet. Binance rejected the signed request (http_status={}).",
                        be.status
                    );
                    match (be.code, be.msg.as_deref()) {
                        (Some(-2015), _) | (Some(-2014), _) => {
                            info!("Reason: keys rejected by Binance (code {}). Confirm the API key + secret pair are correct and not truncated.", be.code.unwrap());
                            info!("Also check key permissions and any IP restrictions.");
                        }
                        (Some(-1021), _) => {
                            info!("Reason: clock skew (code -1021). The bot will auto time-sync and retry once.");
                        }
                        (Some(code), Some(msg)) => {
                            info!("Reason: Binance error code {}: {}", code, msg);
                        }
                        (_, Some(msg)) => {
                            info!("Reason: {}", msg);
                        }
                        _ => {
                            info!("Reason: see the detailed Account response logs above.");
                        }
                    }
                } else {
                    info!("I cannot see your wallet. Your API key is not accepted.");
                }
                info!("Check BINANCE_API_KEY/BINANCE_API_SECRET, enable 'Read' + 'Spot & Margin Trading', and review any IP whitelist restrictions.");
                info!("We will only watch the road today. No trading.");
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
        symbol,
        Interval::OneHour,
        200,
        cfg.candle_cache_max_age,
    )
    .await
    .context("Failed fetching 1h candles")?;

    // Stale candle guard: do not trade if our last candle is too old.
    // This protects against trading on stale cache / broken data.
    let now_ms = state::now_ms();
    if let Some(last) = candles_5m.last() {
        let last_ms = last.open_time.max(0) as u64;
        let lag_ms = now_ms.saturating_sub(last_ms);
        let max_lag_min = env_u32("BOT_MAX_CANDLE_LAG_MIN").unwrap_or(15);
        let max_lag_ms = (max_lag_min as u64) * 60_000;
        if lag_ms > max_lag_ms {
            info!(
                "Candles are too old ({} minutes). We will not trade on stale data.",
                lag_ms / 60_000
            );
            // We still compute/log regime below, but we refuse to trade.
        }
    }

    let f = match features::compute_features(&candles_5m, &candles_1h) {
        Ok(x) => x,
        Err(e) => {
            info!("Not enough candle history yet. We wait. ({})", e);
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
    info!(
        "Road condition: {:?}. {}",
        r.regime,
        r.reason
    );

    if bals.is_none() {
        info!("No trade now. We wait." );
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
    let equity_usdt = bals.usdt_free + (bals.btc_free * btcusdt_price);
    info!(
        "Your money: BTC {}, USDT {}, total {} USDT.",
        fmt_btc(bals.btc_free),
        fmt_usdt(bals.usdt_free),
        fmt_usdt(equity_usdt)
    );

    let mut st = state::load_or_init(&cfg.state_path, equity_usdt)?;
    st.sync_equity_and_day(equity_usdt);
    state::save(&cfg.state_path, &st)?;
    info!("Bot memory updated. It now knows you have about {} USDT.", fmt_usdt(st.equity_usdt));

    // Execution safety caps (needed for summaries/guards).
    let max_trades_per_day = env_u32("BOT_MAX_TRADES_PER_DAY").unwrap_or(3);
    let max_notional_fraction = env_f64("BOT_MAX_TRADE_NOTIONAL_FRACTION").unwrap_or(0.20);
    let max_notional_usdt_abs = env_f64("BOT_MAX_TRADE_NOTIONAL_USDT");

    let mut decision_action = "WAIT".to_string();
    let mut decision_reason = String::new();

    if st.is_dead {
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
        return Err(anyhow!("Bot is dead. We stop to survive."));
    }

    // (now_ms computed earlier)
    if st.in_hibernation(now_ms) {
        info!("We are resting now. We wait." );
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

    // Reconciliation / truth source guardrails.
    // If balances and local memory disagree, protect first.
    let panic_flatten_enabled = env_flag("BOT_PANIC_FLATTEN");
    let btc_free_rounded = sizing::round_down_to_step(bals.btc_free, step_size);
    match st.position {
        state::Position::Flat => {
            if btc_free_rounded > 0.0 {
                info!("I see BTC in your wallet, but my memory says we are flat.");
                if mode == Mode::Live && panic_flatten_enabled {
                    info!("Panic flatten is enabled. We will sell the BTC now to reduce risk.");
                    if sizing::ensure_min_notional(btcusdt_price, btc_free_rounded, min_notional).is_ok() {
                        let _ = execution::execute_sell_market(
                            client,
                            mode,
                            &cfg.api_key,
                            &cfg.api_secret,
                            &cfg.base_url,
                            symbol,
                            btc_free_rounded,
                            qty_precision,
                        )
                        .await?;
                        did_place_order = true;
                        st.exit_to_flat_with_cooldown(now_ms);
                        state::save(&cfg.state_path, &st)?;
                        log_decision_summary(
                            &run_id,
                            mode,
                            r.regime,
                            position_label(&st.position),
                            "PANIC_FLATTEN",
                            "state_flat_but_wallet_had_btc",
                            Some((st.trades_today, max_trades_per_day)),
                            Some(risk::drawdown_fraction(st.peak_equity_usdt, st.equity_usdt)),
                            Some(daily_loss_fraction(st.daily_loss_start_equity_usdt, st.equity_usdt)),
                            Some(risk::drawdown_fraction(st.peak_equity_usdt, st.equity_usdt) >= 0.10),
                        );
                        return Ok(RunOutcome { did_place_order, mode, regime: r.regime });
                    } else {
                        info!("BTC amount is too small to sell by Binance rules.");
                    }
                }

                // Default: do not auto-sell unknown BTC. Instead, place a protective stop in LIVE.
                if mode == Mode::Live {
                    let stop = (btcusdt_price - 1.8 * f.atr14_5m).max(0.01);
                    let limit_price = (stop * 0.998).max(0.01);
                    match execution::place_stop_loss_limit_sell(
                        client,
                        mode,
                        &cfg.api_key,
                        &cfg.api_secret,
                        &cfg.base_url,
                        symbol,
                        btc_free_rounded,
                        qty_precision,
                        stop,
                        limit_price,
                        price_precision,
                    )
                    .await
                    {
                        Ok(Some(stop_order_id)) => {
                            info!("We placed a safety stop on the exchange for this BTC.");
                            st.enter_long(btcusdt_price, btc_free_rounded, stop, now_ms);
                            st.set_stop_order_id(stop_order_id);
                            state::save(&cfg.state_path, &st)?;
                            did_place_order = true;
                            log_decision_summary(
                                &run_id,
                                mode,
                                r.regime,
                                position_label(&st.position),
                                "PROTECT_UNKNOWN_BTC",
                                "state_flat_but_wallet_had_btc",
                                Some((st.trades_today, max_trades_per_day)),
                                Some(risk::drawdown_fraction(st.peak_equity_usdt, st.equity_usdt)),
                                Some(daily_loss_fraction(st.daily_loss_start_equity_usdt, st.equity_usdt)),
                                Some(risk::drawdown_fraction(st.peak_equity_usdt, st.equity_usdt) >= 0.10),
                            );
                            return Ok(RunOutcome { did_place_order, mode, regime: r.regime });
                        }
                        Ok(None) => {}
                        Err(e) => {
                            info!("I could not place a safety stop for this BTC. ({})", e);
                        }
                    }
                } else {
                    info!("In PRACTICE mode we will not touch your wallet.");
                }
            }
        }
        state::Position::Long { qty, .. } => {
            // If memory says we are long but the wallet does not have the BTC, reset to flat.
            if btc_free_rounded + 1e-12 < qty * 0.5 {
                info!("My memory says we are long, but I cannot find the BTC in your wallet.");
                info!("We reset to flat to stay honest.");
                st.exit_to_flat_with_cooldown(now_ms);
                state::save(&cfg.state_path, &st)?;
                decision_action = "RESET_FLAT".to_string();
                decision_reason = "state_long_but_wallet_missing_btc".to_string();
            }
        }
    }

    match st.position {
        state::Position::Flat => {
            // If the candle data is stale, do not enter new trades.
            let stale_block = if let Some(last) = candles_5m.last() {
                let last_ms = last.open_time.max(0) as u64;
                let lag_ms = now_ms.saturating_sub(last_ms);
                let max_lag_min = env_u32("BOT_MAX_CANDLE_LAG_MIN").unwrap_or(15);
                lag_ms > (max_lag_min as u64) * 60_000
            } else {
                true
            };
            if stale_block {
                info!("We skip trading because candle data looks stale.");
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

            let sig = signals::trend_breakout_long_only(r.regime, &f, btcusdt_price, r.vol_ratio);
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
                        let dd = risk::drawdown_fraction(st.peak_equity_usdt, st.equity_usdt);
                        let dl = daily_loss_fraction(st.daily_loss_start_equity_usdt, st.equity_usdt);
                        let throttled = dd >= 0.10;
                        let base_risk_frac = risk::risk_fraction_for_regime(r.regime);
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

                        let order_id = execution::execute_buy_market(
                            client,
                            mode,
                            &cfg.api_key,
                            &cfg.api_secret,
                            &cfg.base_url,
                            symbol,
                            qty,
                            qty_precision,
                        )
                        .await?;

                        did_place_order = true;
                        decision_action = "ENTER_LONG".to_string();
                        decision_reason = if was_capped {
                            "entered_long (capped)".to_string()
                        } else {
                            "entered_long".to_string()
                        };
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
                                .await?;
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
                                .unwrap_or_default();
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
                                Ok(None) => {}
                                Err(e) => {
                                    info!("I could not place the safety stop. This is dangerous.");
                                    info!("We will sell right away to protect you.");

                                    // Best-effort panic flatten: sell what we believe we bought.
                                    let sell_qty = sizing::round_down_to_step(entry_qty, step_size);
                                    if sell_qty > 0.0 {
                                        let _ = execution::execute_sell_market(
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
                                    }
                                    st.exit_to_flat_with_cooldown(now_ms);
                                    state::save(&cfg.state_path, &st)?;
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
                                    return Err(anyhow!("Failed to place exchange-side stop: {e}"));
                                }
                            }
                        }

                        state::save(&cfg.state_path, &st)?;
                    }
                }
            } else {
                info!("No trade now. We wait." );
                decision_action = "WAIT".to_string();
                decision_reason = "no_signal".to_string();
            }
        }
        state::Position::Long { qty, entry_price, .. } => {
            let (decision, why) = st.manage_open_position(btcusdt_price, f.atr14_5m, now_ms);
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
                } else {
                    // Ensure Binance min_notional is met; if not, do not trade.
                    if let Err(_e) = sizing::ensure_min_notional(btcusdt_price, sell_qty, min_notional) {
                        info!("We cannot sell now. Binance says it is too small." );
                    } else {
                        let sell_order_id = execution::execute_sell_market(
                            client,
                            mode,
                            &cfg.api_key,
                            &cfg.api_secret,
                            &cfg.base_url,
                            symbol,
                            sell_qty,
                            qty_precision,
                        )
                        .await?;

                        did_place_order = true;
                        decision_action = "EXIT_LONG".to_string();
                        decision_reason = why.clone();
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

    // Refresh balances at end for a clear summary (still required for real runs)
    if have_keys {
        match account::fetch_spot_balances(client, &cfg.api_key, &cfg.api_secret, &cfg.base_url)
            .await
        {
            Ok(bals2) => {
                let equity2 = bals2.usdt_free + (bals2.btc_free * btcusdt_price);
                st.sync_equity_and_day(equity2);
                state::save(&cfg.state_path, &st)?;

                info!(
                    "Here is what you have now: BTC {}, USDT {}, total {} USDT.",
                    fmt_btc(bals2.btc_free),
                    fmt_usdt(bals2.usdt_free),
                    fmt_usdt(st.equity_usdt)
                );
            }
            Err(e) if is_auth_error(&e) => {
                info!("I cannot re-check your wallet at the end. Your API key was rejected.");
                info!("We stop here safely." );
            }
            Err(e) => return Err(e),
        }
    }

    // Final one-line summary for the run.
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
