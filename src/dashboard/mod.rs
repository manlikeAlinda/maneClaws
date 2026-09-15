pub mod backtest_ui;
pub mod control;

use axum::{
    extract::State,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::Serialize;
use std::sync::{Arc, RwLock};
use tokio::net::TcpListener;
use tower_http::services::ServeDir;

use control::ControlState;

// ── shared state types ────────────────────────────────────────────────────────

pub type SharedSnapshot = Arc<RwLock<Option<DashboardSnapshot>>>;

pub fn new_shared_snapshot() -> SharedSnapshot {
    Arc::new(RwLock::new(None))
}

#[derive(Clone)]
pub struct AppState {
    pub snap: SharedSnapshot,
    pub control: Arc<ControlState>,
}

// ── snapshot struct ───────────────────────────────────────────────────────────

/// One completed trade, appended when position transitions Long → Flat.
#[derive(Debug, Clone, Serialize)]
pub struct TradeRecord {
    pub closed_at_ms: u64,
    pub entry_price: f64,
    pub exit_price: f64,
    pub qty_btc: f64,
    pub pnl_usdt: f64,
    pub result: String, // "WIN" | "LOSS"
}

/// Written by the pipeline at the end of every successful tick.
/// Served as JSON at GET /api/state.
#[derive(Debug, Clone, Serialize)]
pub struct DashboardSnapshot {
    pub captured_at_ms: u64,

    // market
    pub price_usdt: f64,
    pub regime: String,
    pub regime_reason: String,

    // position
    pub position: String,
    pub entry_price: f64,
    pub qty_btc: f64,
    pub stop_price: f64,
    pub tp_price: f64,
    pub unrealised_pnl_usdt: f64,

    // wallet / equity
    pub usdt_free: f64,
    pub btc_free: f64,
    pub equity_usdt: f64,
    pub session_pnl_usdt: f64,
    pub peak_equity_usdt: f64,
    pub drawdown_frac: f64,

    // risk / state
    pub is_dead: bool,
    pub trades_today: u32,
    pub max_trades_per_day: u32,
    pub in_cooldown: bool,
    pub in_hibernation: bool,

    // key indicators
    pub atr14_5m: f64,
    pub rsi14_5m: f64,
    pub rsi14_1m: f64,
    pub bb_width_5m: f64,
    pub volume_z: f64,
    pub velocity_1m: f64,
    pub velocity_5m: f64,
    pub ema20_1h: f64,
    pub ema50_1h: f64,
    pub ema200_1h: f64,
    pub atr_ratio_5m: f64,
    pub htf_bullish: bool,
    pub bearish_bias: bool,
    pub vol_squeeze: bool,

    // last decision
    pub last_action: String,
    pub last_reason: String,
    pub mode: String,

    // trade history (last 20, newest last)
    pub realized_pnl_usdt: f64,
    pub recent_trades: Vec<TradeRecord>,
}

// ── axum handlers ─────────────────────────────────────────────────────────────

async fn get_index() -> impl IntoResponse {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(DASHBOARD_HTML.to_string())
        .unwrap()
}

async fn get_api_state(State(state): State<AppState>) -> impl IntoResponse {
    match state.snap.read() {
        Ok(guard) => match guard.as_ref() {
            Some(s) => (StatusCode::OK, Json(s.clone())).into_response(),
            None => (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({ "error": "No data yet — bot has not completed a tick." })),
            )
                .into_response(),
        },
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": "Snapshot lock poisoned." })),
        )
            .into_response(),
    }
}

// ── server entry point ────────────────────────────────────────────────────────

/// Spawn as `tokio::spawn(dashboard::serve(snap.clone(), control.clone()))`.
/// Binds to DASHBOARD_ADDR env var, default 127.0.0.1:3030.
pub async fn serve(snap: SharedSnapshot, control: Arc<ControlState>) {
    let addr = std::env::var("DASHBOARD_ADDR").unwrap_or_else(|_| "127.0.0.1:3030".to_string());

    let state = AppState { snap, control };

    let protected = Router::new()
        .merge(control::protected_routes())
        .merge(backtest_ui::protected_routes())
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            control::require_token,
        ));

    let app = Router::new()
        .route("/", get(get_index))
        .route("/api/state", get(get_api_state))
        .merge(control::public_routes())
        .merge(backtest_ui::public_routes())
        .merge(protected)
        .nest_service("/reports", ServeDir::new(backtest_ui::REPORTS_DIR))
        .with_state(state);

    let listener = match TcpListener::bind(&addr).await {
        Ok(l) => {
            tracing::info!("Dashboard → http://{}", addr);
            l
        }
        Err(e) => {
            tracing::warn!("Dashboard failed to bind {}: {}", addr, e);
            return;
        }
    };

    if let Err(e) = axum::serve(listener, app).await {
        tracing::warn!("Dashboard server stopped: {}", e);
    }
}

// ── embedded HTML ─────────────────────────────────────────────────────────────

const DASHBOARD_HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>BTC Bot — Terminal</title>
<style>
/* ── RESET & TOKENS ── */
:root {
  --bg:      #07090f;
  --bg2:     #0b0e18;
  --border:  rgba(255,255,255,0.07);
  --border2: rgba(255,255,255,0.12);
  --text:    #c8d0de;
  --muted:   #4e5d73;
  --dim:     #2e3a4a;
  --cyan:    #00d4ff;
  --green:   #00e87a;
  --red:     #ff3d5a;
  --yellow:  #ffb836;
  --purple:  #b47aff;
  --glow-c:  0 0 14px rgba(0,212,255,0.45);
  --glow-g:  0 0 14px rgba(0,232,122,0.45);
  --glow-r:  0 0 14px rgba(255,61,90,0.45);
}
*{box-sizing:border-box;margin:0;padding:0}
html,body{height:100%;overflow:hidden}
body{
  background:var(--bg);
  color:var(--text);
  font-family:'SF Mono','Fira Code','Cascadia Code',Consolas,monospace;
  display:flex;flex-direction:column;
}
/* subtle scanline overlay */
body::after{
  content:'';position:fixed;inset:0;pointer-events:none;z-index:9999;
  background:repeating-linear-gradient(0deg,transparent,transparent 3px,rgba(0,0,0,0.08) 3px,rgba(0,0,0,0.08) 4px);
}

/* ── HEADER ── */
#hdr{
  flex-shrink:0;
  display:flex;align-items:center;gap:1.2rem;flex-wrap:wrap;
  padding:.5rem 1.1rem;
  background:linear-gradient(90deg,#09102a 0%,#080c1a 100%);
  border-bottom:1px solid rgba(0,212,255,0.18);
  box-shadow:0 1px 24px rgba(0,212,255,0.07);
  min-height:52px;
}
.brand{
  font-size:.78rem;font-weight:700;letter-spacing:.18em;text-transform:uppercase;
  color:var(--cyan);text-shadow:var(--glow-c);
  display:flex;align-items:center;gap:.45rem;flex-shrink:0;
}
a.navlink{
  font-size:.62rem;color:var(--muted);text-decoration:none;
  border:1px solid var(--border2);border-radius:3px;padding:.2rem .55rem;flex-shrink:0;
}
a.navlink:hover{color:var(--text)}
#hdr-price{
  font-size:1.75rem;font-weight:700;
  font-variant-numeric:tabular-nums;letter-spacing:-.02em;
  color:var(--text);transition:color .25s,text-shadow .25s;
  flex-shrink:0;
}
#hdr-price.fup{color:var(--green);text-shadow:var(--glow-g)}
#hdr-price.fdn{color:var(--red);text-shadow:var(--glow-r)}
#hdr-meta{
  display:flex;gap:1.1rem;flex-wrap:wrap;font-size:.68rem;
  color:var(--muted);align-items:center;margin-left:auto;
}
#hdr-meta .v{color:var(--text)}

/* ── LOOP CONTROL ── */
#loop-ctrl{display:none;align-items:center;gap:.4rem}
#loop-btn{
  font-family:inherit;font-size:.6rem;padding:.16rem .5rem;border-radius:3px;
  border:1px solid var(--border2);background:rgba(255,255,255,.03);color:var(--text);
  cursor:pointer;text-transform:uppercase;letter-spacing:.05em;
}
#loop-btn:hover{background:rgba(255,255,255,.07)}

/* ── PULSE ── */
.pulse-wrap{display:inline-flex;align-items:center;gap:.38rem;font-size:.65rem;font-weight:700;letter-spacing:.1em}
.pdot{
  width:7px;height:7px;border-radius:50%;flex-shrink:0;
  background:var(--green);box-shadow:0 0 6px var(--green);
  animation:pp 1.6s ease-in-out infinite;
}
.pdot.warn{background:var(--yellow);box-shadow:0 0 6px var(--yellow)}
.pdot.dead{background:var(--red);box-shadow:0 0 6px var(--red);animation:none}
@keyframes pp{0%,100%{transform:scale(1);opacity:1}50%{transform:scale(1.6);opacity:.55}}

/* ── BADGE ── */
.bdg{
  display:inline-block;border-radius:3px;padding:.12rem .45rem;
  font-size:.6rem;font-weight:700;letter-spacing:.1em;text-transform:uppercase;
}
.bl{background:rgba(0,212,255,.13);color:var(--cyan);border:1px solid rgba(0,212,255,.3)}
.bp{background:rgba(255,184,54,.13);color:var(--yellow);border:1px solid rgba(255,184,54,.3)}
.btrend{background:rgba(0,232,122,.1);color:var(--green);border:1px solid rgba(0,232,122,.28)}
.brang{background:rgba(0,212,255,.1);color:var(--cyan);border:1px solid rgba(0,212,255,.28)}
.bvol{background:rgba(255,184,54,.1);color:var(--yellow);border:1px solid rgba(255,184,54,.28)}
.bill{background:rgba(180,122,255,.1);color:var(--purple);border:1px solid rgba(180,122,255,.28)}
.blong{background:rgba(0,232,122,.1);color:var(--green);border:1px solid rgba(0,232,122,.28)}
.bflat{background:rgba(255,255,255,.04);color:var(--muted);border:1px solid var(--border)}
.bext{background:rgba(180,122,255,.1);color:var(--purple);border:1px solid rgba(180,122,255,.28)}
.bdead{background:rgba(255,61,90,.14);color:var(--red);border:1px solid rgba(255,61,90,.32)}
.blivewarn{
  background:rgba(255,61,90,.16);color:var(--red);border:1px solid rgba(255,61,90,.35);
  animation:livepulse 1.4s ease-in-out infinite;
}
@keyframes livepulse{0%,100%{opacity:1}50%{opacity:.55}}

/* ── LAYOUT ── */
#content{flex:1;display:flex;overflow:hidden;min-height:0}
#left{width:268px;flex-shrink:0;display:flex;flex-direction:column;border-right:1px solid var(--border)}
#right{flex:1;display:flex;flex-direction:column;min-width:0}
#right-top{flex:1;display:flex;overflow:hidden;min-height:0}
#right-bot{flex-shrink:0;height:90px;border-top:1px solid var(--border);padding:.55rem .9rem}

/* ── PANEL ── */
.pnl{
  display:flex;flex-direction:column;gap:.5rem;
  padding:.65rem .85rem;overflow-y:auto;
  border-right:1px solid var(--border);
}
.pnl:last-child{border-right:none}
.pnl-t{
  font-size:.55rem;text-transform:uppercase;letter-spacing:.16em;color:var(--dim);
  padding-bottom:.35rem;border-bottom:1px solid var(--border);
  display:flex;justify-content:space-between;align-items:center;flex-shrink:0;
}
.sep{height:1px;background:var(--border);flex-shrink:0}
.slbl{font-size:.55rem;text-transform:uppercase;letter-spacing:.12em;color:var(--dim)}

/* ── KV ROWS ── */
.kv{display:flex;justify-content:space-between;align-items:center;padding:.18rem 0}
.kl{font-size:.67rem;color:var(--muted)}
.kv2{font-size:.7rem;font-variant-numeric:tabular-nums}

/* ── COLOR UTILS ── */
.up{color:var(--green)} .dn{color:var(--red)} .warn{color:var(--yellow)}
.neu{color:var(--text)} .blue{color:var(--cyan)} .pur{color:var(--purple)}
.mut{color:var(--muted)}

/* ── POSITION CARD ── */
#pos-card{
  border-radius:5px;background:rgba(255,255,255,.02);
  border:1px solid var(--border);padding:.5rem .65rem;
}
#pos-card.active{
  border-color:rgba(0,232,122,.3);background:rgba(0,232,122,.03);
  box-shadow:inset 0 0 24px rgba(0,232,122,.04),0 0 12px rgba(0,232,122,.06);
}

/* ── PRICE LADDER ── */
#ladder{display:flex;flex-direction:column;gap:2px;font-size:.64rem;margin-top:.2rem}
.lrow{display:flex;align-items:center;gap:.45rem;padding:.16rem .38rem;border-radius:3px}
.lrow.tp {background:rgba(0,232,122,.07);color:var(--green)}
.lrow.now{background:rgba(0,212,255,.07);color:var(--cyan);font-weight:700}
.lrow.ent{background:rgba(255,184,54,.07);color:var(--yellow)}
.lrow.stp{background:rgba(255,61,90,.07);color:var(--red)}
.ltag{width:2.6rem;font-size:.54rem;opacity:.65;flex-shrink:0;text-transform:uppercase}
.lval{font-variant-numeric:tabular-nums;flex:1}
.lbar{height:2px;border-radius:1px;min-width:3px;flex-shrink:0}

/* ── RSI GAUGE ── */
.gauge-wrap{display:flex;flex-direction:column;align-items:center;flex-shrink:0}
.gauge-sub{font-size:.56rem;color:var(--muted);text-transform:uppercase;letter-spacing:.1em;margin-top:.1rem}

/* ── IND BARS ── */
.ibar-row{display:flex;align-items:center;gap:.45rem;font-size:.65rem}
.ibar-lbl{width:5.2rem;color:var(--muted);flex-shrink:0}
.ibar-track{flex:1;height:3px;background:rgba(255,255,255,.05);border-radius:2px;overflow:hidden}
.ibar-fill{height:100%;border-radius:2px;transition:width .4s ease}
.ibar-num{width:3.8rem;text-align:right;font-variant-numeric:tabular-nums;font-size:.63rem}

/* ── DECISION BOX ── */
#dec-box{
  background:rgba(255,255,255,.02);border:1px solid var(--border);
  border-radius:4px;padding:.45rem .6rem;
}
.dec-act{font-size:.88rem;font-weight:700;letter-spacing:.06em;margin-bottom:.2rem}
.dec-rsn{font-size:.65rem;color:var(--muted);line-height:1.45}

/* ── TRADE LOG ── */
#trade-log{display:flex;flex-direction:column;gap:3px;overflow-y:auto;max-height:160px}
.trow{
  display:grid;grid-template-columns:3.8rem 1fr auto;
  align-items:center;gap:.35rem;
  font-size:.62rem;padding:.22rem .38rem;border-radius:3px;
  background:rgba(255,255,255,.02);border:1px solid var(--border);
  font-variant-numeric:tabular-nums;
}
.trow.twin{border-left:2px solid var(--green)}
.trow.tloss{border-left:2px solid var(--red)}
.ttime{color:var(--muted);font-size:.58rem}
.tprices{display:flex;flex-direction:column;gap:1px}
.tentry{color:var(--muted);font-size:.6rem}
.texit{color:var(--text);font-size:.62rem}
.tpnl{text-align:right;font-weight:700}
.tpnl.up{color:var(--green)} .tpnl.dn{color:var(--red)}
.tno-trades{font-size:.65rem;color:var(--dim);text-align:center;padding:.6rem 0}

/* ── SPARKLINE ── */
#spark-canvas{display:block;width:100%;height:58px}
#spark-lbl{font-size:.54rem;text-transform:uppercase;letter-spacing:.14em;color:var(--dim);margin-bottom:.3rem}

/* ── STATUS BAR ── */
#sbar{
  flex-shrink:0;display:flex;gap:1.4rem;flex-wrap:wrap;align-items:center;
  padding:.28rem 1.1rem;
  background:rgba(0,0,0,.35);border-top:1px solid var(--border);
  font-size:.59rem;color:var(--muted);
}
#sbar .v{color:var(--text)}

/* ── WAIT SCREEN ── */
#wait{
  position:fixed;inset:0;z-index:200;
  background:var(--bg);
  display:flex;flex-direction:column;align-items:center;justify-content:center;gap:.9rem;
}
.wlogo{font-size:1.7rem;letter-spacing:.22em;color:var(--cyan);text-shadow:var(--glow-c)}
.wsub{font-size:.68rem;color:var(--muted)}
.wspin{
  width:28px;height:28px;border-radius:50%;
  border:2px solid rgba(0,212,255,.18);border-top-color:var(--cyan);
  animation:sp 1s linear infinite;
}
@keyframes sp{to{transform:rotate(360deg)}}

/* ── SCROLLBAR ── */
::-webkit-scrollbar{width:3px;height:3px}
::-webkit-scrollbar-track{background:transparent}
::-webkit-scrollbar-thumb{background:var(--dim);border-radius:2px}

/* ── RESPONSIVE ── */
@media(max-width:820px){
  html,body{height:auto;overflow:auto}
  #content{flex-direction:column}
  #left{width:100%;border-right:none;border-bottom:1px solid var(--border)}
  #right-top{flex-direction:column;overflow:visible}
  .pnl{border-right:none;border-bottom:1px solid var(--border)}
  #right-bot{height:80px}
}
</style>
</head>
<body>

<div id="wait">
  <div class="wlogo">⚡ BTC BOT</div>
  <div class="wspin"></div>
  <div class="wsub">Connecting to bot…</div>
</div>

<!-- HEADER -->
<div id="hdr">
  <div class="brand"><span>⚡</span><span>BTC Survival Bot</span></div>
  <a class="navlink" href="/backtest">⇄ Backtests</a>
  <div id="hdr-price">$—</div>
  <div id="hdr-mode"></div>
  <div class="pulse-wrap"><div class="pdot warn" id="pdot"></div><span id="ptext">Connecting</span></div>
  <div id="hdr-meta">
    <span>Equity <span class="v" id="h-eq">—</span></span>
    <span>Session P&L <span class="v" id="h-spnl">—</span></span>
    <span>Trades <span class="v" id="h-tr">—</span></span>
    <span>Drawdown <span class="v" id="h-dd">—</span></span>
    <span>Ticks <span class="v" id="h-tk">0</span></span>
    <span>Updated <span class="v" id="h-ts">—</span></span>
    <span id="loop-ctrl">Loop <span class="v" id="loop-status">—</span><button id="loop-btn">Pause</button></span>
  </div>
</div>

<!-- CONTENT -->
<div id="content">

  <!-- LEFT: Position + Wallet -->
  <div id="left">
    <div class="pnl" style="flex:none">
      <div class="pnl-t">Position <span id="pos-bdg"></span></div>
      <div id="pos-card"><div id="pos-body"></div></div>
      <div id="ladder"></div>
    </div>
    <div class="pnl" style="flex:1">
      <div class="pnl-t">Wallet &amp; Equity</div>
      <div class="kv"><span class="kl">USDT Free</span><span class="kv2" id="w-usdt">—</span></div>
      <div class="kv"><span class="kl">BTC Free</span><span class="kv2" id="w-btc">—</span></div>
      <div class="sep"></div>
      <div class="kv"><span class="kl">Equity</span><span class="kv2 blue" id="w-eq">—</span></div>
      <div class="kv"><span class="kl">Peak</span><span class="kv2 mut" id="w-pk">—</span></div>
      <div class="kv"><span class="kl">Drawdown</span><span class="kv2" id="w-dd">—</span></div>
      <div class="sep"></div>
      <div class="slbl">Session</div>
      <div class="kv"><span class="kl">P&amp;L</span><span class="kv2" id="w-spnl">—</span></div>
      <div class="kv"><span class="kl">Trades today</span><span class="kv2" id="w-tr">—</span></div>
    </div>
  </div>

  <!-- RIGHT -->
  <div id="right">
    <div id="right-top">

      <!-- Panel: Indicators 5m -->
      <div class="pnl" style="min-width:190px;flex:1">
        <div class="pnl-t">Indicators · 5m</div>
        <div class="gauge-wrap">
          <svg width="88" height="50" viewBox="0 0 88 50">
            <path d="M8,44 A36,36,0,0,1,80,44" fill="none" stroke="rgba(255,255,255,0.06)" stroke-width="6" stroke-linecap="round"/>
            <path id="rsi5-arc" d="M8,44 A36,36,0,0,1,80,44" fill="none" stroke="#00d4ff" stroke-width="6" stroke-linecap="round" stroke-dasharray="113" stroke-dashoffset="113" style="transition:stroke-dashoffset .5s,stroke .3s"/>
            <text id="rsi5-txt" x="44" y="43" text-anchor="middle" font-size="12" font-weight="700" fill="#c8d0de">—</text>
          </svg>
          <div class="gauge-sub">RSI 14 · 5m</div>
        </div>
        <div style="display:flex;flex-direction:column;gap:.32rem;margin-top:.15rem">
          <div class="ibar-row"><span class="ibar-lbl">ATR 14</span><div class="ibar-track"><div class="ibar-fill" id="atr-b" style="width:0%;background:var(--cyan)"></div></div><span class="ibar-num" id="atr-v">—</span></div>
          <div class="ibar-row"><span class="ibar-lbl">ATR Ratio</span><div class="ibar-track"><div class="ibar-fill" id="aratio-b" style="width:0%;background:var(--yellow)"></div></div><span class="ibar-num" id="aratio-v">—</span></div>
          <div class="ibar-row"><span class="ibar-lbl">BB Width</span><div class="ibar-track"><div class="ibar-fill" id="bb-b" style="width:0%;background:var(--purple)"></div></div><span class="ibar-num" id="bb-v">—</span></div>
          <div class="ibar-row"><span class="ibar-lbl">Volume Z</span><div class="ibar-track"><div class="ibar-fill" id="vz-b" style="width:50%;background:var(--green)"></div></div><span class="ibar-num" id="vz-v">—</span></div>
        </div>
      </div>

      <!-- Panel: Indicators 1m + HTF -->
      <div class="pnl" style="min-width:190px;flex:1">
        <div class="pnl-t">Indicators · 1m + HTF</div>
        <div class="gauge-wrap">
          <svg width="88" height="50" viewBox="0 0 88 50">
            <path d="M8,44 A36,36,0,0,1,80,44" fill="none" stroke="rgba(255,255,255,0.06)" stroke-width="6" stroke-linecap="round"/>
            <path id="rsi1-arc" d="M8,44 A36,36,0,0,1,80,44" fill="none" stroke="#00d4ff" stroke-width="6" stroke-linecap="round" stroke-dasharray="113" stroke-dashoffset="113" style="transition:stroke-dashoffset .5s,stroke .3s"/>
            <text id="rsi1-txt" x="44" y="43" text-anchor="middle" font-size="12" font-weight="700" fill="#c8d0de">—</text>
          </svg>
          <div class="gauge-sub">RSI 14 · 1m</div>
        </div>
        <div style="display:flex;flex-direction:column;gap:.32rem;margin-top:.15rem">
          <div class="ibar-row"><span class="ibar-lbl">Velocity 1m</span><div class="ibar-track"><div class="ibar-fill" id="v1-b" style="width:50%;background:var(--cyan)"></div></div><span class="ibar-num" id="v1-v">—</span></div>
          <div class="ibar-row"><span class="ibar-lbl">Velocity 5m</span><div class="ibar-track"><div class="ibar-fill" id="v5-b" style="width:50%;background:var(--cyan)"></div></div><span class="ibar-num" id="v5-v">—</span></div>
        </div>
        <div class="sep"></div>
        <div class="slbl">HTF EMAs · 1h</div>
        <div class="kv"><span class="kl">EMA 20</span><span class="kv2" id="ema20">—</span></div>
        <div class="kv"><span class="kl">EMA 50</span><span class="kv2" id="ema50">—</span></div>
        <div class="kv"><span class="kl">EMA 200</span><span class="kv2" id="ema200">—</span></div>
      </div>

      <!-- Panel: Market Regime -->
      <div class="pnl" style="min-width:170px;flex:1">
        <div class="pnl-t">Market Regime</div>
        <div id="regime-bdg" style="margin-bottom:.25rem"></div>
        <div id="regime-rsn" style="font-size:.64rem;color:var(--muted);line-height:1.5"></div>
        <div class="sep"></div>
        <div class="kv"><span class="kl">HTF Bullish</span><span class="kv2" id="htf-b">—</span></div>
        <div class="kv"><span class="kl">Bearish Bias</span><span class="kv2" id="bear-b">—</span></div>
        <div class="kv"><span class="kl">Vol Squeeze</span><span class="kv2" id="vsq">—</span></div>
        <div class="sep"></div>
        <div class="slbl">Bot Status</div>
        <div id="bot-st" style="font-size:.72rem;margin-top:.25rem">—</div>
      </div>

      <!-- Panel: Last Decision + Trade Log -->
      <div class="pnl" style="min-width:200px;flex:1.2">
        <div class="pnl-t">Last Decision</div>
        <div id="dec-box">
          <div class="dec-act" id="dec-act">—</div>
          <div class="dec-rsn" id="dec-rsn">—</div>
        </div>
        <div class="sep"></div>
        <div class="pnl-t" style="border-bottom:none;padding-bottom:0">
          Trade Log <span id="tlog-summary" style="color:var(--muted);font-size:.55rem"></span>
        </div>
        <div id="trade-log"><div class="tno-trades">No closed trades yet this session</div></div>
      </div>

    </div>

    <!-- Sparkline -->
    <div id="right-bot">
      <div id="spark-lbl">BTC · Session Price History</div>
      <canvas id="spark-canvas"></canvas>
    </div>
  </div>

</div>

<!-- STATUS BAR -->
<div id="sbar">
  <span>⚡ BTC Survival Bot</span>
  <span>Regime: <span class="v" id="sb-reg">—</span></span>
  <span>Mode: <span class="v" id="sb-mod">—</span></span>
  <span>Captured: <span class="v" id="sb-ts">—</span></span>
  <span style="margin-left:auto;color:var(--dim)">localhost:3030</span>
</div>

<script>
const $=id=>document.getElementById(id);
const usd=v=>v==null?'—':'$'+v.toLocaleString('en-US',{minimumFractionDigits:2,maximumFractionDigits:2});
const btc=v=>v==null?'—':v.toFixed(6)+' ₿';
const pct=v=>v==null?'—':(v*100).toFixed(2)+'%';
const ts=ms=>ms?new Date(ms).toLocaleTimeString('en-US',{hour12:false}):'—';
const clr=v=>v>0?'up':v<0?'dn':'neu';
function bdg(t,c){return`<span class="bdg ${c}">${t}</span>`}
function regCls(r){return{Trending:'btrend',Ranging:'brang',Volatile:'bvol',Illiquid:'bill'}[r]||'brang'}

// ── SPARKLINE ──────────────────────────────────────────────────────────
const MAX_PTS=180;
let hist=[];
function drawSpark(){
  const cv=$('spark-canvas');
  if(!cv)return;
  const W=cv.parentElement.clientWidth-20;
  const H=58;
  cv.width=W; cv.height=H;
  const ctx=cv.getContext('2d');
  ctx.clearRect(0,0,W,H);
  if(hist.length<2)return;
  const mn=Math.min(...hist),mx=Math.max(...hist);
  const rng=mx-mn||1;
  const yx=p=>H-4-((p-mn)/rng)*(H-8);
  const xx=i=>(i/(hist.length-1))*W;
  // gradient fill
  const g=ctx.createLinearGradient(0,0,0,H);
  g.addColorStop(0,'rgba(0,212,255,.22)');
  g.addColorStop(1,'rgba(0,212,255,0)');
  ctx.beginPath();
  ctx.moveTo(xx(0),yx(hist[0]));
  hist.forEach((p,i)=>{if(i)ctx.lineTo(xx(i),yx(p))});
  ctx.lineTo(xx(hist.length-1),H);ctx.lineTo(0,H);ctx.closePath();
  ctx.fillStyle=g;ctx.fill();
  // line
  ctx.beginPath();
  ctx.moveTo(xx(0),yx(hist[0]));
  hist.forEach((p,i)=>{if(i)ctx.lineTo(xx(i),yx(p))});
  ctx.strokeStyle='#00d4ff';ctx.lineWidth=1.5;
  ctx.shadowColor='#00d4ff';ctx.shadowBlur=5;ctx.stroke();ctx.shadowBlur=0;
  // last dot
  const lx=xx(hist.length-1),ly=yx(hist[hist.length-1]);
  ctx.beginPath();ctx.arc(lx,ly,3,0,Math.PI*2);
  ctx.fillStyle='#00d4ff';ctx.shadowColor='#00d4ff';ctx.shadowBlur=10;ctx.fill();ctx.shadowBlur=0;
}

// ── RSI GAUGE ──────────────────────────────────────────────────────────
const ARC=113;
function setGauge(arcId,txtId,rsi){
  const a=$(arcId),t=$(txtId);
  if(!a||!t||rsi==null)return;
  const f=Math.max(0,Math.min(100,rsi))/100;
  a.style.strokeDashoffset=ARC-f*ARC;
  const c=rsi>70?'#ffb836':rsi<30?'#00e87a':'#00d4ff';
  a.style.stroke=c;
  t.textContent=rsi.toFixed(1);
  t.setAttribute('fill',c);
}

// ── IND BAR ────────────────────────────────────────────────────────────
function setBar(bId,vId,val,lo,hi,fmt,col){
  const b=$(bId),v=$(vId);
  if(!b||!v)return;
  const f=Math.max(0,Math.min(1,(val-lo)/(hi-lo||1)));
  b.style.width=(f*100)+'%';
  b.style.background=typeof col==='function'?col(val):col;
  v.textContent=fmt(val);
}
function setCenteredBar(bId,vId,val,range,fmt,posCol,negCol){
  const b=$(bId),v=$(vId);
  if(!b||!v)return;
  const f=Math.max(0,Math.min(1,(val+range)/(2*range)));
  b.style.width=(f*100)+'%';
  b.style.background=val>=0?posCol:negCol;
  v.textContent=(val>=0?'+':'')+fmt(val);
}

// ── PRICE FLASH ────────────────────────────────────────────────────────
let lastPrice=null;
function flashPrice(newP){
  const el=$('hdr-price');
  if(lastPrice!==null&&newP!==lastPrice){
    el.classList.remove('fup','fdn');
    void el.offsetWidth;
    el.classList.add(newP>lastPrice?'fup':'fdn');
    setTimeout(()=>el.classList.remove('fup','fdn'),400);
  }
  lastPrice=newP;
  el.textContent=usd(newP);
}

// ── PRICE LADDER ───────────────────────────────────────────────────────
function buildLadder(d){
  if(d.position!=='Long')return'';
  const p=d.price_usdt,e=d.entry_price,s=d.stop_price,t=d.tp_price;
  const lo=Math.min(s,e,p)*.998,hi=Math.max(t>0?t:p,e,p)*1.002;
  const rng=hi-lo||1;
  const bw=px=>Math.max(4,Math.round(((px-lo)/rng)*72))+'px';
  let r='';
  if(t>0)r+=`<div class="lrow tp"><span class="ltag">TP</span><span class="lval">${usd(t)}</span><div class="lbar" style="width:${bw(t)};background:var(--green)"></div></div>`;
  r+=`<div class="lrow now"><span class="ltag">NOW</span><span class="lval">${usd(p)}</span><div class="lbar" style="width:${bw(p)};background:var(--cyan)"></div></div>`;
  r+=`<div class="lrow ent"><span class="ltag">ENTRY</span><span class="lval">${usd(e)}</span><div class="lbar" style="width:${bw(e)};background:var(--yellow)"></div></div>`;
  r+=`<div class="lrow stp"><span class="ltag">STOP</span><span class="lval">${usd(s)}</span><div class="lbar" style="width:${bw(s)};background:var(--red)"></div></div>`;
  return r;
}

// ── MAIN RENDER ────────────────────────────────────────────────────────
let ticks=0;
function render(d){
  ticks++;
  const wait=$('wait');
  if(wait)wait.style.display='none';

  // Header
  flashPrice(d.price_usdt);
  const isLive=d.mode==='Live';
  $('hdr-mode').innerHTML=bdg(isLive?'⚠ LIVE':d.mode,isLive?'blivewarn':'bp');
  $('h-eq').textContent=usd(d.equity_usdt);
  const spEl=$('h-spnl');
  spEl.textContent=(d.session_pnl_usdt>=0?'+':'')+usd(d.session_pnl_usdt);
  spEl.className='v '+clr(d.session_pnl_usdt);
  $('h-tr').textContent=d.trades_today+'/'+d.max_trades_per_day;
  const ddEl=$('h-dd');
  ddEl.textContent=pct(d.drawdown_frac);
  ddEl.className='v '+(d.drawdown_frac>.15?'dn':d.drawdown_frac>.08?'warn':'up');
  $('h-tk').textContent=ticks;
  $('h-ts').textContent=ts(d.captured_at_ms);

  // Pulse dot
  const pd=$('pdot'),pt=$('ptext');
  if(d.is_dead){pd.className='pdot dead';pt.textContent='Dead'}
  else if(d.in_hibernation){pd.className='pdot warn';pt.textContent='Hibernating'}
  else if(d.in_cooldown){pd.className='pdot warn';pt.textContent='Cooldown'}
  else{pd.className='pdot';pt.textContent='Live'}

  // Position
  const posCls=d.position==='Long'?'blong':d.position==='ExternalInventory'?'bext':'bflat';
  $('pos-bdg').innerHTML=d.is_dead?bdg('DEAD','bdead'):bdg(d.position,posCls);
  const pc=$('pos-card');
  pc.className=d.position==='Long'?'active':'';
  if(d.position==='Long'){
    const uc=clr(d.unrealised_pnl_usdt);
    $('pos-body').innerHTML=
      `<div class="kv"><span class="kl">Entry</span><span class="kv2 warn">${usd(d.entry_price)}</span></div>`+
      `<div class="kv"><span class="kl">Qty</span><span class="kv2">${btc(d.qty_btc)}</span></div>`+
      `<div class="kv"><span class="kl">Stop</span><span class="kv2 dn">${usd(d.stop_price)}</span></div>`+
      `<div class="kv"><span class="kl">TP</span><span class="kv2 up">${d.tp_price>0?usd(d.tp_price):'—'}</span></div>`+
      `<div class="kv"><span class="kl">Unrealised</span><span class="kv2 ${uc}">${usd(d.unrealised_pnl_usdt)}</span></div>`;
  }else{
    $('pos-body').innerHTML=`<div style="color:var(--muted);font-size:.68rem;padding:.15rem 0">No open position</div>`;
  }
  $('ladder').innerHTML=buildLadder(d);

  // Wallet
  $('w-usdt').textContent=usd(d.usdt_free);
  $('w-btc').textContent=btc(d.btc_free);
  $('w-eq').textContent=usd(d.equity_usdt);
  $('w-pk').textContent=usd(d.peak_equity_usdt);
  const wdd=$('w-dd');
  wdd.textContent=pct(d.drawdown_frac);
  wdd.className='kv2 '+(d.drawdown_frac>.15?'dn':d.drawdown_frac>.08?'warn':'up');
  const wsp=$('w-spnl');
  wsp.textContent=(d.session_pnl_usdt>=0?'+':'')+usd(d.session_pnl_usdt);
  wsp.className='kv2 '+clr(d.session_pnl_usdt);
  $('w-tr').textContent=d.trades_today+' / '+d.max_trades_per_day;

  // Gauges
  setGauge('rsi5-arc','rsi5-txt',d.rsi14_5m);
  setGauge('rsi1-arc','rsi1-txt',d.rsi14_1m);

  // Bars 5m
  setBar('atr-b','atr-v',d.atr14_5m,0,3000,v=>'$'+v.toFixed(0),'#00d4ff');
  setBar('aratio-b','aratio-v',d.atr_ratio_5m,0,3,v=>v.toFixed(3),
    v=>v<.8?'#ffb836':v>1.5?'#ff3d5a':'#00d4ff');
  setBar('bb-b','bb-v',d.bb_width_5m,0,.06,v=>v.toFixed(4),'#b47aff');
  setCenteredBar('vz-b','vz-v',d.volume_z,3,v=>v.toFixed(2),'#00e87a','#ff3d5a');

  // Bars 1m
  setCenteredBar('v1-b','v1-v',d.velocity_1m,.001,v=>v.toFixed(5),'#00e87a','#ff3d5a');
  setCenteredBar('v5-b','v5-v',d.velocity_5m,.001,v=>v.toFixed(5),'#00e87a','#ff3d5a');

  // HTF EMAs
  const pr=d.price_usdt;
  function setEma(id,val){
    const el=$(id);
    el.textContent=usd(val);
    el.className='kv2 '+(pr>val?'up':'dn');
  }
  setEma('ema20',d.ema20_1h);setEma('ema50',d.ema50_1h);setEma('ema200',d.ema200_1h);

  // Regime
  $('regime-bdg').innerHTML=bdg(d.regime,regCls(d.regime));
  $('regime-rsn').textContent=d.regime_reason;
  $('htf-b').innerHTML=d.htf_bullish?'<span class="up">▲ Yes</span>':'<span class="dn">▼ No</span>';
  $('bear-b').innerHTML=d.bearish_bias?'<span class="dn">Yes</span>':'<span class="mut">No</span>';
  $('vsq').innerHTML=d.vol_squeeze?'<span class="warn">Yes</span>':'<span class="mut">No</span>';

  // Bot status
  const bs=$('bot-st');
  if(d.is_dead)bs.innerHTML='<span class="dn">\u{1f6d1} Dead — no more trading</span>';
  else if(d.in_hibernation)bs.innerHTML='<span class="warn">\u{1f319} Hibernating</span>';
  else if(d.in_cooldown)bs.innerHTML='<span class="warn">⏳ Trade cooldown</span>';
  else bs.innerHTML='<span class="up">✓ Ready to trade</span>';

  // Decision
  const da=$('dec-act');
  da.textContent=d.last_action;
  da.className='dec-act '+(d.last_action.includes('ENTER')?'up':d.last_action.includes('EXIT')?'dn':'warn');
  $('dec-rsn').textContent=d.last_reason;

  // Trade log
  const tlog=$('trade-log');
  const trades=d.recent_trades||[];
  if(trades.length===0){
    tlog.innerHTML='<div class="tno-trades">No closed trades yet this session</div>';
    $('tlog-summary').textContent='';
  }else{
    const wins=trades.filter(t=>t.result==='WIN').length;
    const totalPnl=trades.reduce((s,t)=>s+t.pnl_usdt,0);
    $('tlog-summary').textContent=`${trades.length} trades · ${wins}W/${trades.length-wins}L · ${totalPnl>=0?'+':''}${usd(totalPnl)}`;
    // newest first
    tlog.innerHTML=[...trades].reverse().map(t=>{
      const win=t.result==='WIN';
      const pnlStr=(t.pnl_usdt>=0?'+':'')+usd(t.pnl_usdt);
      const entryFmt=usd(t.entry_price);
      const exitFmt=usd(t.exit_price);
      const timeFmt=ts(t.closed_at_ms);
      return `<div class="trow ${win?'twin':'tloss'}">
        <div class="ttime">${timeFmt}</div>
        <div class="tprices"><div class="tentry">IN&nbsp;${entryFmt}</div><div class="texit">OUT ${exitFmt}</div></div>
        <div class="tpnl ${win?'up':'dn'}">${pnlStr}</div>
      </div>`;
    }).join('');
  }

  // Status bar
  $('sb-reg').textContent=d.regime;
  $('sb-mod').textContent=d.mode;
  $('sb-ts').textContent=ts(d.captured_at_ms);

  // Sparkline
  hist.push(d.price_usdt);
  if(hist.length>MAX_PTS)hist.shift();
  drawSpark();
}

// ── POLL ────────────────────────────────────────────────────────────────
async function poll(){
  try{
    const r=await fetch('/api/state',{cache:'no-store'});
    if(r.ok){const d=await r.json();if(!d.error)render(d);}
  }catch{}
}

// ── LOOP CONTROL ───────────────────────────────────────────────────────
let loopPaused=false;
let dashboardToken=null;
async function loadToken(){
  try{
    const r=await fetch('/api/control/token',{cache:'no-store'});
    if(r.ok){dashboardToken=(await r.json()).token;}
  }catch{}
}
async function pollControl(){
  try{
    const r=await fetch('/api/control/status',{cache:'no-store'});
    if(!r.ok)return;
    const d=await r.json();
    loopPaused=d.paused;
    const ctrl=$('loop-ctrl'),st=$('loop-status'),btn=$('loop-btn');
    if(!d.loop_mode){ctrl.style.display='none';return}
    ctrl.style.display='inline-flex';
    st.textContent=loopPaused?'Paused':'Running';
    st.className='v '+(loopPaused?'warn':'up');
    btn.textContent=loopPaused?'Resume':'Pause';
  }catch{}
}
$('loop-btn').addEventListener('click', async ()=>{
  const action=loopPaused?'resume':'pause';
  try{await fetch('/api/control/'+action,{method:'POST',headers:{'X-Dashboard-Token':dashboardToken||''}})}catch{}
  pollControl();
});

loadToken();
poll();
pollControl();
setInterval(poll,2000);
setInterval(pollControl,2000);
window.addEventListener('resize',drawSpark);
</script>
</body>
</html>"##;
