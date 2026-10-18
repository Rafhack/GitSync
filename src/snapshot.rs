//! Turns uncommitted work into a hidden snapshot commit at
//! `refs/gitsync/<machine>/wip`, without touching the user's staging area or
//! files. See DESIGN.md §2.

use std::fs;
use std::path::PathBuf;

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
    let wip = wip_ref(machine_id);
    let git_dir = git.git_dir()?;

    // 1. Private copy of the index. Copying keeps git's file-stat cache, so
    //    `git add -A` below only re-reads files that actually changed.
    let temp_index = TempIndex(git_dir.join(format!("gitsync-index.{}", std::process::id())));
    let real_index = git_dir.join("index");
    if real_index.exists() {
        fs::copy(&real_index, &temp_index.0).context("copying .git/index")?;
    } else {
        // Fresh repo with nothing staged yet: start from an empty index.
        let _ = fs::remove_file(&temp_index.0);
    }
    let index_path = temp_index.0.to_str().context("non UTF-8 path to .git")?;
    let index_env = [("GIT_INDEX_FILE", index_path)];

    // 2 and 3. Stage everything into the private index and turn it into a tree.
    git.run_env(&["add", "-A"], &index_env)?;
    let tree = git.run_env(&["write-tree"], &index_env)?;

    // 4. Skip if nothing changed since the previous snapshot.
    let head = git.try_run(&["rev-parse", "--verify", "-q", "HEAD"])?;
    let head_trailer = head.as_deref().unwrap_or("none");
    let prev = git.try_run(&["rev-parse", "--verify", "-q", &wip])?;
    if let Some(prev) = &prev {
        let prev_tree = git.run(&["rev-parse", &format!("{prev}^{{tree}}")])?;
        let prev_head = git.run(&[
            "log",
            "-1",
            "--format=%(trailers:key=gitsync-head,valueonly)",
            prev,
        ])?;
        if prev_tree == tree && prev_head == head_trailer {
            return Ok(Outcome::Unchanged);
        }
    }

    // 5. Create the snapshot commit. Parents: previous snapshot, then HEAD.
    let branch = git
        .try_run(&["symbolic-ref", "--short", "-q", "HEAD"])?
        .unwrap_or_else(|| "(detached)".to_string());
    let message = format!(
        "gitsync snapshot\n\n\
         gitsync-machine: {machine_id}\n\
         gitsync-branch: {branch}\n\
         gitsync-head: {head_trailer}\n"
    );
    let mut args = vec!["commit-tree", tree.as_str(), "-m", message.as_str()];
    for parent in prev.iter().chain(head.iter()) {
        args.extend(["-p", parent.as_str()]);
    }
    let email = format!("gitsync@{machine_id}");
    let identity = [
        ("GIT_AUTHOR_NAME", "gitsync"),
        ("GIT_AUTHOR_EMAIL", email.as_str()),
        ("GIT_COMMITTER_NAME", "gitsync"),
        ("GIT_COMMITTER_EMAIL", email.as_str()),
    ];
    let commit = git.run_env(&args, &identity)?;

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

/// Deletes the private index file when the snapshot finishes, even on error.
struct TempIndex(PathBuf);

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
