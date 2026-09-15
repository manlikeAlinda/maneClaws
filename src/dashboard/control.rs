use axum::{extract::State, response::IntoResponse, routing::{get, post}, Json, Router};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use super::AppState;

/// Shared runtime control surface: a pause flag the main loop checks each
/// iteration, plus a store of background job statuses (backtest runs,
/// history fetches) triggered from the dashboard.
pub struct ControlState {
    pub paused: AtomicBool,
    pub loop_mode: bool,
    pub jobs: RwLock<HashMap<String, JobStatus>>,
    job_seq: AtomicU64,
}

impl ControlState {
    pub fn new(loop_mode: bool) -> Arc<Self> {
        Arc::new(Self {
            paused: AtomicBool::new(false),
            loop_mode,
            jobs: RwLock::new(HashMap::new()),
            job_seq: AtomicU64::new(0),
        })
    }

    pub fn new_job_id(&self, kind: &str) -> String {
        let seq = self.job_seq.fetch_add(1, Ordering::Relaxed);
        format!("job-{kind}-{}-{seq}", crate::state::now_ms())
    }

    pub fn insert_job(&self, job: JobStatus) {
        if let Ok(mut jobs) = self.jobs.write() {
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

pub fn routes() -> Router<AppState> {
    Router::new()
        .route("/api/control/status", get(get_status))
        .route("/api/control/pause", post(post_pause))
        .route("/api/control/resume", post(post_resume))
}
