//! Snapshot tests. Each test builds a throwaway git repo in a temp directory.

mod common;

use std::fs;

use common::{empty_repo, repo, write};
use gitsync::git::Git;
use gitsync::snapshot::{snapshot, wip_ref, Outcome};

const ID: &str = "testbox";

fn created(outcome: Outcome) -> String {
    match outcome {
        Outcome::Created(commit) => commit,
        Outcome::Unchanged => panic!("expected a new snapshot"),
    }
}

/// File names in a commit's tree.
fn files(git: &Git, commit: &str) -> Vec<String> {
    let out = git.run(&["ls-tree", "-r", "--name-only", commit]).unwrap();
    out.lines().map(str::to_string).collect()
}

fn parents(git: &Git, commit: &str) -> Vec<String> {
    let out = git.run(&["rev-list", "--parents", "-n1", commit]).unwrap();
    out.split_whitespace().skip(1).map(str::to_string).collect()
}

#[test]
fn captures_modified_new_and_deleted_files() {
    let (dir, git) = repo();
    write(dir.path(), "a.txt", "changed\n");
    write(dir.path(), "new.txt", "new\n");
    fs::remove_file(dir.path().join("b.txt")).unwrap();

    let commit = created(snapshot(&git, ID).unwrap());

    assert_eq!(files(&git, &commit), ["a.txt", "new.txt"]);
    assert_eq!(
        git.run(&["show", &format!("{commit}:a.txt")]).unwrap(),
        "changed"
    );
    assert_eq!(git.run(&["rev-parse", &wip_ref(ID)]).unwrap(), commit);
}

#[test]
fn leaves_staging_area_and_files_alone() {
    let (dir, git) = repo();
    write(dir.path(), "a.txt", "staged\n");
    git.run(&["add", "a.txt"]).unwrap();
    write(dir.path(), "a.txt", "staged then edited\n");
    write(dir.path(), "untracked.txt", "u\n");
    let before = git.run(&["status", "--porcelain"]).unwrap();
    let staged_before = git.run(&["diff", "--cached"]).unwrap();

    created(snapshot(&git, ID).unwrap());

    assert_eq!(git.run(&["status", "--porcelain"]).unwrap(), before);
    assert_eq!(git.run(&["diff", "--cached"]).unwrap(), staged_before);
    assert_eq!(
        fs::read_to_string(dir.path().join("a.txt")).unwrap(),
        "staged then edited\n"
    );
}

#[test]
fn skips_ignored_files() {
    let (dir, git) = repo();
    write(dir.path(), ".gitignore", "*.log\n");
    write(dir.path(), "debug.log", "noise\n");

    let commit = created(snapshot(&git, ID).unwrap());

    assert_eq!(files(&git, &commit), [".gitignore", "a.txt", "b.txt"]);
}

#[test]
fn no_changes_means_no_new_snapshot() {
    let (dir, git) = repo();
    write(dir.path(), "a.txt", "changed\n");
    let first = created(snapshot(&git, ID).unwrap());

    assert_eq!(snapshot(&git, ID).unwrap(), Outcome::Unchanged);
    assert_eq!(git.run(&["rev-parse", &wip_ref(ID)]).unwrap(), first);
}

#[test]
fn new_snapshot_has_previous_snapshot_and_head_as_parents() {
    let (dir, git) = repo();
    let head = git.run(&["rev-parse", "HEAD"]).unwrap();
    write(dir.path(), "a.txt", "one\n");
    let first = created(snapshot(&git, ID).unwrap());
    assert_eq!(parents(&git, &first), std::slice::from_ref(&head));

    write(dir.path(), "a.txt", "two\n");
    let second = created(snapshot(&git, ID).unwrap());

    assert_eq!(parents(&git, &second), [first, head]);
    let message = git.run(&["log", "-1", "--format=%B", &second]).unwrap();
    assert!(message.contains(&format!("gitsync-machine: {ID}")));
    assert!(message.contains("gitsync-branch: main"));
}

#[test]
fn commit_with_same_tree_still_makes_new_snapshot() {
    let (dir, git) = repo();
    write(dir.path(), "a.txt", "changed\n");
    created(snapshot(&git, ID).unwrap());

    // Committing moves HEAD but leaves the working tree the same.
    git.run(&["commit", "-q", "-am", "second"]).unwrap();
    let after_commit = created(snapshot(&git, ID).unwrap());

    let head = git.run(&["rev-parse", "HEAD"]).unwrap();
    assert_eq!(parents(&git, &after_commit)[1], head);
    assert_eq!(snapshot(&git, ID).unwrap(), Outcome::Unchanged);
}

#[test]
fn works_in_repo_without_commits() {
    let (dir, git) = empty_repo();
    write(dir.path(), "first.txt", "hello\n");

    let commit = created(snapshot(&git, ID).unwrap());

    assert!(parents(&git, &commit).is_empty());
    assert_eq!(files(&git, &commit), ["first.txt"]);
    let message = git.run(&["log", "-1", "--format=%B", &commit]).unwrap();
    assert!(message.contains("gitsync-head: none"));
    assert_eq!(snapshot(&git, ID).unwrap(), Outcome::Unchanged);
}

#[test]
fn leaves_no_temp_index_behind() {
    let (dir, git) = repo();
    write(dir.path(), "a.txt", "changed\n");
    created(snapshot(&git, ID).unwrap());

    let leftovers: Vec<_> = fs::read_dir(dir.path().join(".git"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("gitsync-index"))
        .collect();
    assert!(leftovers.is_empty(), "left behind: {leftovers:?}");
}
