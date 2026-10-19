//! `gitsync watch`, the daemon. Takes snapshots on file changes (debounced)
//! and on a timer as a safety net (DESIGN.md §3). Announces this machine over
//! mDNS and finds peers (§4). In a background thread, fetches peers when they
//! announce news (static peers on a timer) and applies what is safe (§5, §6).

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use notify::RecursiveMode;
use notify_debouncer_full::{new_debouncer, DebounceEventResult};
use tracing::{debug, info, warn};

use crate::apply::{sync_peer, Plan};
use crate::config::{Config, Discovery, Peer};
use crate::discovery::{ipv6_note, Announcement, Mdns, PeerEvent};
use crate::fetch::fetch_peer;
use crate::git::Git;
use crate::snapshot::{snapshot, Outcome};

/// How often a peer that announces its news is fetched anyway, in case an
/// announcement got lost (mDNS runs over UDP).
const SAFETY_POLL: Duration = Duration::from_secs(300);

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

    let presence = start_mdns(git, machine_id, config).map(Arc::new);
    let peer_events = presence.as_ref().and_then(|p| p.browse());
    if let Some(presence) = &presence {
        // Say goodbye on Ctrl+C, so peers forget us now instead of waiting
        // for the announcement to expire.
        let presence = presence.clone();
        let _ = ctrlc::set_handler(move || {
            presence.mdns.goodbye();
            std::process::exit(130);
        });
    }

    // The peer thread hears about peers (from mDNS) and about local changes
    // (from the snapshot loop below) on one channel.
    let mut nudge = None;
    if !config.peers.is_empty() || peer_events.is_some() {
        let (loop_tx, loop_rx) = mpsc::channel();
        if let Some(peer_events) = peer_events {
            let loop_tx = loop_tx.clone();
            std::thread::spawn(move || {
                for event in peer_events {
                    if loop_tx.send(LoopEvent::Peer(event)).is_err() {
                        return;
                    }
                }
                warn!("mDNS stopped; using static peers only");
            });
        }
        nudge = Some(loop_tx);
        // Its own thread, so a slow or unreachable peer never delays snapshots.
        let mut peers = PeerLoop {
            git: git.clone(),
            machine_id: machine_id.to_string(),
            poll,
            lock: lock.clone(),
            presence: presence.clone(),
            peers: BTreeMap::new(),
        };
        for peer in &config.peers {
            peers.add_static(peer.clone());
        }
        std::thread::spawn(move || peers.run(loop_rx));
    }

    let after_snapshot = || {
        take_snapshot(git, machine_id, &lock);
        if let Some(presence) = &presence {
            presence.refresh();
        }
        // Our side may have just become ready for work a peer is holding:
        // e.g. the user dropped a conflicting edit or unstaged files.
        if let Some(nudge) = &nudge {
            let _ = nudge.send(LoopEvent::LocalChange);
        }
    };
    loop {
        match rx.recv_timeout(poll) {
            Ok(Ok(events)) => {
                let relevant = events
                    .iter()
                    .flat_map(|event| event.paths.iter())
                    .any(|path| is_relevant(path, &git_dir));
                if relevant {
                    after_snapshot();
                }
            }
            Ok(Err(errors)) => {
                for err in errors {
                    warn!("file watcher error: {err}");
                }
            }
            Err(RecvTimeoutError::Timeout) => after_snapshot(),
            // Only happens if the watcher could not start: poll forever.
            Err(RecvTimeoutError::Disconnected) => loop {
                std::thread::sleep(poll);
                after_snapshot();
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

/// This machine on the network: keeps its announcement up to date.
struct Presence {
    mdns: Mdns,
    git: Git,
    machine_id: String,
    config: Config,
    repo: String,
}

/// Starts mDNS if the config asks for it. Problems are logged and gitsync
/// carries on with the static peers only.
fn start_mdns(git: &Git, machine_id: &str, config: &Config) -> Option<Presence> {
    if config.discovery != Discovery::Mdns {
        return None;
    }
    let announcement = match Announcement::for_repo(git, machine_id, config) {
        Ok(Some(a)) => a,
        Ok(None) => {
            warn!("mDNS discovery starts after the first commit; using static peers only");
            return None;
        }
        Err(err) => {
            warn!("mDNS discovery unavailable ({err:#}); using static peers only");
            return None;
        }
    };
    let mdns = match Mdns::start() {
        Ok(mdns) => mdns,
        Err(err) => {
            warn!("mDNS unavailable ({err:#}); using static peers only");
            return None;
        }
    };
    if let Err(err) = mdns.announce(&announcement) {
        warn!("cannot announce over mDNS ({err:#}); peers will not find this machine");
    } else {
        info!(
            "announcing on the LAN as {} (ssh://{}@<this machine>{})",
            announcement.machine, announcement.user, announcement.path
        );
    }
    Some(Presence {
        mdns,
        git: git.clone(),
        machine_id: machine_id.to_string(),
        config: config.clone(),
        repo: announcement.repo,
    })
}

impl Presence {
    /// Re-announces if our branches or snapshot changed (a new `gen`).
    fn refresh(&self) {
        match Announcement::for_repo(&self.git, &self.machine_id, &self.config) {
            Ok(Some(announcement)) => match self.mdns.announce(&announcement) {
                Ok(true) => debug!("announced gen {}", announcement.gen),
                Ok(false) => {}
                Err(err) => warn!("cannot announce over mDNS: {err:#}"),
            },
            Ok(None) => {}
            Err(err) => debug!("cannot build announcement: {err:#}"),
        }
    }

    fn browse(&self) -> Option<Receiver<PeerEvent>> {
        match self.mdns.browse(&self.repo, &self.machine_id) {
            Ok(events) => Some(events),
            Err(err) => {
                warn!("cannot look for peers over mDNS ({err:#}); using static peers only");
                None
            }
        }
    }
}

/// What the peer thread waits for.
enum LoopEvent {
    /// A peer appeared, announced news, or left.
    Peer(PeerEvent),
    /// We took a snapshot attempt; our state may have changed.
    LocalChange,
}

/// A peer the background thread keeps in sync with.
struct Tracked {
    peer: Peer,
    /// Listed in the config (its URL there wins over an announced one).
    is_static: bool,
    /// Currently announcing itself over mDNS, so news arrive by themselves.
    announced: bool,
    gen: Option<String>,
    next_fetch: Instant,
    failing: bool,
    /// Last "waiting" state logged, to log each one once.
    last_plan: Option<String>,
}

struct PeerLoop {
    git: Git,
    machine_id: String,
    poll: Duration,
    lock: Arc<Mutex<()>>,
    presence: Option<Arc<Presence>>,
    peers: BTreeMap<String, Tracked>,
}

impl PeerLoop {
    fn add_static(&mut self, peer: Peer) {
        self.peers.insert(
            peer.name.clone(),
            Tracked {
                peer,
                is_static: true,
                announced: false,
                gen: None,
                next_fetch: Instant::now(),
                failing: false,
                last_plan: None,
            },
        );
    }

    /// Fetches peers when they announce news (or when their timer is due),
    /// applies what is safe, forever.
    fn run(mut self, events: Receiver<LoopEvent>) {
        loop {
            let now = Instant::now();
            let wait = self
                .peers
                .values()
                .map(|t| t.next_fetch.saturating_duration_since(now))
                .min()
                .unwrap_or(self.poll);
            let mut local_change = false;
            match events.recv_timeout(wait) {
                Ok(first) => {
                    for event in std::iter::once(first).chain(events.try_iter()) {
                        match event {
                            LoopEvent::Peer(event) => self.handle(event),
                            LoopEvent::LocalChange => local_change = true,
                        }
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                // The snapshot loop is gone: the process is ending.
                Err(RecvTimeoutError::Disconnected) => return,
            }

            let now = Instant::now();
            let due: Vec<String> = self
                .peers
                .iter()
                .filter(|(_, t)| t.next_fetch <= now)
                .map(|(name, _)| name.clone())
                .collect();
            for name in &due {
                self.sync(name);
            }
            if local_change {
                // Re-check peers whose work is waiting, with what was
                // already fetched (no network needed).
                let waiting: Vec<String> = self
                    .peers
                    .iter()
                    .filter(|(name, t)| t.last_plan.is_some() && !due.contains(name))
                    .map(|(name, _)| name.clone())
                    .collect();
                for name in waiting {
                    self.apply(&name);
                }
            }
        }
    }

    fn handle(&mut self, event: PeerEvent) {
        let now = Instant::now();
        match event {
            PeerEvent::Found(found) => {
                let name = found.peer.name.clone();
                match self.peers.get_mut(&name) {
                    Some(tracked) => {
                        if !tracked.is_static && tracked.peer.url != found.peer.url {
                            let note = ipv6_note(&found.peer.url);
                            info!("peer {name} moved to {}{note}", found.peer.url);
                            tracked.peer = found.peer;
                        }
                        if !tracked.announced {
                            debug!("peer {name} is announcing itself");
                        }
                        tracked.announced = true;
                        if tracked.gen.as_deref() != Some(found.gen.as_str()) {
                            tracked.gen = Some(found.gen);
                            tracked.next_fetch = now; // news: fetch right away
                        }
                    }
                    None => {
                        let note = ipv6_note(&found.peer.url);
                        info!("found peer {name} on the LAN ({}){note}", found.peer.url);
                        self.peers.insert(
                            name,
                            Tracked {
                                peer: found.peer,
                                is_static: false,
                                announced: true,
                                gen: Some(found.gen),
                                next_fetch: now,
                                failing: false,
                                last_plan: None,
                            },
                        );
                    }
                }
            }
            PeerEvent::Lost(name) => {
                let Some(tracked) = self.peers.get_mut(&name) else {
                    return;
                };
                if tracked.is_static {
                    // Back to polling it on the timer.
                    tracked.announced = false;
                    tracked.next_fetch = tracked.next_fetch.min(now + self.poll);
                } else {
                    info!("peer {name} left the LAN");
                    self.peers.remove(&name);
                }
            }
        }
    }

    /// Fetches one peer and applies what is safe. A peer that is down is
    /// reported once when it goes down and once when it comes back; a
    /// waiting state ("both changed", ...) once when it starts.
    fn sync(&mut self, name: &str) {
        let Some(peer) = self.peers.get(name).map(|t| t.peer.clone()) else {
            return;
        };
        let fetched = fetch_peer(&self.git, &peer);
        let tracked = self.peers.get_mut(name).expect("still tracked");
        // Peers that announce their news only need a rare safety fetch;
        // failures are retried on the normal timer.
        tracked.next_fetch = Instant::now()
            + if tracked.announced && fetched.is_ok() {
                SAFETY_POLL
            } else {
                self.poll
            };
        match fetched {
            Ok(report) => {
                if tracked.failing {
                    info!("peer {name} is reachable again");
                }
                tracked.failing = false;
                for change in &report.changes {
                    let new = change.new.as_deref().map_or("deleted", short);
                    debug!("fetched {} → {new}", change.name);
                }
            }
            Err(err) => {
                if tracked.failing {
                    debug!("{err:#}");
                } else {
                    warn!("{err:#} (will keep retrying quietly)");
                    tracked.failing = true;
                }
                return;
            }
        }

        self.apply(name);
    }

    /// Applies what was fetched from `name`, if safe, and logs what happened.
    fn apply(&mut self, name: &str) {
        let Some(tracked) = self.peers.get_mut(name) else {
            return;
        };
        let report = {
            let _guard = self.lock.lock().unwrap_or_else(|p| p.into_inner());
            sync_peer(&self.git, &self.machine_id, name)
        };
        match report {
            Ok(report) => {
                if !report.applied_files.is_empty() {
                    let verb = if matches!(report.plan, Some(Plan::Merge { .. })) {
                        "merged"
                    } else {
                        "applied"
                    };
                    info!(
                        "{verb} changes from {name}: {}",
                        report.applied_files.join(", ")
                    );
                }
                for (branch, old, new) in &report.branches {
                    info!(
                        "fast-forwarded {branch} {}..{} from {name}",
                        short(old),
                        short(new)
                    );
                }
                let waiting = report.plan.as_ref().filter(|plan| plan.is_waiting());
                let text = waiting.map(|plan| plan.to_string());
                if text.is_some() && text != tracked.last_plan {
                    info!("{name}: {}", text.as_deref().unwrap_or(""));
                }
                tracked.last_plan = text;
                if !report.applied_files.is_empty() || !report.branches.is_empty() {
                    if let Some(presence) = &self.presence {
                        presence.refresh();
                    }
                }
            }
            Err(err) => warn!("applying changes from {name} failed: {err:#}"),
        }
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

/// Whether a file event should trigger a snapshot. Everything in the working
/// tree does. Inside `.git/` only the user's own branch moves do (a commit,
/// checkout or reset changes `HEAD` or `refs/heads/`), so peers hear about
/// commits at once; gitsync's own writes there (objects, its refs, its
/// private index) must not trigger it, or it would loop.
fn is_relevant(path: &Path, git_dir: &Path) -> bool {
    match path.strip_prefix(git_dir) {
        Err(_) => true,
        Ok(inside) => {
            inside == Path::new("HEAD")
                || inside == Path::new("packed-refs")
                || inside.starts_with(Path::new("refs").join("heads"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_relevant;
    use std::path::Path;

    #[test]
    fn reacts_to_files_and_branch_moves_only() {
        let git = Path::new("/r/.git");
        assert!(is_relevant(Path::new("/r/src/main.rs"), git));
        assert!(is_relevant(Path::new("/r/.git/HEAD"), git));
        assert!(is_relevant(Path::new("/r/.git/refs/heads/main"), git));
        assert!(is_relevant(Path::new("/r/.git/refs/heads/main.lock"), git));
        assert!(is_relevant(Path::new("/r/.git/packed-refs"), git));
        assert!(!is_relevant(Path::new("/r/.git/objects/ab/cdef"), git));
        assert!(!is_relevant(
            Path::new("/r/.git/refs/gitsync/laptop/wip"),
            git
        ));
        assert!(!is_relevant(
            Path::new("/r/.git/refs/gitsync-peers/w/heads/main"),
            git
        ));
        assert!(!is_relevant(Path::new("/r/.git/gitsync-index.1.0"), git));
        assert!(!is_relevant(Path::new("/r/.git/index"), git));
        assert!(!is_relevant(Path::new("/r/.git/logs/HEAD"), git));
    }
}
