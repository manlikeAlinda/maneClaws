use anyhow::{Context, Result};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

fn path_with_suffix(path: &Path, suffix: &str) -> PathBuf {
    let file_name = path
        .file_name()
        .and_then(|x| x.to_str())
        .unwrap_or("file");
    let new_name = format!("{file_name}{suffix}");
    path.with_file_name(new_name)
}

fn rename_or_copy(src: &Path, dst: &Path) -> Result<()> {
    match fs::rename(src, dst) {
        Ok(()) => Ok(()),
        Err(_) => {
            // Cross-device rename can fail; fall back to copy+remove.
            fs::copy(src, dst)
                .with_context(|| format!("Failed copying {} -> {}", src.display(), dst.display()))?;
            let _ = fs::remove_file(src);
            Ok(())
        }
    }
}

pub fn quarantine_corrupt_file(path: &Path, reason: &str) -> Result<PathBuf> {
    let parent = path
        .parent()
        .context("Cannot quarantine: path has no parent")?;
    let quarantine_dir = parent.join("quarantine");
    fs::create_dir_all(&quarantine_dir)
        .with_context(|| format!("Failed creating quarantine dir: {}", quarantine_dir.display()))?;

    let file_name = path
        .file_name()
        .and_then(|x| x.to_str())
        .unwrap_or("file");
    let ts = crate::state::now_ms();
    let pid = std::process::id();
    let dst = quarantine_dir.join(format!("{file_name}.corrupt.{reason}.{ts}.{pid}"));

    if path.exists() {
        let _ = rename_or_copy(path, &dst);
    }

    Ok(dst)
}

pub fn atomic_write_with_prev(path: &Path, contents: &str) -> Result<()> {
    let parent = path
        .parent()
        .context("Cannot write file: path has no parent")?;
    fs::create_dir_all(parent)
        .with_context(|| format!("Failed creating parent dir: {}", parent.display()))?;

    let ts = crate::state::now_ms();
    let pid = std::process::id();
    let tmp_path = path_with_suffix(path, &format!(".tmp.{ts}.{pid}"));
    let prev_path = path_with_suffix(path, ".prev");

    {
        let mut f = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp_path)
            .with_context(|| format!("Failed opening tmp file: {}", tmp_path.display()))?;
        f.write_all(contents.as_bytes())
            .with_context(|| format!("Failed writing tmp file: {}", tmp_path.display()))?;
        f.sync_all()
            .with_context(|| format!("Failed syncing tmp file: {}", tmp_path.display()))?;
    }

    if path.exists() {
        let _ = fs::remove_file(&prev_path);
        let _ = rename_or_copy(path, &prev_path);
    }

    if let Err(e) = rename_or_copy(&tmp_path, path)
        .with_context(|| format!("Failed committing tmp file: {}", tmp_path.display()))
    {
        // Best-effort rollback.
        let _ = fs::remove_file(&tmp_path);
        if !path.exists() && prev_path.exists() {
            let _ = rename_or_copy(&prev_path, path);
        }
        return Err(e);
    }

    Ok(())
}

pub fn prev_path_for(path: &Path) -> PathBuf {
    path_with_suffix(path, ".prev")
}
