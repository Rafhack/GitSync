//! Config file and `gitsync status` tests, on throwaway repos.

mod common;

use common::{repo, write};
use gitsync::config::{Config, Discovery};
use gitsync::snapshot::{has_unsaved_changes, snapshot, wip_ref};
use gitsync::status::Status;

const ID: &str = "testbox";

#[test]
fn missing_config_file_gives_defaults() {
    let (_dir, git) = repo();
    assert_eq!(Config::load(&git).unwrap(), Config::default());
}

#[test]
fn reads_config_from_dot_git() {
    let (dir, git) = repo();
    write(
        &dir.path().join(".git"),
        "gitsync.toml",
        "machine_id = \"laptop\"\ndiscovery = \"static\"\n\
         [[peers]]\nname = \"desktop\"\nurl = \"ssh://me@desktop.local/code/proj\"\n",
    );

    let config = Config::load(&git).unwrap();

    assert_eq!(config.machine_id_or_default().unwrap(), "laptop");
    assert_eq!(config.discovery, Discovery::Static);
    assert_eq!(config.peers.len(), 1);
}

#[test]
fn bad_config_error_names_the_file() {
    let (dir, git) = repo();
    write(
        &dir.path().join(".git"),
        "gitsync.toml",
        "poll_interval_secs = \"soon\"\n",
    );

    let err = format!("{:#}", Config::load(&git).unwrap_err());

    assert!(err.contains("gitsync.toml"), "{err}");
}

#[test]
fn unsaved_changes_tracks_working_tree_without_snapshotting() {
    let (dir, git) = repo();
    assert!(has_unsaved_changes(&git, ID).unwrap(), "no snapshot yet");
    assert!(
        git.try_run(&["rev-parse", "--verify", "-q", &wip_ref(ID)])
            .unwrap()
            .is_none(),
        "checking must not create a snapshot"
    );

    snapshot(&git, ID).unwrap();
    assert!(!has_unsaved_changes(&git, ID).unwrap());

    write(dir.path(), "a.txt", "edited\n");
    assert!(has_unsaved_changes(&git, ID).unwrap());
}

#[test]
fn status_reports_own_and_other_snapshots() {
    let (dir, git) = repo();
    write(dir.path(), "a.txt", "edited\n");
    snapshot(&git, ID).unwrap();
    // Pretend another machine's snapshot was fetched into this repo.
    snapshot(&git, "desktop").unwrap();
    write(dir.path(), "a.txt", "edited again\n");

    let status = Status::collect(&git, ID, Config::default()).unwrap();

    assert_eq!(status.branch.as_deref(), Some("main"));
    assert!(status.head.is_some());
    assert!(!status.config_found);
    assert_eq!(status.snapshot.as_ref().unwrap().branch, "main");
    assert!(status.unsaved_changes);
    assert_eq!(status.other_snapshots.len(), 1);
    assert_eq!(status.other_snapshots[0].0, "desktop");

    let text = status.to_string();
    assert!(text.contains("machine   testbox"), "{text}");
    assert!(text.contains("changes not in a snapshot yet"), "{text}");
    assert!(text.contains("desktop"), "{text}");
}

#[test]
fn status_in_fresh_repo() {
    let (_dir, git) = common::empty_repo();

    let text = Status::collect(&git, ID, Config::default())
        .unwrap()
        .to_string();

    assert!(text.contains("no commits yet"), "{text}");
    assert!(text.contains("snapshot  none yet"), "{text}");
}
