//! Turns uncommitted work into a hidden snapshot commit at
//! `refs/gitsync/<machine>/wip`, without touching the user's staging area or
//! files. See DESIGN.md §2.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{bail, Context, Result};

use crate::git::Git;

/// What a call to [`snapshot`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// A new snapshot commit was created (its id).
    Created(String),
    /// Nothing changed since the previous snapshot.
    Unchanged,
}

/// The ref holding this machine's latest snapshot.
pub fn wip_ref(machine_id: &str) -> String {
    format!("refs/gitsync/{machine_id}/wip")
}

/// Saves the current working tree (modified, deleted and new non-ignored
/// files) as a snapshot commit, unless it is identical to the previous one.
pub fn snapshot(git: &Git, machine_id: &str) -> Result<Outcome> {
    snapshot_including(git, machine_id, &[])
}

/// Like [`snapshot`], and records that the working tree now includes the
/// peer snapshots `merged` (taken over, merged, or seen and overruled): they
/// become extra parents, so peers can tell their work is already in ours.
/// Creates a snapshot even if the files did not change, unless the previous
/// snapshot already includes all of `merged`.
pub fn snapshot_including(git: &Git, machine_id: &str, merged: &[&str]) -> Result<Outcome> {
    let wip = wip_ref(machine_id);

    // 1 to 3. The working tree as a git tree object.
    let tree = working_tree(git)?;

    // 4. Skip if nothing changed since the previous snapshot.
    let head = git.try_run(&["rev-parse", "--verify", "-q", "HEAD"])?;
    let head_trailer = head.as_deref().unwrap_or("none");
    let prev = git.try_run(&["rev-parse", "--verify", "-q", &wip])?;
    if let Some(prev) = &prev {
        let mut known = true;
        for commit in merged {
            known &= git
                .try_run(&["merge-base", "--is-ancestor", commit, prev])?
                .is_some();
        }
        if known && is_same_snapshot(git, prev, &tree, head_trailer)? {
            return Ok(Outcome::Unchanged);
        }
    }

    // 5. Create the snapshot commit. Parents: previous snapshot, HEAD, then
    //    the included peer snapshots.
    let branch = git
        .try_run(&["symbolic-ref", "--short", "-q", "HEAD"])?
        .unwrap_or_else(|| "(detached)".to_string());
    let mut message = format!(
        "gitsync snapshot\n\n\
         gitsync-machine: {machine_id}\n\
         gitsync-branch: {branch}\n\
         gitsync-head: {head_trailer}\n"
    );
    for commit in merged {
        message.push_str(&format!("gitsync-merged: {commit}\n"));
    }
    let parents: Vec<&str> = prev
        .iter()
        .chain(head.iter())
        .map(String::as_str)
        .chain(merged.iter().copied())
        .collect();
    let commit = commit_tree(git, machine_id, &tree, &parents, &message)?;

    // 6. Move the ref with compare-and-swap: if another snapshot won the race,
    //    this fails instead of silently dropping that snapshot. An empty old
    //    value means "the ref must not exist yet".
    git.run(&[
        "update-ref",
        "-m",
        "gitsync snapshot",
        &wip,
        &commit,
        prev.as_deref().unwrap_or(""),
    ])?;

    Ok(Outcome::Created(commit))
}

/// Creates a commit object (no ref is moved) with gitsync's own identity, so
/// it works on machines without `user.name` configured.
pub(crate) fn commit_tree(
    git: &Git,
    machine_id: &str,
    tree: &str,
    parents: &[&str],
    message: &str,
) -> Result<String> {
    let mut args = vec!["commit-tree", tree, "-m", message];
    for parent in parents {
        args.extend(["-p", parent]);
    }
    let email = format!("gitsync@{machine_id}");
    let identity = [
        ("GIT_AUTHOR_NAME", "gitsync"),
        ("GIT_AUTHOR_EMAIL", email.as_str()),
        ("GIT_COMMITTER_NAME", "gitsync"),
        ("GIT_COMMITTER_EMAIL", email.as_str()),
    ];
    git.run_env(&args, &identity)
}

/// True if [`snapshot`] would create a new snapshot right now: the working
/// tree or HEAD changed since the latest one, or there is none yet.
/// Does not create a snapshot.
pub fn has_unsaved_changes(git: &Git, machine_id: &str) -> Result<bool> {
    let Some(prev) = git.try_run(&["rev-parse", "--verify", "-q", &wip_ref(machine_id)])? else {
        return Ok(true);
    };
    let tree = working_tree(git)?;
    let head = git.try_run(&["rev-parse", "--verify", "-q", "HEAD"])?;
    let same = is_same_snapshot(git, &prev, &tree, head.as_deref().unwrap_or("none"))?;
    Ok(!same)
}

/// Writes the working tree (respecting `.gitignore`) as a tree object and
/// returns its id, without touching the user's index.
pub fn working_tree(git: &Git) -> Result<String> {
    let git_dir = git.git_dir()?;

    // Private copy of the index. Copying keeps git's file-stat cache, so
    // `git add -A` below only re-reads files that actually changed.
    let temp_index = TempIndex::new(&git_dir);
    let real_index = git_dir.join("index");
    if real_index.exists() {
        fs::copy(&real_index, temp_index.path()).context("copying .git/index")?;
    }
    // (Fresh repo with nothing staged yet: git starts from an empty index.)

    // Stage everything into the private index and turn it into a tree.
    let index_env = [("GIT_INDEX_FILE", temp_index.path_str()?)];
    git.run_env(&["add", "-A"], &index_env)?;
    git.run_env(&["write-tree"], &index_env)
}

/// True if snapshot commit `snapshot` has this tree and was based on this HEAD.
fn is_same_snapshot(git: &Git, snapshot: &str, tree: &str, head_trailer: &str) -> Result<bool> {
    let snapshot_tree = git.run(&["rev-parse", &format!("{snapshot}^{{tree}}")])?;
    let snapshot_head = git.run(&[
        "log",
        "-1",
        "--format=%(trailers:key=gitsync-head,valueonly)",
        snapshot,
    ])?;
    Ok(snapshot_tree == tree && snapshot_head == head_trailer)
}

/// The machine id used when none is given: the hostname, made safe for use
/// inside a git ref name.
pub fn default_machine_id() -> Result<String> {
    let host = gethostname::gethostname().to_string_lossy().into_owned();
    let id = sanitize_machine_id(&host);
    validate_machine_id(&id)?;
    Ok(id)
}

/// Fails if `id` cannot be used as one component of a git ref name.
pub fn validate_machine_id(id: &str) -> Result<()> {
    let ok = !id.is_empty()
        && Git::new(".")
            .try_run(&["check-ref-format", &wip_ref(id)])?
            .is_some();
    if !ok || id.contains('/') {
        bail!("machine id {id:?} is not valid in a git ref name");
    }
    Ok(())
}

fn sanitize_machine_id(host: &str) -> String {
    let id: String = host
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let id = id.trim_matches(|c| c == '.' || c == '-').replace("..", "-");
    let id = id.strip_suffix(".lock").unwrap_or(&id).to_string();
    if id.is_empty() {
        "machine".to_string()
    } else {
        id
    }
}

/// A private index file in `.git/`, so gitsync can stage things without
/// touching the user's staging area. Deleted when dropped, even on error.
pub(crate) struct TempIndex(PathBuf);

impl TempIndex {
    /// A fresh, unique path (unique per process and per call, so threads
    /// cannot clobber each other's index). The file does not exist yet.
    pub(crate) fn new(git_dir: &Path) -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        Self(git_dir.join(format!("gitsync-index.{}.{n}", std::process::id())))
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }

    pub(crate) fn path_str(&self) -> Result<&str> {
        self.0.to_str().context("non UTF-8 path to .git")
    }
}

impl Drop for TempIndex {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::sanitize_machine_id;

    #[test]
    fn sanitizes_hostnames() {
        assert_eq!(sanitize_machine_id("laptop"), "laptop");
        assert_eq!(sanitize_machine_id("My Laptop"), "My-Laptop");
        assert_eq!(sanitize_machine_id("a..b"), "a-b");
        assert_eq!(sanitize_machine_id("box.lock"), "box");
        assert_eq!(sanitize_machine_id("~~"), "machine");
    }
}
