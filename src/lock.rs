//! Per-repo process locks: `.git/gitsync-<name>.lock`, held by a running
//! gitsync process (one `watch`, one Windows service loop), with its process
//! id in `.git/gitsync-<name>.pid`. The OS releases the lock when the process
//! ends, even if it crashes, so a lock never goes stale.

use std::fs::{self, File, OpenOptions, TryLockError};
use std::path::Path;

use anyhow::{Context, Result};

/// Held for as long as this value lives.
pub struct ProcessLock {
    _file: File,
}

impl ProcessLock {
    /// Takes the lock, or `None` if another process holds it.
    pub fn acquire(git_dir: &Path, name: &str) -> Result<Option<Self>> {
        let Some(file) = try_lock(git_dir, name)? else {
            return Ok(None);
        };
        // A separate file: on Windows a locked file cannot be read by others.
        let pid_file = git_dir.join(format!("gitsync-{name}.pid"));
        fs::write(&pid_file, std::process::id().to_string())
            .with_context(|| format!("writing {}", pid_file.display()))?;
        Ok(Some(Self { _file: file }))
    }

    /// True if some process holds the lock right now.
    pub fn is_held(git_dir: &Path, name: &str) -> Result<bool> {
        Ok(try_lock(git_dir, name)?.is_none())
    }

    /// The process id of whoever holds the lock right now, if anyone (and
    /// if it wrote one).
    pub fn holder(git_dir: &Path, name: &str) -> Result<Option<u32>> {
        if try_lock(git_dir, name)?.is_some() {
            return Ok(None);
        }
        // Only trusted while the lock is held: the file may be stale otherwise.
        let pid =
            fs::read_to_string(git_dir.join(format!("gitsync-{name}.pid"))).unwrap_or_default();
        Ok(Some(pid.trim().parse().unwrap_or(0)).filter(|&pid| pid != 0))
    }
}

fn try_lock(git_dir: &Path, name: &str) -> Result<Option<File>> {
    let path = git_dir.join(format!("gitsync-{name}.lock"));
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(file)),
        Err(TryLockError::WouldBlock) => Ok(None),
        Err(TryLockError::Error(err)) => {
            Err(err).with_context(|| format!("locking {}", path.display()))
        }
    }
}
