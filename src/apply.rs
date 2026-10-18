//! Applies what was fetched from a peer to this machine: its snapshot to the
//! working tree, and its branches as fast-forwards. See DESIGN.md §6.
//!
//! The rule is: never destroy unsynced local work. A peer's snapshot is only
//! applied when our working tree has not changed since the last time the two
//! machines were in sync (the "base"). If both sides changed, nothing is
//! touched (merging is step 6).
//!
//! A "state" is what a snapshot captures: the commit HEAD pointed to and the
//! tree of the working tree. Per peer we remember the base state in
//! `refs/gitsync-state/<peer>/base` (a namespace peers never fetch).

use anyhow::{bail, Context, Result};

use crate::fetch::peer_heads_prefix;
use crate::git::Git;
use crate::snapshot::{snapshot, wip_ref, working_tree, TempIndex};

/// Where the last in-sync state with `peer` is remembered.
pub fn base_ref(peer: &str) -> String {
    format!("refs/gitsync-state/{peer}/base")
}

/// What a snapshot (or the working tree) holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    /// Commit HEAD pointed to; `None` before the first commit.
    pub head: Option<String>,
    /// The working tree, as a tree object id.
    pub tree: String,
}

/// What should happen with a peer's latest snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// Nothing fetched from this peer yet.
    NoSnapshot,
    /// Our working tree and HEAD already match the peer's snapshot.
    InSync,
    /// The peer has newer work and ours is unchanged: take theirs.
    Apply {
        /// The peer's snapshot commit.
        snapshot: String,
        /// Our branch (moved too if the peer's HEAD is ahead).
        branch: String,
        from: State,
        to: State,
    },
    /// We changed and the peer did not: nothing to do, they fetch ours.
    OursNewer,
    /// Both changed since the last sync. Left alone (merging is step 6).
    Diverged,
    /// The peer is working on another branch; its snapshot is only stored.
    OtherBranch { ours: String, theirs: String },
    /// The peer's snapshot is based on a commit that is not ahead of ours.
    NotFastForward,
    /// Applying is not possible right now (reason for the user).
    Blocked(String),
}

impl Plan {
    /// States where the peer's work is held back until something changes.
    pub fn is_waiting(&self) -> bool {
        matches!(
            self,
            Plan::Diverged | Plan::OtherBranch { .. } | Plan::NotFastForward | Plan::Blocked(_)
        )
    }
}

impl std::fmt::Display for Plan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Plan::NoSnapshot => write!(f, "no snapshot fetched yet"),
            Plan::InSync => write!(f, "in sync"),
            Plan::Apply { .. } => write!(f, "has newer changes; they will be applied"),
            Plan::OursNewer => write!(f, "your changes are newer; the peer will fetch them"),
            Plan::Diverged => write!(
                f,
                "both sides changed since the last sync; not applied (no merging yet)"
            ),
            Plan::OtherBranch { ours, theirs } => {
                write!(
                    f,
                    "peer is on branch {theirs}, you are on {ours}; not applied"
                )
            }
            Plan::NotFastForward => {
                write!(
                    f,
                    "peer's work is based on a commit that is not ahead of your branch; not applied"
                )
            }
            Plan::Blocked(reason) => write!(f, "waiting: {reason}"),
        }
    }
}

/// What [`sync_peer`] did.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SyncReport {
    /// The decision for the peer's snapshot (`None` if it could not be made).
    pub plan: Option<Plan>,
    /// Files changed in the working tree, as `git diff --name-status` lines.
    pub applied_files: Vec<String>,
    /// Branches fast-forwarded, including the checked-out one when applying
    /// moved it: (branch, old commit, new commit).
    pub branches: Vec<(String, String, String)>,
}

/// Decides what to do with `peer`'s latest fetched snapshot. Changes nothing.
pub fn plan(git: &Git, machine_id: &str, peer: &str) -> Result<Plan> {
    let Some(snap) = rev(git, &wip_ref(peer))? else {
        return Ok(Plan::NoSnapshot);
    };
    let theirs = read_state(git, &snap)?;
    let theirs_branch = trailer(git, &snap, "gitsync-branch")?;

    let head = rev(git, "HEAD")?;
    let mine = State {
        head: head.clone(),
        tree: working_tree(git)?,
    };
    if mine == theirs {
        return Ok(Plan::InSync);
    }

    // Who has seen whose work? Snapshot histories answer that without
    // trusting clocks. Checked in this order so local work wins a tie.
    if let Some(ours) = rev(git, &wip_ref(machine_id))? {
        if history_states(git, &ours)?.contains(&theirs) {
            // We already had the peer's state and moved on.
            return Ok(Plan::OursNewer);
        }
    }
    let peer_saw_mine = history_states(git, &snap)?.contains(&mine);

    if !peer_saw_mine {
        // Fall back to the last state both sides had. Without a record, a
        // clean checkout of our HEAD counts as the common starting point.
        let base = match rev(git, &base_ref(peer))? {
            Some(commit) => read_state(git, &commit)?,
            None => match &head {
                Some(head) => State {
                    head: Some(head.clone()),
                    tree: git.run(&["rev-parse", &format!("{head}^{{tree}}")])?,
                },
                None => return Ok(Plan::Blocked("this repo has no commits yet".into())),
            },
        };
        if mine != base {
            return Ok(if theirs == base {
                Plan::OursNewer
            } else {
                Plan::Diverged
            });
        }
    }

    // Only the peer changed. Check it is safe to take its state.
    let Some(branch) = current_branch(git)? else {
        return Ok(Plan::Blocked("HEAD is detached".into()));
    };
    if theirs_branch != branch {
        return Ok(Plan::OtherBranch {
            ours: branch,
            theirs: theirs_branch,
        });
    }
    let (Some(our_head), Some(their_head)) = (&mine.head, &theirs.head) else {
        return Ok(Plan::NotFastForward);
    };
    if our_head != their_head && !is_ancestor(git, our_head, their_head)? {
        return Ok(Plan::NotFastForward);
    }
    if let Some(reason) = blocked_reason(git)? {
        return Ok(Plan::Blocked(reason));
    }
    Ok(Plan::Apply {
        snapshot: snap,
        branch,
        from: mine,
        to: theirs,
    })
}

/// Brings this repo in line with what was fetched from `peer`: applies its
/// snapshot if [`plan`] says so, then fast-forwards branches. Snapshots our
/// own state before and after applying, so nothing is ever only on disk.
pub fn sync_peer(git: &Git, machine_id: &str, peer: &str) -> Result<SyncReport> {
    let plan = plan(git, machine_id, peer)?;
    execute(git, machine_id, peer, plan)
}

/// Carries out a [`plan`] made earlier. If files changed since the plan was
/// made, applying fails and nothing is overwritten.
pub fn execute(git: &Git, machine_id: &str, peer: &str, plan: Plan) -> Result<SyncReport> {
    let mut report = SyncReport::default();
    match &plan {
        Plan::InSync => {
            // Remember this as the new common state.
            let snap = git.run(&["rev-parse", &wip_ref(peer)])?;
            if rev(git, &base_ref(peer))?.as_deref() != Some(snap.as_str()) {
                git.run(&["update-ref", &base_ref(peer), &snap])?;
            }
        }
        Plan::Apply {
            snapshot: snap,
            branch,
            from,
            to,
        } => {
            snapshot(git, machine_id)?; // safety copy of what we are replacing
            report.applied_files = changed_files(git, &from.tree, &to.tree)?;
            apply_state(git, branch, from, to, &format!("gitsync: apply {peer}"))?;
            if let (Some(old), Some(new)) = (&from.head, &to.head) {
                if old != new {
                    report
                        .branches
                        .push((branch.clone(), old.clone(), new.clone()));
                }
            }
            git.run(&["update-ref", &base_ref(peer), snap])?;
            snapshot(git, machine_id)?;
        }
        _ => {}
    }
    report.plan = Some(plan);
    report.branches.extend(fast_forward_branches(git, peer)?);
    Ok(report)
}

/// Fast-forwards our branches to the peer's where ours is strictly behind.
/// The checked-out branch is only moved when nothing at all is uncommitted
/// (and then the working tree follows); otherwise the snapshot rules apply.
fn fast_forward_branches(git: &Git, peer: &str) -> Result<Vec<(String, String, String)>> {
    let prefix = peer_heads_prefix(peer);
    let refs = git.run(&["for-each-ref", "--format=%(objectname) %(refname)", &prefix])?;
    let current = current_branch(git)?;
    let mut moved = Vec::new();
    for line in refs.lines() {
        let Some((theirs, refname)) = line.split_once(' ') else {
            continue;
        };
        let name = refname.strip_prefix(&prefix).unwrap_or(refname);
        let Some(ours) = rev(git, &format!("refs/heads/{name}"))? else {
            continue; // we don't have this branch: leave it in the peer namespace
        };
        if ours == theirs || !is_ancestor(git, &ours, theirs)? {
            continue;
        }
        if current.as_deref() == Some(name) {
            if !is_clean(git, &ours)? || blocked_reason(git)?.is_some() {
                continue;
            }
            let tree = |c: &str| git.run(&["rev-parse", &format!("{c}^{{tree}}")]);
            let from = State {
                head: Some(ours.clone()),
                tree: tree(&ours)?,
            };
            let to = State {
                head: Some(theirs.to_string()),
                tree: tree(theirs)?,
            };
            apply_state(
                git,
                name,
                &from,
                &to,
                &format!("gitsync: fast-forward from {peer}"),
            )?;
        } else {
            git.run(&[
                "update-ref",
                "-m",
                &format!("gitsync: fast-forward from {peer}"),
                &format!("refs/heads/{name}"),
                theirs,
                &ours,
            ])?;
        }
        moved.push((name.to_string(), ours, theirs.to_string()));
    }
    Ok(moved)
}

/// Turns the working tree from `from` into `to` (only touching files that
/// differ) and moves HEAD's branch if `to` is based on a newer commit.
///
/// Safe against the user editing at the same time: a private index of
/// `from.tree` is checked against the files on disk first, and git refuses
/// to overwrite any file that no longer matches, or an untracked file that
/// is in the way.
fn apply_state(git: &Git, branch: &str, from: &State, to: &State, reason: &str) -> Result<()> {
    let git_dir = git.git_dir()?;
    let temp_index = TempIndex::new(&git_dir);
    let env = [("GIT_INDEX_FILE", temp_index.path_str()?)];
    git.run_env(&["read-tree", &from.tree], &env)?;
    git.run_env(&["update-index", "--refresh"], &env)
        .context("files changed while applying; will retry later")?;
    git.run_env(&["read-tree", "-m", "-u", &from.tree, &to.tree], &env)?;

    if to.head != from.head {
        let (Some(new), Some(old)) = (&to.head, &from.head) else {
            bail!("cannot move a branch without commits");
        };
        // Through HEAD so the branch and HEAD reflogs both record it, and
        // compare-and-swap so a commit made meanwhile is never lost.
        if current_branch(git)?.as_deref() != Some(branch) {
            bail!("branch changed while applying");
        }
        git.run(&["update-ref", "-m", reason, "HEAD", new, old])?;
        // Point the staging area at the new commit; files stay as they are.
        git.run(&["reset", "-q"])?;
    }
    Ok(())
}

/// Why changing the working tree must wait, if it must.
fn blocked_reason(git: &Git) -> Result<Option<String>> {
    let git_dir = git.git_dir()?;
    let busy = [
        ("index.lock", "another git command is running"),
        ("MERGE_HEAD", "a merge is in progress"),
        ("rebase-merge", "a rebase is in progress"),
        ("rebase-apply", "a rebase or am is in progress"),
        ("CHERRY_PICK_HEAD", "a cherry-pick is in progress"),
        ("REVERT_HEAD", "a revert is in progress"),
        ("BISECT_LOG", "a bisect is in progress"),
    ];
    for (file, reason) in busy {
        if git_dir.join(file).exists() {
            return Ok(Some(reason.to_string()));
        }
    }
    if git.try_run(&["diff", "--cached", "--quiet"])?.is_none() {
        return Ok(Some("you have staged changes".to_string()));
    }
    Ok(None)
}

/// True if nothing is staged and the working tree matches commit `head`.
fn is_clean(git: &Git, head: &str) -> Result<bool> {
    let head_tree = git.run(&["rev-parse", &format!("{head}^{{tree}}")])?;
    Ok(working_tree(git)? == head_tree && git.try_run(&["diff", "--cached", "--quiet"])?.is_some())
}

/// States of the snapshots in `snapshot`'s history, newest first: its chain
/// of previous snapshots (first parents), up to a limit. Each machine's chain
/// includes the states it took over from peers, since it snapshots right
/// after applying.
fn history_states(git: &Git, snapshot: &str) -> Result<Vec<State>> {
    let out = git.run(&[
        "log",
        "--first-parent",
        "--max-count=500",
        "--format=%x01%T%x00%(trailers:key=gitsync-head,valueonly)",
        snapshot,
    ])?;
    Ok(out
        .split('\x01')
        .filter_map(|record| {
            let (tree, head) = record.split_once('\0')?;
            let head = head.trim();
            // Real commits (reached past the first snapshot) have no trailer.
            (!head.is_empty()).then(|| State {
                head: (head != "none").then(|| head.to_string()),
                tree: tree.trim().to_string(),
            })
        })
        .collect())
}

fn read_state(git: &Git, snapshot: &str) -> Result<State> {
    let head = trailer(git, snapshot, "gitsync-head")?;
    Ok(State {
        head: (head != "none" && !head.is_empty()).then_some(head),
        tree: git.run(&["rev-parse", &format!("{snapshot}^{{tree}}")])?,
    })
}

fn trailer(git: &Git, commit: &str, key: &str) -> Result<String> {
    git.run(&[
        "log",
        "-1",
        &format!("--format=%(trailers:key={key},valueonly)"),
        commit,
    ])
}

fn changed_files(git: &Git, from: &str, to: &str) -> Result<Vec<String>> {
    let out = git.run(&["diff", "--name-status", "--no-renames", from, to])?;
    Ok(out.lines().map(|l| l.replace('\t', " ")).collect())
}

fn rev(git: &Git, name: &str) -> Result<Option<String>> {
    git.try_run(&["rev-parse", "--verify", "-q", name])
}

fn current_branch(git: &Git) -> Result<Option<String>> {
    git.try_run(&["symbolic-ref", "--short", "-q", "HEAD"])
}

fn is_ancestor(git: &Git, ancestor: &str, descendant: &str) -> Result<bool> {
    Ok(git
        .try_run(&["merge-base", "--is-ancestor", ancestor, descendant])?
        .is_some())
}
