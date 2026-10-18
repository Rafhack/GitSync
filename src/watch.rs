//! Takes snapshots automatically: on file changes (debounced) and on a timer
//! as a safety net (DESIGN.md §3). Also fetches from the static peers in the
//! config on the same timer, in a background thread (DESIGN.md §4, §5).

use std::path::Path;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::time::Duration;

use anyhow::Result;
use notify::RecursiveMode;
use notify_debouncer_full::{new_debouncer, DebounceEventResult};
use tracing::{debug, info, warn};

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

    if !config.peers.is_empty() {
        // Its own thread, so a slow or unreachable peer never delays snapshots.
        let (git, peers) = (git.clone(), config.peers.clone());
        std::thread::spawn(move || fetch_loop(&git, &peers, poll));
    }

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
        Ok(Outcome::Created(commit)) => info!("snapshot {}", short(&commit)),
        Ok(Outcome::Unchanged) => debug!("no changes"),
        Err(err) => warn!("snapshot failed: {err:#}"),
    }
}

/// Fetches every peer, then waits `interval`, forever. A peer that is down
/// is reported once when it goes down and once when it comes back.
fn fetch_loop(git: &Git, peers: &[Peer], interval: Duration) {
    let mut failing = vec![false; peers.len()];
    loop {
        for (peer, failing) in peers.iter().zip(failing.iter_mut()) {
            match fetch_peer(git, peer) {
                Ok(report) => {
                    if *failing {
                        info!("peer {} is reachable again", peer.name);
                    }
                    *failing = false;
                    for change in &report.changes {
                        let new = change.new.as_deref().map_or("deleted", short);
                        info!("fetched {} → {new}", change.name);
                    }
                }
                Err(err) if !*failing => {
                    warn!("{err:#} (will keep retrying quietly)");
                    *failing = true;
                }
                Err(err) => debug!("{err:#}"),
            }
        }
        std::thread::sleep(interval);
    }
}

fn short(commit: &str) -> &str {
    &commit[..commit.len().min(12)]
}

/// Events inside `.git/` come from git itself (or from gitsync's own writes),
/// so they must not trigger a snapshot.
fn is_inside(path: &Path, dir: &Path) -> bool {
    path.starts_with(dir)
}
