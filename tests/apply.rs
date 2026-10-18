//! Applying a peer's snapshot and branches (DESIGN.md §6). Two repos on disk
//! play "laptop" (the one applying) and "desktop" (the peer).

mod common;

use std::fs;
use std::path::Path;

use common::{clone_of, commit, repo, write};
use gitsync::apply::{base_ref, execute, plan, sync_peer, Plan};
use gitsync::config::Peer;
use gitsync::fetch::fetch_peer;
use gitsync::git::Git;
use gitsync::snapshot::{snapshot, wip_ref};
use tempfile::TempDir;

struct Machine {
    dir: TempDir,
    git: Git,
    id: &'static str,
}

impl Machine {
    fn path(&self) -> &Path {
        self.dir.path()
    }
    /// A file's contents, with Windows line endings turned into `\n` (git
    /// writes `\r\n` there when `core.autocrlf` is on, as checkout does).
    fn read(&self, file: &str) -> Option<String> {
        let text = fs::read_to_string(self.path().join(file)).ok()?;
        Some(text.replace("\r\n", "\n"))
    }
    fn write(&self, file: &str, contents: &str) {
        write(self.path(), file, contents);
    }
    fn snap(&self) {
        snapshot(&self.git, self.id).unwrap();
    }
    /// Fetch `other` and apply what is safe, like one round of `watch`.
    fn sync_from(&self, other: &Machine) -> gitsync::apply::SyncReport {
        self.fetch(other);
        sync_peer(&self.git, self.id, other.id).unwrap()
    }
    fn fetch(&self, other: &Machine) {
        let peer = Peer {
            name: other.id.into(),
            url: other.path().to_str().unwrap().into(),
        };
        fetch_peer(&self.git, &peer).unwrap();
    }
    fn rev(&self, name: &str) -> Option<String> {
        self.git
            .try_run(&["rev-parse", "--verify", "-q", name])
            .unwrap()
    }
    fn staged(&self) -> String {
        self.git.run(&["diff", "--cached", "--name-only"]).unwrap()
    }
}

/// "laptop" with one commit (a.txt, b.txt) and "desktop" cloned from it.
fn pair() -> (Machine, Machine) {
    let (dir, git) = repo();
    let (desk_dir, desk_git) = clone_of(dir.path());
    let laptop = Machine {
        dir,
        git,
        id: "laptop",
    };
    let desktop = Machine {
        dir: desk_dir,
        git: desk_git,
        id: "desktop",
    };
    laptop.snap();
    desktop.snap();
    (laptop, desktop)
}

#[test]
fn clean_machine_takes_peer_work() {
    let (laptop, desktop) = pair();
    desktop.write("a.txt", "desktop edit\n");
    desktop.write("new.txt", "new\n");
    fs::remove_file(desktop.path().join("b.txt")).unwrap();
    desktop.snap();

    let report = laptop.sync_from(&desktop);

    assert!(
        matches!(report.plan, Some(Plan::Apply { .. })),
        "{report:?}"
    );
    assert_eq!(laptop.read("a.txt").as_deref(), Some("desktop edit\n"));
    assert_eq!(laptop.read("new.txt").as_deref(), Some("new\n"));
    assert_eq!(laptop.read("b.txt"), None);
    assert_eq!(report.applied_files, ["M a.txt", "D b.txt", "A new.txt"]);
    // The staging area is untouched and HEAD did not move.
    assert_eq!(laptop.staged(), "");
    assert_eq!(laptop.rev("HEAD"), desktop.rev("HEAD"));
    // Our own snapshot now matches theirs, and the sync point is recorded.
    let tree = |m: &Machine, r: &str| m.rev(&format!("{r}^{{tree}}"));
    assert_eq!(
        tree(&laptop, &wip_ref("laptop")),
        tree(&desktop, &wip_ref("desktop"))
    );
    assert_eq!(
        laptop.rev(&base_ref("desktop")),
        desktop.rev(&wip_ref("desktop"))
    );
}

#[test]
fn applied_work_does_not_echo_back() {
    let (laptop, desktop) = pair();
    desktop.write("a.txt", "desktop edit\n");
    desktop.snap();
    laptop.sync_from(&desktop);

    let report = desktop.sync_from(&laptop);

    assert_eq!(report.plan, Some(Plan::InSync));
    assert!(report.applied_files.is_empty());
}

#[test]
fn keeps_syncing_back_and_forth() {
    let (laptop, desktop) = pair();
    desktop.write("a.txt", "one\n");
    desktop.snap();
    laptop.sync_from(&desktop);
    desktop.sync_from(&laptop);

    laptop.write("a.txt", "two\n");
    laptop.snap();
    let report = desktop.sync_from(&laptop);
    assert!(
        matches!(report.plan, Some(Plan::Apply { .. })),
        "{report:?}"
    );
    assert_eq!(desktop.read("a.txt").as_deref(), Some("two\n"));

    desktop.write("a.txt", "three\n");
    desktop.snap();
    laptop.sync_from(&desktop);
    assert_eq!(laptop.read("a.txt").as_deref(), Some("three\n"));
}

#[test]
fn local_changes_are_never_overwritten() {
    let (laptop, desktop) = pair();
    laptop.write("a.txt", "laptop edit\n");
    laptop.snap();

    // Peer unchanged: ours is newer.
    let report = laptop.sync_from(&desktop);
    assert_eq!(report.plan, Some(Plan::OursNewer));

    // Peer changed too: both changed, nothing is touched on either side.
    desktop.write("b.txt", "desktop edit\n");
    desktop.snap();
    assert_eq!(laptop.sync_from(&desktop).plan, Some(Plan::Diverged));
    assert_eq!(desktop.sync_from(&laptop).plan, Some(Plan::Diverged));
    assert_eq!(laptop.read("a.txt").as_deref(), Some("laptop edit\n"));
    assert_eq!(laptop.read("b.txt").as_deref(), Some("b\n"));
    assert_eq!(desktop.read("a.txt").as_deref(), Some("a\n"));
}

#[test]
fn unsnapshotted_local_edit_also_counts_as_a_change() {
    let (laptop, desktop) = pair();
    desktop.write("a.txt", "desktop\n");
    desktop.snap();
    laptop.write("b.txt", "typed but not snapshotted yet\n");

    assert_eq!(laptop.sync_from(&desktop).plan, Some(Plan::Diverged));
    assert_eq!(
        laptop.read("b.txt").as_deref(),
        Some("typed but not snapshotted yet\n")
    );
}

#[test]
fn edit_after_planning_makes_apply_fail_safely() {
    let (laptop, desktop) = pair();
    desktop.write("a.txt", "desktop\n");
    desktop.snap();
    laptop.fetch(&desktop);
    let planned = plan(&laptop.git, "laptop", "desktop").unwrap();
    assert!(matches!(planned, Plan::Apply { .. }));

    laptop.write("a.txt", "user typed this just now\n");
    let result = execute(&laptop.git, "laptop", "desktop", planned);

    assert!(result.is_err());
    assert_eq!(
        laptop.read("a.txt").as_deref(),
        Some("user typed this just now\n")
    );
}

#[test]
fn peer_commits_fast_forward_our_branch_with_their_work() {
    let (laptop, desktop) = pair();
    let new_head = commit(desktop.path(), &desktop.git, "a.txt", "committed\n");
    desktop.write("b.txt", "uncommitted on top\n");
    desktop.snap();

    let report = laptop.sync_from(&desktop);

    assert!(
        matches!(report.plan, Some(Plan::Apply { .. })),
        "{report:?}"
    );
    assert_eq!(report.branches.len(), 1, "the branch move is reported");
    assert_eq!(laptop.rev("refs/heads/main"), Some(new_head));
    assert_eq!(laptop.read("a.txt").as_deref(), Some("committed\n"));
    assert_eq!(
        laptop.read("b.txt").as_deref(),
        Some("uncommitted on top\n")
    );
    // Nothing staged; the peer's uncommitted edit is uncommitted here too.
    assert_eq!(laptop.staged(), "");
    let status = laptop.git.run(&["status", "--porcelain"]).unwrap();
    assert_eq!(status, "M b.txt");
    let reflog = laptop
        .git
        .run(&["reflog", "-1", "--format=%gs", "main"])
        .unwrap();
    assert!(reflog.contains("gitsync"), "{reflog}");
}

#[test]
fn peer_on_another_branch_is_not_applied() {
    let (laptop, desktop) = pair();
    desktop
        .git
        .run(&["checkout", "-q", "-b", "feature"])
        .unwrap();
    desktop.write("a.txt", "on feature\n");
    desktop.snap();

    let report = laptop.sync_from(&desktop);

    assert!(
        matches!(report.plan, Some(Plan::OtherBranch { .. })),
        "{report:?}"
    );
    assert_eq!(laptop.read("a.txt").as_deref(), Some("a\n"));
}

#[test]
fn waits_while_git_is_busy_or_something_is_staged() {
    let (laptop, desktop) = pair();
    // In sync with an edit, then the laptop user stages that edit.
    laptop.write("a.txt", "shared\n");
    laptop.snap();
    desktop.sync_from(&laptop);
    laptop.sync_from(&desktop);
    laptop.git.run(&["add", "a.txt"]).unwrap();
    desktop.write("b.txt", "desktop\n");
    desktop.snap();

    let report = laptop.sync_from(&desktop);
    assert!(matches!(report.plan, Some(Plan::Blocked(_))), "{report:?}");
    assert_eq!(laptop.read("b.txt").as_deref(), Some("b\n"));

    laptop.git.run(&["reset", "-q"]).unwrap();
    fs::write(laptop.path().join(".git/MERGE_HEAD"), "x").unwrap();
    let report = laptop.sync_from(&desktop);
    assert_eq!(
        report.plan,
        Some(Plan::Blocked("a merge is in progress".into()))
    );

    fs::remove_file(laptop.path().join(".git/MERGE_HEAD")).unwrap();
    laptop.sync_from(&desktop);
    assert_eq!(laptop.read("b.txt").as_deref(), Some("desktop\n"));
}

#[test]
fn branches_fast_forward_only_when_behind() {
    let (laptop, desktop) = pair();
    for branch in ["behind", "diverged", "ahead"] {
        laptop.git.run(&["branch", branch]).unwrap();
        desktop.git.run(&["branch", branch, "origin/main"]).unwrap();
    }
    let on = |m: &Machine, branch: &str, file: &str| {
        m.git.run(&["checkout", "-q", branch]).unwrap();
        let c = commit(m.path(), &m.git, file, branch);
        m.git.run(&["checkout", "-q", "main"]).unwrap();
        c
    };
    let desk_behind = on(&desktop, "behind", "x.txt");
    on(&desktop, "diverged", "x.txt");
    let ours_diverged = on(&laptop, "diverged", "y.txt");
    let ours_ahead = on(&laptop, "ahead", "y.txt");
    desktop.git.run(&["branch", "only-there"]).unwrap();

    let report = laptop.sync_from(&desktop);

    assert_eq!(report.branches.len(), 1, "{:?}", report.branches);
    assert_eq!(laptop.rev("refs/heads/behind"), Some(desk_behind));
    assert_eq!(laptop.rev("refs/heads/diverged"), Some(ours_diverged));
    assert_eq!(laptop.rev("refs/heads/ahead"), Some(ours_ahead));
    assert_eq!(laptop.rev("refs/heads/only-there"), None);
}

#[test]
fn checked_out_branch_follows_peer_only_when_clean() {
    let (laptop, desktop) = pair();
    // The desktop moved main forward but is now working on another branch,
    // so only its branch (not its snapshot) says where main is.
    let new_main = commit(desktop.path(), &desktop.git, "a.txt", "newer\n");
    desktop.git.run(&["checkout", "-q", "-b", "other"]).unwrap();
    desktop.snap();

    laptop.write("b.txt", "dirty\n");
    laptop.sync_from(&desktop);
    assert_ne!(laptop.rev("refs/heads/main"), Some(new_main.clone()));
    assert_eq!(laptop.read("a.txt").as_deref(), Some("a\n"));

    laptop.write("b.txt", "b\n");
    let report = laptop.sync_from(&desktop);
    assert_eq!(laptop.rev("refs/heads/main"), Some(new_main));
    assert_eq!(laptop.read("a.txt").as_deref(), Some("newer\n"));
    assert_eq!(report.branches.len(), 1);
    assert_eq!(laptop.git.run(&["status", "--porcelain"]).unwrap(), "");
}

#[test]
fn work_travels_through_a_third_machine() {
    let (laptop, desktop) = pair();
    let (phone_dir, phone_git) = clone_of(laptop.path());
    let phone = Machine {
        dir: phone_dir,
        git: phone_git,
        id: "phone",
    };
    phone.snap();

    // laptop → desktop → phone; the phone never talks to the laptop.
    laptop.write("a.txt", "from laptop\n");
    laptop.snap();
    desktop.sync_from(&laptop);
    phone.sync_from(&desktop);
    assert_eq!(phone.read("a.txt").as_deref(), Some("from laptop\n"));

    // The phone edits; the laptop takes it straight from the phone.
    phone.write("b.txt", "from phone\n");
    phone.snap();
    let report = laptop.sync_from(&phone);
    assert!(
        matches!(report.plan, Some(Plan::Apply { .. })),
        "{report:?}"
    );
    assert_eq!(laptop.read("b.txt").as_deref(), Some("from phone\n"));
    assert_eq!(laptop.read("a.txt").as_deref(), Some("from laptop\n"));

    // And the desktop, which only saw the laptop's first edit, catches up.
    desktop.sync_from(&laptop);
    assert_eq!(desktop.read("b.txt").as_deref(), Some("from phone\n"));
}
