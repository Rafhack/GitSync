//! Applies what was fetched from a peer to this machine: its snapshot to the
//! working tree, and its branches as fast-forwards. See DESIGN.md §6.
//!
//! The rule is: never destroy unsynced local work. A peer's snapshot is
//! applied as is when our working tree has not changed since the last time
//! the two machines were in sync (the "base"). If both sides changed, the two
//! are merged when they changed different lines; if they changed the same
//! lines, nothing is touched until the user settles it (`gitsync resolve`).
//!
//! A "state" is what a snapshot captures: the commit HEAD pointed to and the
//! tree of the working tree. Per peer we remember the base state in
//! `refs/gitsync-state/<peer>/base` (a namespace peers never fetch).
//!
//! Every snapshot taken after applying or merging a peer's work links that
//! peer's snapshot as an extra parent. So the snapshot graph records whose
//! work each machine has taken in, and git finds the common base for merges
//! in it.

use anyhow::{bail, Context, Result};

use crate::fetch::peer_heads_prefix;
use crate::git::Git;
use crate::snapshot::{
    commit_tree, snapshot, snapshot_including, wip_ref, working_tree, TempIndex,
};

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
    /// Both changed since the last sync, in different places: `to` combines
    /// both, and replaces our state.
    Merge {
        /// The peer's snapshot commit.
        snapshot: String,
        /// Our branch (moved too if the peer's HEAD is ahead).
        branch: String,
        from: State,
        to: State,
    },
    /// We changed and the peer did not: nothing to do, they fetch ours.
    OursNewer,
    /// Both changed the same lines (or one side deleted a file the other
    /// changed). Nothing is applied until `gitsync resolve` settles it.
    /// `files` are the conflicting paths.
    Conflict { files: Vec<String> },
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
            Plan::Conflict { .. }
                | Plan::OtherBranch { .. }
                | Plan::NotFastForward
                | Plan::Blocked(_)
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
            Plan::Merge { .. } => write!(
                f,
                "both sides changed different things; they will be merged"
            ),
            Plan::Conflict { files } => {
                const SHOWN: usize = 5;
                let mut list = files[..files.len().min(SHOWN)].join(", ");
                if files.len() > SHOWN {
                    list.push_str(&format!(" and {} more", files.len() - SHOWN));
                }
                write!(
                    f,
                    "conflict: both sides changed {list} in ways that do not merge; not applied (see `gitsync resolve`)"
                )
            }
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

/// Which side wins where both changed the same lines (`gitsync resolve`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Mine,
    Theirs,
}

/// Decides what to do with `peer`'s latest fetched snapshot. Changes nothing
/// (it may write git objects, never refs or files).
pub fn plan(git: &Git, machine_id: &str, peer: &str) -> Result<Plan> {
    make_plan(git, machine_id, peer, None)
}

fn make_plan(git: &Git, machine_id: &str, peer: &str, favor: Option<Side>) -> Result<Plan> {
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

    // Who has taken in whose work? The snapshot graph answers that without
    // trusting clocks. Checked in this order so local work wins a tie.
    let ours = match rev(git, &wip_ref(machine_id))? {
        Some(commit) => Some((read_state(git, &commit)?, commit)),
        None => None,
    };
    if let Some((_, ours)) = &ours {
        if is_ancestor(git, &snap, ours)? {
            // We already have the peer's latest work.
            return Ok(Plan::OursNewer);
        }
    }
    let peer_has_mine = match &ours {
        // (Only if we did not change anything since that snapshot.)
        Some((state, commit)) => *state == mine && is_ancestor(git, commit, &snap)?,
        None => false,
    };

    let both_changed = !peer_has_mine && {
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
        if mine != base && theirs == base {
            return Ok(Plan::OursNewer);
        }
        mine != base
    };

    // Check it is safe to take the peer's work.
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
    // The commit the result is based on: the newer of the two HEADs.
    let new_head = if our_head == their_head || is_ancestor(git, our_head, their_head)? {
        their_head.clone()
    } else if both_changed && is_ancestor(git, their_head, our_head)? {
        our_head.clone()
    } else {
        return Ok(Plan::NotFastForward);
    };

    let to = if both_changed {
        // Our state as a commit linked into the snapshot graph: the latest
        // snapshot if it is current, else a throwaway one on top of it.
        let ours_commit = match &ours {
            Some((state, commit)) if *state == mine => commit.clone(),
            _ => {
                let parents: Vec<&str> = ours
                    .iter()
                    .map(|(_, commit)| commit.as_str())
                    .chain(head.as_deref())
                    .collect();
                commit_tree(
                    git,
                    machine_id,
                    &mine.tree,
                    &parents,
                    "gitsync working tree",
                )?
            }
        };
        match merge(git, peer, &ours_commit, &snap, favor)? {
            Merged::Clean(tree) => State {
                head: Some(new_head),
                tree,
            },
            Merged::Conflict(files) => return Ok(Plan::Conflict { files }),
        }
    } else {
        theirs.clone()
    };

    if let Some(reason) = blocked_reason(git)? {
        return Ok(Plan::Blocked(reason));
    }
    // A merge that comes out as exactly the peer's state (their work already
    // contained ours, through a third machine) is just taking theirs.
    Ok(if both_changed && to != theirs {
        Plan::Merge {
            snapshot: snap,
            branch,
            from: mine,
            to,
        }
    } else {
        Plan::Apply {
            snapshot: snap,
            branch,
            from: mine,
            to,
        }
    })
}

/// Settles a conflict with `peer`: merges again, taking `side`'s version
/// where both changed the same lines (and of whole files where one side
/// deleted what the other changed), then applies the result like any merge.
/// Changes on only one side are kept either way.
pub fn resolve(git: &Git, machine_id: &str, peer: &str, side: Side) -> Result<SyncReport> {
    let current = plan(git, machine_id, peer)?;
    if !matches!(current, Plan::Conflict { .. }) {
        bail!("no conflict with {peer} to resolve ({current})");
    }
    let resolved = make_plan(git, machine_id, peer, Some(side))?;
    // (Taking theirs where we changed nothing else is just applying theirs.)
    if !matches!(resolved, Plan::Merge { .. } | Plan::Apply { .. }) {
        bail!("cannot resolve the conflict with {peer} now: {resolved}");
    }
    execute(git, machine_id, peer, resolved)
}

enum Merged {
    Clean(String),
    Conflict(Vec<String>),
}

/// 3-way merge of two snapshot commits with `git merge-tree`. The base is
/// their common ancestor in the snapshot graph, or the last in-sync state
/// with `peer` when that is newer (states both sides reached on their own,
/// like the same edit made twice, are not linked in the graph).
fn merge(git: &Git, peer: &str, ours: &str, theirs: &str, favor: Option<Side>) -> Result<Merged> {
    let mut args = vec![
        "merge-tree",
        "--write-tree",
        "-z",
        "--name-only",
        "--no-messages",
    ];
    let base_arg;
    if let Some(base) = recorded_base(git, peer, ours, theirs)? {
        base_arg = format!("--merge-base={base}");
        args.push(&base_arg);
    }
    match favor {
        Some(Side::Mine) => args.extend(["-X", "ours"]),
        Some(Side::Theirs) => args.extend(["-X", "theirs"]),
        None => {}
    }
    args.extend([ours, theirs]);
    let (clean, out) = git.run_ok_or_1(&args)?;
    // Output: the tree, then the conflicted paths, NUL separated.
    let mut fields = out.split('\0');
    let tree = fields.next().unwrap_or_default().trim().to_string();
    if clean {
        return Ok(Merged::Clean(tree));
    }
    let mut files: Vec<String> = fields
        .take_while(|field| !field.is_empty())
        .map(String::from)
        .collect();
    files.dedup();
    Ok(match favor {
        None => Merged::Conflict(files),
        // What `-X` cannot settle (a file deleted on one side and changed on
        // the other, a file against a folder): take that side's version.
        Some(side) => {
            let winner = if side == Side::Mine { ours } else { theirs };
            Merged::Clean(take_versions(git, &tree, &files, winner)?)
        }
    })
}

/// The recorded base with `peer`, if every common ancestor of `ours` and
/// `theirs` in the snapshot graph is also an ancestor of it.
fn recorded_base(git: &Git, peer: &str, ours: &str, theirs: &str) -> Result<Option<String>> {
    let Some(base) = rev(git, &base_ref(peer))? else {
        return Ok(None);
    };
    if let Some(found) = git.try_run(&["merge-base", "--all", ours, theirs])? {
        for ancestor in found.lines() {
            if !is_ancestor(git, ancestor, &base)? {
                return Ok(None);
            }
        }
    }
    Ok(Some(base))
}

/// `tree` with each of `paths` replaced by its version in commit `source`
/// (removed where `source` has none).
fn take_versions(git: &Git, tree: &str, paths: &[String], source: &str) -> Result<String> {
    let temp_index = TempIndex::new(&git.git_dir()?);
    let env = [
        ("GIT_INDEX_FILE", temp_index.path_str()?),
        ("GIT_LITERAL_PATHSPECS", "1"),
    ];
    git.run_env(&["read-tree", tree], &env)?;
    for path in paths {
        git.run_env(
            &["rm", "--cached", "-r", "-q", "--ignore-unmatch", "--", path],
            &env,
        )?;
        let entries = git.run_env(&["ls-tree", "-r", "-z", source, "--", path], &env)?;
        for entry in entries.split('\0').filter(|e| !e.is_empty()) {
            // "<mode> <type> <oid>\t<path>"
            let Some((info, name)) = entry.split_once('\t') else {
                continue;
            };
            let info: Vec<&str> = info.split(' ').collect();
            let [mode, _, oid] = info[..] else {
                continue;
            };
            git.run_env(
                &[
                    "update-index",
                    "--add",
                    "--replace",
                    "--cacheinfo",
                    mode,
                    oid,
                    name,
                ],
                &env,
            )?;
        }
    }
    git.run_env(&["write-tree"], &env)
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
        }
        | Plan::Merge {
            snapshot: snap,
            branch,
            from,
            to,
        } => {
            let verb = if matches!(plan, Plan::Merge { .. }) {
                "merge"
            } else {
                "apply"
            };
            snapshot(git, machine_id)?; // safety copy of what we are replacing
            report.applied_files = changed_files(git, &from.tree, &to.tree)?;
            apply_state(git, branch, from, to, &format!("gitsync: {verb} {peer}"))?;
            if let (Some(old), Some(new)) = (&from.head, &to.head) {
                if old != new {
                    report
                        .branches
                        .push((branch.clone(), old.clone(), new.clone()));
                }
            }
            git.run(&["update-ref", &base_ref(peer), snap])?;
            // Record that the peer's work is in ours now.
            snapshot_including(git, machine_id, &[snap.as_str()])?;
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
