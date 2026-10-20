//! `gitsync status`: what this machine has saved, and what it knows about
//! other machines.

use std::fmt;
use std::path::PathBuf;

use anyhow::Result;

use crate::apply::{plan, Plan};
use crate::config::{Config, Discovery};
use crate::fetch::{fetched_peer_names, peer_heads_prefix};
use crate::git::Git;
use crate::service::Service;
use crate::snapshot::{has_unsaved_changes, wip_ref};
use crate::watch;

#[derive(Debug, Clone)]
pub struct Status {
    pub machine_id: String,
    pub config_path: PathBuf,
    pub config_found: bool,
    pub config: Config,
    /// Checked-out branch, `None` when HEAD is detached.
    pub branch: Option<String>,
    /// Current commit, `None` in a repo without commits.
    pub head: Option<String>,
    /// This machine's latest snapshot.
    pub snapshot: Option<SnapshotInfo>,
    /// The working tree or HEAD changed since the latest snapshot.
    pub unsaved_changes: bool,
    /// Snapshots from other machines found in this repo, by machine id.
    pub other_snapshots: Vec<(String, SnapshotInfo)>,
    /// Peers: the configured ones first, then any found on the LAN earlier.
    pub peers: Vec<PeerStatus>,
    /// Whether `gitsync watch` runs, as a service or by hand, in words.
    pub watcher: String,
}

#[derive(Debug, Clone)]
pub struct PeerStatus {
    pub name: String,
    /// From the config; `None` for a peer found on the LAN (its address is
    /// only known while `gitsync watch` hears it).
    pub url: Option<String>,
    /// Fetched branches; empty if never fetched.
    pub branches: Vec<PeerBranch>,
    /// What would happen with the peer's snapshot.
    pub plan: Plan,
}

/// A branch fetched from a peer, compared with our branch of the same name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerBranch {
    pub name: String,
    pub commit: String,
    pub relation: Relation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relation {
    /// Same commit as our branch.
    Same,
    /// The peer has this many commits we don't (we can fast-forward).
    PeerAhead(usize),
    /// We have this many commits the peer doesn't (the peer will fetch them).
    PeerBehind(usize),
    /// Both sides have commits the other doesn't.
    Diverged { ours: usize, theirs: usize },
    /// We have no branch with this name.
    OnlyOnPeer,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SnapshotInfo {
    pub commit: String,
    /// Branch the snapshot was taken on (its `gitsync-branch` trailer).
    pub branch: String,
    /// When it was taken, in words ("5 minutes ago").
    pub age: String,
}

impl Status {
    /// Collects the status. `config` is the effective config (file plus
    /// command line overrides); `machine_id` is already resolved.
    pub fn collect(git: &Git, machine_id: &str, config: Config) -> Result<Self> {
        let config_path = Config::path(git)?;
        let head = git.try_run(&["rev-parse", "--verify", "-q", "HEAD"])?;
        let snapshot = match git.try_run(&["rev-parse", "--verify", "-q", &wip_ref(machine_id)])? {
            Some(commit) => Some(snapshot_info(git, &commit)?),
            None => None,
        };

        let mut other_snapshots = Vec::new();
        let refs = git.run(&["for-each-ref", "--format=%(refname)", "refs/gitsync/"])?;
        for refname in refs.lines() {
            let Some(machine) = refname
                .strip_prefix("refs/gitsync/")
                .and_then(|rest| rest.strip_suffix("/wip"))
            else {
                continue;
            };
            if machine != machine_id {
                other_snapshots.push((machine.to_string(), snapshot_info(git, refname)?));
            }
        }

        let mut names: Vec<(String, Option<String>)> = config
            .peers
            .iter()
            .map(|peer| (peer.name.clone(), Some(peer.url.clone())))
            .collect();
        for name in fetched_peer_names(git)? {
            if name != machine_id && !names.iter().any(|(n, _)| *n == name) {
                names.push((name, None));
            }
        }
        let peers = names
            .into_iter()
            .map(|(name, url)| {
                Ok(PeerStatus {
                    branches: peer_branches(git, &name)?,
                    plan: plan(git, machine_id, &name)?,
                    name,
                    url,
                })
            })
            .collect::<Result<_>>()?;

        Ok(Self {
            peers,
            watcher: watcher(git),
            machine_id: machine_id.to_string(),
            config_found: config_path.exists(),
            config_path,
            config,
            branch: git.try_run(&["symbolic-ref", "--short", "-q", "HEAD"])?,
            head,
            snapshot,
            unsaved_changes: has_unsaved_changes(git, machine_id)?,
            other_snapshots,
        })
    }
}

fn watcher(git: &Git) -> String {
    let service = Service::for_repo(git).and_then(|s| Ok((s.state()?, s)));
    let running = watch::is_running(git).unwrap_or(false);
    match service {
        Ok((state, service)) if state.running => {
            format!("gitsync watch runs in the background ({})", service.name)
        }
        Ok((state, _)) if state.installed => format!(
            "background service installed but not running{}",
            if running {
                "; gitsync watch runs in a terminal"
            } else {
                " (`gitsync service start`)"
            }
        ),
        _ if running => "gitsync watch runs in a terminal".to_string(),
        _ => "gitsync watch is not running (`gitsync service install` runs it in the background)"
            .to_string(),
    }
}

fn snapshot_info(git: &Git, rev: &str) -> Result<SnapshotInfo> {
    let out = git.run(&[
        "log",
        "-1",
        "--format=%H%x00%cr%x00%(trailers:key=gitsync-branch,valueonly)",
        rev,
    ])?;
    let mut fields = out.split('\0');
    let mut next = || fields.next().unwrap_or("").trim().to_string();
    Ok(SnapshotInfo {
        commit: next(),
        age: next(),
        branch: next(),
    })
}

fn peer_branches(git: &Git, peer: &str) -> Result<Vec<PeerBranch>> {
    let prefix = peer_heads_prefix(peer);
    let refs = git.run(&["for-each-ref", "--format=%(objectname) %(refname)", &prefix])?;
    let mut branches = Vec::new();
    for line in refs.lines() {
        let Some((commit, refname)) = line.split_once(' ') else {
            continue;
        };
        let name = refname.strip_prefix(&prefix).unwrap_or(refname).to_string();
        let local = format!("refs/heads/{name}");
        let relation = if git
            .try_run(&["rev-parse", "--verify", "-q", &local])?
            .is_none()
        {
            Relation::OnlyOnPeer
        } else {
            // "<ours> <theirs>": commits only on our side, only on theirs.
            let counts = git.run(&[
                "rev-list",
                "--left-right",
                "--count",
                &format!("{local}...{refname}"),
            ])?;
            let mut counts = counts
                .split_whitespace()
                .map(|n| n.parse::<usize>().unwrap_or(0));
            match (counts.next().unwrap_or(0), counts.next().unwrap_or(0)) {
                (0, 0) => Relation::Same,
                (0, theirs) => Relation::PeerAhead(theirs),
                (ours, 0) => Relation::PeerBehind(ours),
                (ours, theirs) => Relation::Diverged { ours, theirs },
            }
        };
        branches.push(PeerBranch {
            name,
            commit: commit.to_string(),
            relation,
        });
    }
    Ok(branches)
}

impl fmt::Display for Relation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Relation::Same => write!(f, "same as yours"),
            Relation::PeerAhead(n) => write!(f, "{n} commit(s) ahead of yours"),
            Relation::PeerBehind(n) => write!(f, "{n} commit(s) behind yours"),
            Relation::Diverged { ours, theirs } => {
                write!(f, "diverged: {ours} yours only, {theirs} theirs only")
            }
            Relation::OnlyOnPeer => write!(f, "only on this peer"),
        }
    }
}

fn short(commit: &str) -> &str {
    &commit[..commit.len().min(12)]
}

impl fmt::Display for Status {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "machine   {}", self.machine_id)?;

        let found = if self.config_found {
            ""
        } else {
            " (not found, using defaults)"
        };
        writeln!(f, "config    {}{found}", self.config_path.display())?;

        let branch = self.branch.as_deref().unwrap_or("(detached)");
        match &self.head {
            Some(head) => writeln!(f, "branch    {branch} at {}", short(head))?,
            None => writeln!(f, "branch    {branch} (no commits yet)")?,
        }

        match &self.snapshot {
            None => writeln!(f, "snapshot  none yet")?,
            Some(snap) => {
                writeln!(f, "snapshot  {}, {}", short(&snap.commit), snap.age)?;
                if self.unsaved_changes {
                    writeln!(f, "          changes not in a snapshot yet")?;
                } else {
                    writeln!(f, "          up to date")?;
                }
            }
        }

        let discovery = match self.config.discovery {
            Discovery::Mdns => {
                "mdns (peers on the LAN find each other; `gitsync discover` lists them)"
            }
            Discovery::Static => "static (only the peers listed in the config)",
        };
        writeln!(f, "discovery {discovery}")?;
        writeln!(f, "watch     {}", self.watcher)?;
        if self.peers.is_empty() {
            writeln!(f, "peers     none yet")?;
        } else {
            writeln!(f, "peers")?;
            for peer in &self.peers {
                let url = peer
                    .url
                    .as_deref()
                    .unwrap_or("(not in the config; found on the LAN earlier)");
                writeln!(f, "  {}  {url}", peer.name)?;
                writeln!(f, "    sync: {}", peer.plan)?;
                if peer.branches.is_empty() {
                    writeln!(f, "    (not fetched yet; run `gitsync fetch`)")?;
                }
                for branch in &peer.branches {
                    writeln!(
                        f,
                        "    {}  {}  {}",
                        branch.name,
                        short(&branch.commit),
                        branch.relation
                    )?;
                }
            }
        }

        if !self.other_snapshots.is_empty() {
            writeln!(f, "other machines' snapshots")?;
            for (machine, snap) in &self.other_snapshots {
                writeln!(
                    f,
                    "  {machine}  {} on {}, {}",
                    short(&snap.commit),
                    snap.branch,
                    snap.age
                )?;
            }
        }
        Ok(())
    }
}
