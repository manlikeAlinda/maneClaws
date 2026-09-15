use axum::{
    extract::{Request, State},
    http::StatusCode,
    middleware::Next,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use super::AppState;

/// Shared runtime control surface: a pause flag the main loop checks each
/// iteration, a store of background job statuses (backtest runs, history
/// fetches) triggered from the dashboard, and the CSRF token guarding every
/// mutating dashboard endpoint.
pub struct ControlState {
    pub paused: AtomicBool,
    pub loop_mode: bool,
    pub jobs: RwLock<HashMap<String, JobStatus>>,
    job_seq: AtomicU64,
    /// Random per-process (or BOT_DASHBOARD_TOKEN-pinned) token. A same-origin
    /// page fetches it once via GET /api/control/token and sends it back as
    /// X-Dashboard-Token on every mutating request. A cross-site page cannot
    /// read that GET's response (blocked by the browser's same-origin policy,
    /// since no CORS layer permits it), so it can never learn the token to
    /// forge a request - closing the CSRF path a bare cross-site <form> POST
    /// had on /api/control/pause|resume (audit finding 3.2).
    pub token: String,
    /// One backtest/history-fetch job at a time: both are CPU/network heavy
    /// and share the live trading loop's process and outbound IP, so
    /// unbounded concurrent jobs risk starving the loop or tripping Binance's
    /// IP rate limit (audit finding 3.3/6.1).
    job_in_flight: AtomicBool,
}

fn generate_token() -> String {
    if let Ok(fixed) = std::env::var("BOT_DASHBOARD_TOKEN") {
        if !fixed.trim().is_empty() {
            return fixed;
        }
    }
    let mut hasher = Sha256::new();
    hasher.update(crate::state::now_ms().to_le_bytes());
    hasher.update(std::process::id().to_le_bytes());
    // Address of a fresh stack allocation as weak, ASLR-derived extra entropy -
    // this only needs to be unguessable by a web attacker, not cryptographically
    // secure against a determined local adversary.
    let stack_marker: u64 = 0;
    hasher.update((&stack_marker as *const u64 as usize).to_le_bytes());
    hex::encode(hasher.finalize())
}

impl ControlState {
    pub fn new(loop_mode: bool) -> Arc<Self> {
        Arc::new(Self {
            paused: AtomicBool::new(false),
            loop_mode,
            jobs: RwLock::new(HashMap::new()),
            job_seq: AtomicU64::new(0),
            token: generate_token(),
            job_in_flight: AtomicBool::new(false),
        })
    }

    pub fn new_job_id(&self, kind: &str) -> String {
        let seq = self.job_seq.fetch_add(1, Ordering::Relaxed);
        format!("job-{kind}-{}-{seq}", crate::state::now_ms())
    }

    /// Only one job runs at a time (see `try_start_job`), but completed jobs
    /// were never evicted, so the map grew by one entry per job for the life
    /// of the process. Cap it so a long-running dashboard doesn't leak memory.
    const MAX_JOBS: usize = 50;

    pub fn insert_job(&self, job: JobStatus) {
        if let Ok(mut jobs) = self.jobs.write() {
            if jobs.len() >= Self::MAX_JOBS {
                if let Some(oldest_id) = jobs
                    .iter()
                    .min_by_key(|(_, j)| j.started_at_ms)
                    .map(|(id, _)| id.clone())
                {
                    jobs.remove(&oldest_id);
                }
            }
            jobs.insert(job.id.clone(), job);
        }
    }

    pub fn update_job(&self, id: &str, f: impl FnOnce(&mut JobStatus)) {
        if let Ok(mut jobs) = self.jobs.write() {
            if let Some(job) = jobs.get_mut(id) {
                f(job);
            }
        }
    }

    pub fn get_job(&self, id: &str) -> Option<JobStatus> {
        self.jobs.read().ok().and_then(|jobs| jobs.get(id).cloned())
    }

    /// Attempts to reserve the single job slot. Returns true if reserved (the
    /// caller must call `release_job_slot` when the job finishes), false if
    /// another job is already in flight.
    pub fn try_start_job(&self) -> bool {
        self.job_in_flight
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    pub fn release_job_slot(&self) {
        self.job_in_flight.store(false, Ordering::SeqCst);
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct JobStatus {
    pub id: String,
    pub kind: String,
    /// "running" | "done" | "error"
    pub status: String,
    pub message: String,
    pub result_paths: Vec<String>,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
}

/// Middleware guarding every mutating dashboard endpoint: requires the
/// `X-Dashboard-Token` header to match the process's token.
pub async fn require_token(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Response {
    let ok = request
        .headers()
        .get("X-Dashboard-Token")
        .and_then(|v| v.to_str().ok())
        .map(|v| v == state.control.token)
        .unwrap_or(false);

    if !ok {
        return (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "missing or invalid X-Dashboard-Token" })),
        )
            .into_response();
    }

    next.run(request).await
}

#[derive(Serialize)]
struct TokenResponse {
    token: String,
}

async fn get_token(State(state): State<AppState>) -> impl IntoResponse {
    Json(TokenResponse { token: state.control.token.clone() })
}

#[derive(Serialize)]
struct ControlStatusResponse {
    paused: bool,
    loop_mode: bool,
}

async fn get_status(State(state): State<AppState>) -> impl IntoResponse {
    Json(ControlStatusResponse {
        paused: state.control.paused.load(Ordering::Relaxed),
        loop_mode: state.control.loop_mode,
    })
}

async fn post_pause(State(state): State<AppState>) -> impl IntoResponse {
    state.control.paused.store(true, Ordering::Relaxed);
    Json(ControlStatusResponse {
        paused: true,
        loop_mode: state.control.loop_mode,
    })
}

async fn post_resume(State(state): State<AppState>) -> impl IntoResponse {
    state.control.paused.store(false, Ordering::Relaxed);
    Json(ControlStatusResponse {
        paused: false,
        loop_mode: state.control.loop_mode,
    })
}

/// Routes open to any same-origin page load: read-only status plus the token
/// bootstrap endpoint itself (safe to leave unauthenticated - see
/// `require_token`'s doc comment on why a cross-site page can't read it).
pub fn public_routes() -> Router<AppState> {
    Router::new()
        .route("/api/control/status", get(get_status))
        .route("/api/control/token", get(get_token))
}

/// Mutating routes - mounted with `require_token` layered on in `mod.rs`.
pub fn protected_routes() -> Router<AppState> {
    Router::new()
        .route("/api/control/pause", post(post_pause))
        .route("/api/control/resume", post(post_resume))
}
