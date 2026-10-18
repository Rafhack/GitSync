//! Takes snapshots automatically: on file changes (debounced) and on a timer
//! as a safety net. See DESIGN.md §3.

use std::path::Path;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use anyhow::Result;
use notify::RecursiveMode;
use notify_debouncer_full::{new_debouncer, DebounceEventResult};
use tracing::{debug, info, warn};

use crate::git::Git;
use crate::snapshot::{snapshot, Outcome};

/// Runs until the process is stopped (Ctrl+C).
pub fn watch(git: &Git, machine_id: &str, debounce: Duration, poll: Duration) -> Result<()> {
    // Canonical paths, so they compare equal to the paths in watcher events.
    let git_dir = git.git_dir()?.canonicalize()?;
    let workdir = git.workdir().canonicalize()?;
    let (tx, rx) = mpsc::channel::<DebounceEventResult>();

    // Keep the debouncer alive for the whole loop; dropping it stops watching.
    let _debouncer = match new_debouncer(debounce, None, tx) {
        Ok(mut debouncer) => match debouncer.watch(&workdir, RecursiveMode::Recursive) {
            Ok(()) => Some(debouncer),
            Err(err) => {
                warn!("file watcher unavailable ({err}); polling only");
                None
            }
        },
        Err(err) => {
            warn!("file watcher unavailable ({err}); polling only");
            None
        }
    };

    info!(
        "watching {} (poll every {}s)",
        workdir.display(),
        poll.as_secs()
    );
    take_snapshot(git, machine_id);

    loop {
        match rx.recv_timeout(poll) {
            Ok(Ok(events)) => {
                let relevant = events
                    .iter()
                    .flat_map(|event| event.paths.iter())
                    .any(|path| !is_inside(path, &git_dir));
                if relevant {
                    take_snapshot(git, machine_id);
                }
            }
            Ok(Err(errors)) => {
                for err in errors {
                    warn!("file watcher error: {err}");
                }
            }
            Err(RecvTimeoutError::Timeout) => take_snapshot(git, machine_id),
            // Only happens if the watcher could not start: poll forever.
            Err(RecvTimeoutError::Disconnected) => loop {
                std::thread::sleep(poll);
                take_snapshot(git, machine_id);
            },
        }
    }
}

/// One snapshot attempt. Errors are logged, not fatal: for example another
/// git command may briefly hold `.git/index.lock`; the next attempt retries.
fn take_snapshot(git: &Git, machine_id: &str) {
    match snapshot(git, machine_id) {
        Ok(Outcome::Created(commit)) => info!("snapshot {}", &commit[..commit.len().min(12)]),
        Ok(Outcome::Unchanged) => debug!("no changes"),
        Err(err) => warn!("snapshot failed: {err:#}"),
    }
}

/// Events inside `.git/` come from git itself (or from gitsync's own writes),
/// so they must not trigger a snapshot.
fn is_inside(path: &Path, dir: &Path) -> bool {
    path.starts_with(dir)
}
