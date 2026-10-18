//! `gitsync status`: what this machine has saved, and what it knows about
//! other machines.

use std::fmt;
use std::path::PathBuf;

use anyhow::Result;

use crate::config::{Config, Discovery};
use crate::git::Git;
use crate::snapshot::{has_unsaved_changes, wip_ref};

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

        Ok(Self {
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
            Discovery::Mdns => "mdns (automatic discovery is not built yet)",
            Discovery::Static => "static",
        };
        writeln!(f, "discovery {discovery}")?;
        if self.config.peers.is_empty() {
            writeln!(f, "peers     none listed")?;
        } else {
            writeln!(f, "peers")?;
            for peer in &self.config.peers {
                writeln!(f, "  {}  {}", peer.name, peer.url)?;
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
