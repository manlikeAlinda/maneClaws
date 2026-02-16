#![allow(clippy::collapsible_if)]

use anyhow::{anyhow, Result};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct ResolvedPaths {
    pub base_dir: PathBuf,
    pub state_path: PathBuf,
    pub data_dir: PathBuf,
    pub lock_path: PathBuf,
}

fn normalize_dir(p: PathBuf) -> PathBuf {
    std::fs::canonicalize(&p).unwrap_or(p)
}

fn resolve_base_dir() -> Result<PathBuf> {
    if let Ok(v) = std::env::var("BOT_BASE_DIR") {
        let s = v.trim();
        if !s.is_empty() {
            let p = PathBuf::from(s);
            if !p.is_absolute() {
                return Err(anyhow!(
                    "BOT_BASE_DIR must be an absolute path, got: {}",
                    p.display()
                ));
            }
            return Ok(normalize_dir(p));
        }
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            return Ok(normalize_dir(parent.to_path_buf()));
        }
    }

    Ok(normalize_dir(std::env::current_dir()?))
}

fn resolve_under_base(base_dir: &Path, override_env: &str, default_rel: &str) -> PathBuf {
    let override_val = std::env::var(override_env)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let p = match override_val {
        Some(v) => {
            let p = PathBuf::from(v);
            if p.is_absolute() {
                p
            } else {
                base_dir.join(p)
            }
        }
        None => base_dir.join(default_rel),
    };

    std::fs::canonicalize(&p).unwrap_or(p)
}

pub fn resolve_paths() -> Result<ResolvedPaths> {
    let base_dir = resolve_base_dir()?;
    let state_path = resolve_under_base(&base_dir, "BOT_STATE_PATH", "bot_state.json");
    let data_dir = resolve_under_base(&base_dir, "BOT_DATA_DIR", "data");
    let lock_path = resolve_under_base(&base_dir, "BOT_LOCK_PATH", "bot.lock");

    Ok(ResolvedPaths {
        base_dir,
        state_path,
        data_dir,
        lock_path,
    })
}
