//! Helpers shared by the integration tests: throwaway git repos.
// Each test file uses only some of these helpers.
#![allow(dead_code)]

use std::fs;
use std::path::Path;

use gitsync::git::Git;
use tempfile::TempDir;

/// An empty repo with a local identity (so tests don't depend on global git config).
pub fn empty_repo() -> (TempDir, Git) {
    let dir = TempDir::new().unwrap();
    let git = Git::new(dir.path());
    git.run(&["init", "-q", "-b", "main"]).unwrap();
    git.run(&["config", "user.name", "Test"]).unwrap();
    git.run(&["config", "user.email", "test@example.com"])
        .unwrap();
    (dir, git)
}

/// A repo with one commit containing `a.txt` and `b.txt`.
pub fn repo() -> (TempDir, Git) {
    let (dir, git) = empty_repo();
    write(dir.path(), "a.txt", "a\n");
    write(dir.path(), "b.txt", "b\n");
    git.run(&["add", "."]).unwrap();
    git.run(&["commit", "-q", "-m", "initial"]).unwrap();
    (dir, git)
}

pub fn write(dir: &Path, name: &str, contents: &str) {
    fs::write(dir.join(name), contents).unwrap();
}
