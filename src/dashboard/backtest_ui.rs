use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};

use super::control::JobStatus;
use super::AppState;

/// Reports triggered from the web UI land here — deliberately separate from
/// the repo-root `backtest_*.html` files produced by ad-hoc CLI runs, so the
/// two never mix.
pub(super) const REPORTS_DIR: &str = "backtest_reports";

async fn get_backtest_page() -> impl IntoResponse {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
        .body(BACKTEST_HTML.to_string())
        .unwrap()
}

#[derive(Debug, Deserialize)]
struct RunBacktestRequest {
    symbol: Option<String>,
    data_dir: Option<String>,
    start_frac: Option<f64>,
    end_frac: Option<f64>,
    walk_forward: Option<usize>,
    test_fraction: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct FetchHistoryRequest {
    symbol: Option<String>,
    days: Option<i64>,
    out_dir: Option<String>,
}

#[derive(Serialize)]
struct JobIdResponse {
    job_id: String,
}

fn new_running_job(id: &str, kind: &str) -> JobStatus {
    JobStatus {
        id: id.to_string(),
        kind: kind.to_string(),
        status: "running".to_string(),
        message: "Running…".to_string(),
        result_paths: Vec::new(),
        started_at_ms: crate::state::now_ms(),
        finished_at_ms: None,
    }
}

async fn post_run_backtest(
    State(state): State<AppState>,
    Json(req): Json<RunBacktestRequest>,
) -> impl IntoResponse {
    let job_id = state.control.new_job_id("backtest");
    state.control.insert_job(new_running_job(&job_id, "backtest"));

    let control = state.control.clone();
    let job_id_bg = job_id.clone();
    tokio::spawn(async move {
        let outcome = tokio::task::spawn_blocking(move || run_backtest_blocking(req)).await;
        match outcome {
            Ok(Ok(paths)) => control.update_job(&job_id_bg, |job| {
                job.status = "done".to_string();
                job.message = format!("Wrote {} report(s).", paths.len());
                job.result_paths = paths;
                job.finished_at_ms = Some(crate::state::now_ms());
            }),
            Ok(Err(e)) => control.update_job(&job_id_bg, |job| {
                job.status = "error".to_string();
                job.message = e;
                job.finished_at_ms = Some(crate::state::now_ms());
            }),
            Err(e) => control.update_job(&job_id_bg, |job| {
                job.status = "error".to_string();
                job.message = format!("Backtest task panicked: {e}");
                job.finished_at_ms = Some(crate::state::now_ms());
            }),
        }
    });

    Json(JobIdResponse { job_id })
}

fn run_backtest_blocking(req: RunBacktestRequest) -> Result<Vec<String>, String> {
    use crate::{backtest, report};

    let symbol = req.symbol.unwrap_or_else(|| "BTCUSDT".to_string());
    let data_dir = req.data_dir.unwrap_or_else(|| "backtest_data".to_string());
    let has_range = req.start_frac.is_some() || req.end_frac.is_some();
    let start_frac = req.start_frac.unwrap_or(0.0);
    let end_frac = req.end_frac.unwrap_or(1.0);

    std::fs::create_dir_all(REPORTS_DIR)
        .map_err(|e| format!("Failed creating {REPORTS_DIR}/: {e}"))?;

    if let Some(n_windows) = req.walk_forward {
        let test_fraction = req.test_fraction.unwrap_or(0.3);
        let windows = if has_range {
            backtest::walk_forward_range_with_config(
                &symbol,
                &data_dir,
                start_frac,
                end_frac,
                n_windows,
                test_fraction,
                backtest::BacktestConfig::default(),
            )
            .map_err(|e| e.to_string())?
        } else {
            backtest::walk_forward(&symbol, &data_dir, n_windows, test_fraction)
                .map_err(|e| e.to_string())?
        };

        let mut paths = Vec::new();
        for (label, rep) in &windows {
            let filename = format!("backtest_{label}.html");
            let out_path = format!("{REPORTS_DIR}/{filename}");
            report::write_report(rep, &out_path)
                .map_err(|e| format!("Failed writing {out_path}: {e}"))?;
            paths.push(filename);
        }
        return Ok(paths);
    }

    let rep = if has_range {
        backtest::simulate_from_cache_range(
            &symbol,
            &data_dir,
            start_frac,
            end_frac,
            backtest::BacktestConfig::default(),
        )
        .map_err(|e| e.to_string())?
    } else {
        backtest::simulate_from_cache(&symbol, &data_dir).map_err(|e| e.to_string())?
    };

    let filename = format!("backtest_{symbol}_{}.html", crate::state::now_ms());
    let out_path = format!("{REPORTS_DIR}/{filename}");
    report::write_report(&rep, &out_path).map_err(|e| format!("Failed writing {out_path}: {e}"))?;
    Ok(vec![filename])
}

async fn post_fetch_history(
    State(state): State<AppState>,
    Json(req): Json<FetchHistoryRequest>,
) -> impl IntoResponse {
    let job_id = state.control.new_job_id("fetch_history");
    state.control.insert_job(new_running_job(&job_id, "fetch_history"));

    let control = state.control.clone();
    let job_id_bg = job_id.clone();
    tokio::spawn(async move {
        let outcome = fetch_history_job(req).await;
        match outcome {
            Ok(paths) => control.update_job(&job_id_bg, |job| {
                job.status = "done".to_string();
                job.message = format!("Fetched {} interval file(s).", paths.len());
                job.result_paths = paths;
                job.finished_at_ms = Some(crate::state::now_ms());
            }),
            Err(e) => control.update_job(&job_id_bg, |job| {
                job.status = "error".to_string();
                job.message = e;
                job.finished_at_ms = Some(crate::state::now_ms());
            }),
        }
    });

    Json(JobIdResponse { job_id })
}

async fn fetch_history_job(req: FetchHistoryRequest) -> Result<Vec<String>, String> {
    use crate::candles::Interval;
    use crate::history;

    let symbol = req.symbol.unwrap_or_else(|| "BTCUSDT".to_string());
    let days = req.days.unwrap_or(90).clamp(1, 720);
    let out_dir = req.out_dir.unwrap_or_else(|| "backtest_data".to_string());

    let end_ms = crate::state::now_ms() as i64;
    let start_ms = end_ms - days * 24 * 60 * 60 * 1000;
    let client = reqwest::Client::new();
    let base_url = "https://api.binance.com";

    let mut paths = Vec::new();
    for interval in [Interval::OneHour, Interval::FiveMinutes, Interval::OneMinute] {
        let candles =
            history::fetch_history_range(&client, base_url, &symbol, interval, start_ms, end_ms)
                .await
                .map_err(|e| format!("Failed fetching {} history: {e}", interval.as_str()))?;
        history::save_history(std::path::Path::new(&out_dir), &symbol, interval, &candles)
            .map_err(|e| format!("Failed saving {} history: {e}", interval.as_str()))?;
        paths.push(format!("{out_dir}/{symbol}_{}.json", interval.as_str()));
    }
    Ok(paths)
}

#[derive(Serialize)]
struct ReportEntry {
    filename: String,
    size_bytes: u64,
    modified_ms: u64,
}

async fn get_reports() -> impl IntoResponse {
    let mut entries = Vec::new();
    if let Ok(rd) = std::fs::read_dir(REPORTS_DIR) {
        for entry in rd.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("html") {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            let modified_ms = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            entries.push(ReportEntry {
                filename: entry.file_name().to_string_lossy().to_string(),
                size_bytes: meta.len(),
                modified_ms,
            });
        }
    }
    entries.sort_by(|a, b| b.modified_ms.cmp(&a.modified_ms));
    Json(entries)
}

async fn get_job(State(state): State<AppState>, Path(id): Path<String>) -> impl IntoResponse {
    match state.control.get_job(&id) {
        Some(job) => (StatusCode::OK, Json(job)).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "job not found" })),
        )
            .into_response(),
    }
}

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/backtest", get(get_backtest_page))
        .route("/api/backtest/run", post(post_run_backtest))
        .route("/api/backtest/reports", get(get_reports))
        .route("/api/history/fetch", post(post_fetch_history))
        .route("/api/jobs/{id}", get(get_job))
}

const BACKTEST_HTML: &str = r##"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>BTC Bot — Backtests</title>
<style>
:root{
  --bg:#07090f; --border:rgba(255,255,255,0.07); --border2:rgba(255,255,255,0.12);
  --text:#c8d0de; --muted:#4e5d73; --dim:#2e3a4a;
  --cyan:#00d4ff; --green:#00e87a; --red:#ff3d5a;
  --glow-c:0 0 14px rgba(0,212,255,0.45);
}
*{box-sizing:border-box;margin:0;padding:0}
body{background:var(--bg);color:var(--text);font-family:'SF Mono','Fira Code','Cascadia Code',Consolas,monospace;min-height:100vh}
#hdr{display:flex;align-items:center;gap:1rem;padding:.6rem 1.1rem;background:linear-gradient(90deg,#09102a 0%,#080c1a 100%);border-bottom:1px solid rgba(0,212,255,.18)}
.brand{font-size:.78rem;font-weight:700;letter-spacing:.18em;text-transform:uppercase;color:var(--cyan);text-shadow:var(--glow-c)}
a.navlink{margin-left:auto;font-size:.62rem;color:var(--muted);text-decoration:none;border:1px solid var(--border2);border-radius:3px;padding:.25rem .6rem}
a.navlink:hover{color:var(--text)}
#wrap{display:flex;gap:1.1rem;padding:1.1rem;flex-wrap:wrap}
.card{flex:1 1 320px;background:rgba(255,255,255,.02);border:1px solid var(--border);border-radius:6px;padding:1rem}
.card h2{font-size:.6rem;text-transform:uppercase;letter-spacing:.14em;color:var(--dim);margin-bottom:.7rem}
label{display:block;font-size:.6rem;color:var(--muted);margin-bottom:.2rem;margin-top:.55rem}
input{width:100%;background:rgba(255,255,255,.03);border:1px solid var(--border2);border-radius:3px;color:var(--text);padding:.35rem .5rem;font-family:inherit;font-size:.7rem}
button.run{margin-top:.8rem;width:100%;background:rgba(0,212,255,.12);border:1px solid rgba(0,212,255,.35);color:var(--cyan);border-radius:4px;padding:.5rem;font-family:inherit;font-size:.65rem;letter-spacing:.05em;text-transform:uppercase;cursor:pointer}
button.run:hover{background:rgba(0,212,255,.2)}
button.run:disabled{opacity:.4;cursor:default}
#job-status{margin-top:.7rem;font-size:.64rem;color:var(--muted);min-height:1.2em}
#job-status.err{color:var(--red)}
#job-status.ok{color:var(--green)}
table{width:100%;border-collapse:collapse;font-size:.65rem}
th{text-align:left;color:var(--dim);text-transform:uppercase;letter-spacing:.08em;font-size:.56rem;padding:.3rem .4rem;border-bottom:1px solid var(--border)}
td{padding:.35rem .4rem;border-bottom:1px solid var(--border)}
tr:hover td{background:rgba(255,255,255,.02)}
a.rlink{color:var(--cyan);text-decoration:none}
a.rlink:hover{text-decoration:underline}
.hint{font-size:.58rem;color:var(--dim);margin-top:.5rem;line-height:1.5}
</style>
</head>
<body>
<div id="hdr">
  <div class="brand">⚡ BTC Survival Bot</div>
  <a class="navlink" href="/">← Live Dashboard</a>
</div>
<div id="wrap">

  <div class="card">
    <h2>Run Backtest</h2>
    <label>Symbol</label>
    <input id="bt-symbol" value="BTCUSDT">
    <label>Data dir (cached candles)</label>
    <input id="bt-datadir" value="backtest_data">
    <label>Walk-forward windows (blank = single run)</label>
    <input id="bt-walkforward" placeholder="e.g. 5">
    <label>Test fraction (walk-forward only)</label>
    <input id="bt-testfrac" placeholder="0.3">
    <label>Start / end fraction (optional chronological slice)</label>
    <div style="display:flex;gap:.4rem">
      <input id="bt-startfrac" placeholder="0.0" style="flex:1">
      <input id="bt-endfrac" placeholder="1.0" style="flex:1">
    </div>
    <button class="run" id="bt-run-btn">Run Backtest</button>
    <div class="hint">Reads cached candles from the data dir above — use "Fetch History" first if it's empty. Reports are written to <b>backtest_reports/</b> and never touch the live pipeline's own state, wallet, or orders.</div>
  </div>

  <div class="card">
    <h2>Fetch History</h2>
    <label>Symbol</label>
    <input id="fh-symbol" value="BTCUSDT">
    <label>Days</label>
    <input id="fh-days" value="90">
    <label>Output dir</label>
    <input id="fh-outdir" value="backtest_data">
    <button class="run" id="fh-run-btn">Fetch History</button>
    <div class="hint">Pulls public market candles from Binance (no API key, no auth). Safe to run any time, including while the live loop is running.</div>
  </div>

  <div class="card" style="flex:1 1 100%">
    <h2>Job Status</h2>
    <div id="job-status">No job running.</div>
  </div>

  <div class="card" style="flex:1 1 100%">
    <h2>Reports · backtest_reports/</h2>
    <table>
      <thead><tr><th>File</th><th>Size</th><th>Generated</th></tr></thead>
      <tbody id="reports-body"><tr><td colspan="3" style="color:var(--dim)">Loading…</td></tr></tbody>
    </table>
  </div>

</div>

<script>
const $=id=>document.getElementById(id);
function fmtSize(n){if(n<1024)return n+' B';if(n<1024*1024)return (n/1024).toFixed(1)+' KB';return (n/1024/1024).toFixed(1)+' MB'}
function fmtTs(ms){return ms?new Date(ms).toLocaleString('en-US',{hour12:false}):'—'}

async function loadReports(){
  try{
    const r=await fetch('/api/backtest/reports',{cache:'no-store'});
    const rows=await r.json();
    const body=$('reports-body');
    if(!Array.isArray(rows)||rows.length===0){
      body.innerHTML='<tr><td colspan="3" style="color:var(--dim)">No reports yet — run a backtest above.</td></tr>';
      return;
    }
    body.innerHTML=rows.map(r=>
      `<tr><td><a class="rlink" href="/reports/${encodeURIComponent(r.filename)}" target="_blank">${r.filename}</a></td>`+
      `<td>${fmtSize(r.size_bytes)}</td><td>${fmtTs(r.modified_ms)}</td></tr>`
    ).join('');
  }catch{}
}

let jobPoll=null;
function setJobUI(msg,cls){
  const el=$('job-status');
  el.textContent=msg;
  el.className=cls||'';
}
function pollJob(id){
  clearInterval(jobPoll);
  jobPoll=setInterval(async ()=>{
    try{
      const r=await fetch('/api/jobs/'+id,{cache:'no-store'});
      if(!r.ok)return;
      const j=await r.json();
      if(j.status==='running'){setJobUI(`Running (${j.kind})…`);return}
      clearInterval(jobPoll);
      $('bt-run-btn').disabled=false; $('fh-run-btn').disabled=false;
      if(j.status==='done'){
        setJobUI(`Done: ${j.message}`,'ok');
        loadReports();
      }else{
        setJobUI(`Error: ${j.message}`,'err');
      }
    }catch{}
  },1500);
}

$('bt-run-btn').addEventListener('click', async ()=>{
  const body={
    symbol: $('bt-symbol').value.trim()||undefined,
    data_dir: $('bt-datadir').value.trim()||undefined,
    walk_forward: $('bt-walkforward').value.trim()?parseInt($('bt-walkforward').value.trim(),10):undefined,
    test_fraction: $('bt-testfrac').value.trim()?parseFloat($('bt-testfrac').value.trim()):undefined,
    start_frac: $('bt-startfrac').value.trim()?parseFloat($('bt-startfrac').value.trim()):undefined,
    end_frac: $('bt-endfrac').value.trim()?parseFloat($('bt-endfrac').value.trim()):undefined,
  };
  $('bt-run-btn').disabled=true;
  setJobUI('Starting backtest…');
  try{
    const r=await fetch('/api/backtest/run',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify(body)});
    const j=await r.json();
    pollJob(j.job_id);
  }catch(e){setJobUI('Failed to start: '+e,'err');$('bt-run-btn').disabled=false}
});

$('fh-run-btn').addEventListener('click', async ()=>{
  const body={
    symbol: $('fh-symbol').value.trim()||undefined,
    days: $('fh-days').value.trim()?parseInt($('fh-days').value.trim(),10):undefined,
    out_dir: $('fh-outdir').value.trim()||undefined,
  };
  $('fh-run-btn').disabled=true;
  setJobUI('Starting history fetch…');
  try{
    const r=await fetch('/api/history/fetch',{method:'POST',headers:{'Content-Type':'application/json'},body:JSON.stringify(body)});
    const j=await r.json();
    pollJob(j.job_id);
  }catch(e){setJobUI('Failed to start: '+e,'err');$('fh-run-btn').disabled=false}
});

loadReports();
</script>
</body>
</html>"##;
