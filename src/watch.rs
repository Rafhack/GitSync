//! Takes snapshots automatically: on file changes (debounced) and on a timer
//! as a safety net (DESIGN.md §3). Also fetches from the static peers in the
//! config on the same timer, in a background thread, and applies what it
//! fetched when that is safe (DESIGN.md §4 to §6).

use std::path::Path;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use notify::RecursiveMode;
use notify_debouncer_full::{new_debouncer, DebounceEventResult};
use tracing::{debug, info, warn};

use crate::apply::sync_peer;
use crate::config::{Config, Peer};
use crate::fetch::fetch_peer;
use crate::git::Git;
use crate::snapshot::{snapshot, Outcome};

/// Runs until the process is stopped (Ctrl+C).
pub fn watch(git: &Git, machine_id: &str, config: &Config) -> Result<()> {
    let debounce = Duration::from_millis(config.debounce_ms);
    let poll = Duration::from_secs(config.poll_interval_secs);
    // Canonical paths, so they compare equal to the paths in watcher events.
    let git_dir = git.git_dir()?.canonicalize()?;
    let workdir = git.workdir().canonicalize()?;
    let (tx, rx) = mpsc::channel::<DebounceEventResult>();
    // Snapshots and applies must never interleave: a snapshot of a
    // half-applied working tree would look like a conflict to peers.
    let lock = Arc::new(Mutex::new(()));

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
        display_path(&workdir),
        poll.as_secs()
    );
    take_snapshot(git, machine_id, &lock);

    if !config.peers.is_empty() {
        // Its own thread, so a slow or unreachable peer never delays snapshots.
        let (git, peers, lock) = (git.clone(), config.peers.clone(), lock.clone());
        let machine_id = machine_id.to_string();
        std::thread::spawn(move || fetch_loop(&git, &machine_id, &peers, poll, &lock));
    }

    loop {
        match rx.recv_timeout(poll) {
            Ok(Ok(events)) => {
                let relevant = events
                    .iter()
                    .flat_map(|event| event.paths.iter())
                    .any(|path| !is_inside(path, &git_dir));
                if relevant {
                    take_snapshot(git, machine_id, &lock);
                }
            }
            Ok(Err(errors)) => {
                for err in errors {
                    warn!("file watcher error: {err}");
                }
            }
            Err(RecvTimeoutError::Timeout) => take_snapshot(git, machine_id, &lock),
            // Only happens if the watcher could not start: poll forever.
            Err(RecvTimeoutError::Disconnected) => loop {
                std::thread::sleep(poll);
                take_snapshot(git, machine_id, &lock);
            },
        }
    }
}

/// One snapshot attempt. Errors are logged, not fatal: for example another
/// git command may briefly hold `.git/index.lock`; the next attempt retries.
fn take_snapshot(git: &Git, machine_id: &str, lock: &Mutex<()>) {
    let _guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    match snapshot(git, machine_id) {
        Ok(Outcome::Created(commit)) => info!("snapshot {}", short(&commit)),
        Ok(Outcome::Unchanged) => debug!("no changes"),
        Err(err) => warn!("snapshot failed: {err:#}"),
    }
}

/// Fetches every peer, applies what is safe to apply, then waits `interval`,
/// forever. A peer that is down is reported once when it goes down and once
/// when it comes back; a waiting state ("both changed", ...) once when it
/// starts.
fn fetch_loop(git: &Git, machine_id: &str, peers: &[Peer], interval: Duration, lock: &Mutex<()>) {
    let mut failing = vec![false; peers.len()];
    let mut last_plan: Vec<Option<String>> = vec![None; peers.len()];
    loop {
        for (i, peer) in peers.iter().enumerate() {
            match fetch_peer(git, peer) {
                Ok(report) => {
                    if failing[i] {
                        info!("peer {} is reachable again", peer.name);
                    }
                    failing[i] = false;
                    for change in &report.changes {
                        let new = change.new.as_deref().map_or("deleted", short);
                        debug!("fetched {} → {new}", change.name);
                    }
                }
                Err(err) if !failing[i] => {
                    warn!("{err:#} (will keep retrying quietly)");
                    failing[i] = true;
                    continue;
                }
                Err(err) => {
                    debug!("{err:#}");
                    continue;
                }
            }

            let _guard = lock.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            match sync_peer(git, machine_id, &peer.name) {
                Ok(report) => {
                    if !report.applied_files.is_empty() {
                        info!(
                            "applied changes from {}: {}",
                            peer.name,
                            report.applied_files.join(", ")
                        );
                    }
                    for (branch, old, new) in &report.branches {
                        info!(
                            "fast-forwarded {branch} {}..{} from {}",
                            short(old),
                            short(new),
                            peer.name
                        );
                    }
                    // Log waiting states once, not every poll.
                    let waiting = report.plan.as_ref().filter(|plan| plan.is_waiting());
                    let text = waiting.map(|plan| plan.to_string());
                    if text.is_some() && text != last_plan[i] {
                        info!("{}: {}", peer.name, text.as_deref().unwrap_or(""));
                    }
                    last_plan[i] = text;
                }
                Err(err) => warn!("applying changes from {} failed: {err:#}", peer.name),
            }
        }
        std::thread::sleep(interval);
    }
}

/// Paths for humans: without Windows' `\\?\` prefix that canonicalize adds.
fn display_path(path: &Path) -> String {
    let text = path.display().to_string();
    text.strip_prefix(r"\\?\").unwrap_or(&text).to_string()
}

fn short(commit: &str) -> &str {
    &commit[..commit.len().min(12)]
}

/// Events inside `.git/` come from git itself (or from gitsync's own writes),
/// so they must not trigger a snapshot.
fn is_inside(path: &Path, dir: &Path) -> bool {
    path.starts_with(dir)
}
