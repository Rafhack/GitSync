//! Fetching from peers. The "peer" is a second repo on disk; its path is used
//! as the URL, which git fetches from exactly like an ssh URL.

mod common;

use common::{clone_of, commit, repo, write};
use gitsync::config::Peer;
use gitsync::fetch::fetch_peer;
use gitsync::git::Git;
use gitsync::snapshot::snapshot;

fn peer(dir: &tempfile::TempDir) -> Peer {
    Peer {
        name: "desktop".into(),
        url: dir.path().to_str().unwrap().into(),
    }
}

fn rev(git: &Git, name: &str) -> Option<String> {
    git.try_run(&["rev-parse", "--verify", "-q", name]).unwrap()
}

#[test]
fn fetches_branches_and_snapshot_into_private_refs() {
    let (dir, laptop) = repo();
    let (deskdir, desktop) = clone_of(dir.path());
    let desk_head = commit(deskdir.path(), &desktop, "a.txt", "from desktop\n");
    desktop.run(&["branch", "feature"]).unwrap();
    write(deskdir.path(), "wip.txt", "unsaved\n");
    snapshot(&desktop, "desktop").unwrap();
    let desk_snap = rev(&desktop, "refs/gitsync/desktop/wip").unwrap();
    let laptop_main = rev(&laptop, "refs/heads/main");

    let report = fetch_peer(&laptop, &peer(&deskdir)).unwrap();

    assert!(report.has_snapshot);
    assert_eq!(report.changes.len(), 3, "{:?}", report.changes);
    assert_eq!(
        rev(&laptop, "refs/gitsync-peers/desktop/heads/main"),
        Some(desk_head.clone())
    );
    assert_eq!(
        rev(&laptop, "refs/gitsync-peers/desktop/heads/feature"),
        Some(desk_head)
    );
    assert_eq!(rev(&laptop, "refs/gitsync/desktop/wip"), Some(desk_snap));
    // Our own branches are never touched.
    assert_eq!(rev(&laptop, "refs/heads/main"), laptop_main);
    assert_eq!(rev(&laptop, "refs/heads/feature"), None);
    // The user's own FETCH_HEAD is left alone.
    assert!(!dir.path().join(".git/FETCH_HEAD").exists());

    let again = fetch_peer(&laptop, &peer(&deskdir)).unwrap();
    assert!(again.changes.is_empty(), "{:?}", again.changes);
}

#[test]
fn deleted_peer_branch_is_pruned() {
    let (dir, laptop) = repo();
    let (desk_dir, desktop) = clone_of(dir.path());
    desktop.run(&["branch", "temp"]).unwrap();
    fetch_peer(&laptop, &peer(&desk_dir)).unwrap();

    desktop.run(&["branch", "-D", "temp"]).unwrap();
    let report = fetch_peer(&laptop, &peer(&desk_dir)).unwrap();

    assert_eq!(rev(&laptop, "refs/gitsync-peers/desktop/heads/temp"), None);
    assert!(report
        .changes
        .iter()
        .any(|c| c.name.ends_with("/temp") && c.new.is_none()));
}

#[test]
fn only_takes_the_peers_own_snapshot() {
    let (dir, laptop) = repo();
    let (desk_dir, desktop) = clone_of(dir.path());
    write(dir.path(), "a.txt", "laptop wip\n");
    snapshot(&laptop, "laptop").unwrap();
    let our_snapshot = rev(&laptop, "refs/gitsync/laptop/wip");
    // The desktop holds an (older, different) snapshot claiming to be ours,
    // and one from a third machine.
    write(desk_dir.path(), "a.txt", "something else\n");
    snapshot(&desktop, "laptop").unwrap();
    snapshot(&desktop, "phone").unwrap();

    let report = fetch_peer(&laptop, &peer(&desk_dir)).unwrap();

    assert!(!report.has_snapshot, "desktop has no snapshot of its own");
    assert_eq!(rev(&laptop, "refs/gitsync/laptop/wip"), our_snapshot);
    assert_eq!(rev(&laptop, "refs/gitsync/phone/wip"), None);
}

#[test]
fn unreachable_peer_is_an_error_naming_the_peer() {
    let (_dir, laptop) = repo();
    let gone = Peer {
        name: "desktop".into(),
        url: "/nonexistent/gitsync/peer".into(),
    };

    let err = format!("{:#}", fetch_peer(&laptop, &gone).unwrap_err());

    assert!(err.contains("desktop"), "{err}");
}
