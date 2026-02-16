use anyhow::{anyhow, Context, Result};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

pub struct SingleInstanceLock {
    _file: fs::File,
    _path: PathBuf,
}

impl SingleInstanceLock {
    pub fn acquire(lock_path: &Path) -> Result<Self> {
        if let Some(parent) = lock_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed creating lock dir: {}", parent.display()))?;
        }

        let mut file = fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(lock_path)
            .with_context(|| format!("Failed opening lock file: {}", lock_path.display()))?;

        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => {
                let _ = file.set_len(0);
                let _ = file.write_all(format!("pid={}\n", std::process::id()).as_bytes());
                let _ = file.sync_all();
                Ok(Self {
                    _file: file,
                    _path: lock_path.to_path_buf(),
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                Err(anyhow!("lock busy: {}", lock_path.display()))
            }
            Err(e) => Err(anyhow!(e).context(format!(
                "Failed acquiring lock: {}",
                lock_path.display()
            ))),
        }
    }
}
