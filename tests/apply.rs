//! Applying a peer's snapshot and branches (DESIGN.md §6). Two repos on disk
//! play "laptop" (the one applying) and "desktop" (the peer).

mod common;

use std::fs;
use std::path::Path;

use common::{clone_of, commit, repo, write};
use gitsync::apply::{base_ref, execute, plan, resolve, sync_peer, Plan, Side};
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
        let peer = Peer::new(other.id, other.path().to_str().unwrap());
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
fn ours_newer_when_only_we_changed() {
    let (laptop, desktop) = pair();
    laptop.write("a.txt", "laptop edit\n");
    laptop.snap();

    let report = laptop.sync_from(&desktop);
    assert_eq!(report.plan, Some(Plan::OursNewer));
    assert_eq!(laptop.read("a.txt").as_deref(), Some("laptop edit\n"));
}

#[test]
fn changes_to_different_files_are_merged() {
    let (laptop, desktop) = pair();
    laptop.write("a.txt", "laptop edit\n");
    laptop.snap();
    desktop.write("b.txt", "desktop edit\n");
    desktop.write("new.txt", "new\n");
    desktop.snap();

    let report = laptop.sync_from(&desktop);
    assert!(
        matches!(report.plan, Some(Plan::Merge { .. })),
        "{report:?}"
    );
    assert_eq!(report.applied_files, ["M b.txt", "A new.txt"]);
    assert_eq!(laptop.read("a.txt").as_deref(), Some("laptop edit\n"));
    assert_eq!(laptop.read("b.txt").as_deref(), Some("desktop edit\n"));
    assert_eq!(laptop.staged(), "");

    // The desktop sees its own work inside the merge and simply takes it.
    let report = desktop.sync_from(&laptop);
    assert!(
        matches!(report.plan, Some(Plan::Apply { .. })),
        "{report:?}"
    );
    assert_eq!(desktop.read("a.txt").as_deref(), Some("laptop edit\n"));
    assert_eq!(desktop.read("new.txt").as_deref(), Some("new\n"));
    assert_eq!(laptop.sync_from(&desktop).plan, Some(Plan::InSync));
}

#[test]
fn different_lines_of_one_file_are_merged() {
    let (laptop, desktop) = pair();
    let text = "1\n2\n3\n4\n5\n6\n7\n8\n";
    laptop.write("a.txt", text);
    laptop.snap();
    desktop.sync_from(&laptop);

    laptop.write("a.txt", &text.replace("1\n", "one\n"));
    laptop.snap();
    desktop.write("a.txt", &text.replace("8\n", "eight\n"));
    desktop.snap();

    let report = laptop.sync_from(&desktop);
    assert!(
        matches!(report.plan, Some(Plan::Merge { .. })),
        "{report:?}"
    );
    let both = "one\n2\n3\n4\n5\n6\n7\neight\n";
    assert_eq!(laptop.read("a.txt").as_deref(), Some(both));
    desktop.sync_from(&laptop);
    assert_eq!(desktop.read("a.txt").as_deref(), Some(both));
}

#[test]
fn later_edits_merge_on_top_of_the_last_merge() {
    let (laptop, desktop) = pair();
    let text = "1\n2\n3\n4\n5\n6\n7\n8\n";
    laptop.write("a.txt", text);
    laptop.snap();
    desktop.sync_from(&laptop);

    laptop.write("a.txt", &text.replace("1\n", "one\n"));
    laptop.snap();
    desktop.write("b.txt", "desktop\n");
    desktop.snap();
    laptop.sync_from(&desktop); // merge
    desktop.sync_from(&laptop); // takes the merge

    // The desktop changes the line the laptop changed before; the laptop
    // changes another file. The base is the merge, so no conflict.
    let merged = text.replace("1\n", "one\n");
    desktop.write("a.txt", &merged.replace("one\n", "ONE\n"));
    desktop.snap();
    laptop.write("b.txt", "laptop\n");
    laptop.snap();

    let report = laptop.sync_from(&desktop);
    assert!(
        matches!(report.plan, Some(Plan::Merge { .. })),
        "{report:?}"
    );
    assert_eq!(
        laptop.read("a.txt").as_deref(),
        Some(merged.replace("one\n", "ONE\n").as_str())
    );
    assert_eq!(laptop.read("b.txt").as_deref(), Some("laptop\n"));
}

#[test]
fn merging_at_the_same_time_on_both_sides_converges() {
    let (laptop, desktop) = pair();
    laptop.write("a.txt", "laptop\n");
    laptop.snap();
    desktop.write("b.txt", "desktop\n");
    desktop.snap();
    laptop.fetch(&desktop);
    desktop.fetch(&laptop);

    // Each merges the other's work before hearing about the other's merge.
    let mut plans = Vec::new();
    for (me, peer) in [(&laptop, &desktop), (&desktop, &laptop)] {
        plans.push(sync_peer(&me.git, me.id, peer.id).unwrap().plan);
    }
    assert!(
        plans.iter().all(|p| matches!(p, Some(Plan::Merge { .. }))),
        "{plans:?}"
    );

    assert_eq!(laptop.sync_from(&desktop).plan, Some(Plan::InSync));
    assert_eq!(desktop.sync_from(&laptop).plan, Some(Plan::InSync));
    for machine in [&laptop, &desktop] {
        assert_eq!(machine.read("a.txt").as_deref(), Some("laptop\n"));
        assert_eq!(machine.read("b.txt").as_deref(), Some("desktop\n"));
    }
}

#[test]
fn unsnapshotted_local_edit_is_kept_in_the_merge() {
    let (laptop, desktop) = pair();
    desktop.write("a.txt", "desktop\n");
    desktop.snap();
    laptop.write("b.txt", "typed but not snapshotted yet\n");

    let report = laptop.sync_from(&desktop);
    assert!(
        matches!(report.plan, Some(Plan::Merge { .. })),
        "{report:?}"
    );
    assert_eq!(laptop.read("a.txt").as_deref(), Some("desktop\n"));
    assert_eq!(
        laptop.read("b.txt").as_deref(),
        Some("typed but not snapshotted yet\n")
    );
}

/// Both machines change line 1 of a.txt; the laptop also changes b.txt and
/// the desktop c.txt.
fn conflicting_pair() -> (Machine, Machine) {
    let (laptop, desktop) = pair();
    laptop.write("a.txt", "laptop\n");
    laptop.write("b.txt", "laptop b\n");
    laptop.snap();
    desktop.write("a.txt", "desktop\n");
    desktop.write("c.txt", "desktop c\n");
    desktop.snap();
    (laptop, desktop)
}

#[test]
fn same_lines_changed_on_both_sides_is_a_conflict() {
    let (laptop, desktop) = conflicting_pair();

    let conflict = Plan::Conflict {
        files: vec!["a.txt".to_string()],
    };
    assert!(conflict.is_waiting());
    assert_eq!(laptop.sync_from(&desktop).plan.as_ref(), Some(&conflict));
    assert_eq!(desktop.sync_from(&laptop).plan.as_ref(), Some(&conflict));
    // Nothing was touched on either side.
    assert_eq!(laptop.read("a.txt").as_deref(), Some("laptop\n"));
    assert_eq!(laptop.read("c.txt"), None);
    assert_eq!(desktop.read("a.txt").as_deref(), Some("desktop\n"));
    assert_eq!(desktop.read("b.txt").as_deref(), Some("b\n"));
}

#[test]
fn resolve_taking_theirs() {
    let (laptop, desktop) = conflicting_pair();
    laptop.fetch(&desktop);

    let report = resolve(&laptop.git, "laptop", "desktop", Side::Theirs).unwrap();
    assert!(
        matches!(report.plan, Some(Plan::Merge { .. })),
        "{report:?}"
    );
    assert_eq!(laptop.read("a.txt").as_deref(), Some("desktop\n"));
    // Changes on only one side are kept.
    assert_eq!(laptop.read("b.txt").as_deref(), Some("laptop b\n"));
    assert_eq!(laptop.read("c.txt").as_deref(), Some("desktop c\n"));

    // The desktop takes the result as is: no conflict on its side either.
    let report = desktop.sync_from(&laptop);
    assert!(
        matches!(report.plan, Some(Plan::Apply { .. })),
        "{report:?}"
    );
    assert_eq!(desktop.read("a.txt").as_deref(), Some("desktop\n"));
    assert_eq!(desktop.read("b.txt").as_deref(), Some("laptop b\n"));
}

#[test]
fn resolve_keeping_mine() {
    let (laptop, desktop) = conflicting_pair();
    laptop.fetch(&desktop);

    resolve(&laptop.git, "laptop", "desktop", Side::Mine).unwrap();
    assert_eq!(laptop.read("a.txt").as_deref(), Some("laptop\n"));
    assert_eq!(laptop.read("c.txt").as_deref(), Some("desktop c\n"));
    assert_eq!(laptop.sync_from(&desktop).plan, Some(Plan::OursNewer));

    desktop.sync_from(&laptop);
    assert_eq!(desktop.read("a.txt").as_deref(), Some("laptop\n"));
    assert_eq!(desktop.read("b.txt").as_deref(), Some("laptop b\n"));
    assert_eq!(desktop.read("c.txt").as_deref(), Some("desktop c\n"));
}

#[test]
fn resolve_when_one_side_deleted_the_file() {
    for (side, expected) in [(Side::Mine, None), (Side::Theirs, Some("desktop\n"))] {
        let (laptop, desktop) = pair();
        fs::remove_file(laptop.path().join("a.txt")).unwrap();
        laptop.snap();
        desktop.write("a.txt", "desktop\n");
        desktop.snap();

        let report = laptop.sync_from(&desktop);
        assert_eq!(
            report.plan,
            Some(Plan::Conflict {
                files: vec!["a.txt".to_string()]
            })
        );
        resolve(&laptop.git, "laptop", "desktop", side).unwrap();
        assert_eq!(laptop.read("a.txt").as_deref(), expected, "{side:?}");
    }
}

#[test]
fn resolve_needs_a_conflict_and_waits_for_staged_changes() {
    let (laptop, desktop) = pair();
    let err = resolve(&laptop.git, "laptop", "desktop", Side::Mine).unwrap_err();
    assert!(err.to_string().contains("no conflict"), "{err}");

    let (laptop, desktop2) = conflicting_pair();
    drop(desktop);
    laptop.fetch(&desktop2);
    laptop.git.run(&["add", "b.txt"]).unwrap();
    let err = resolve(&laptop.git, "laptop", "desktop", Side::Theirs).unwrap_err();
    assert!(err.to_string().contains("staged"), "{err}");
    assert_eq!(laptop.read("a.txt").as_deref(), Some("laptop\n"));
}

#[test]
fn going_back_to_an_earlier_state_still_syncs() {
    let (laptop, desktop) = pair();
    desktop.write("a.txt", "edit\n");
    desktop.snap();
    laptop.sync_from(&desktop);
    desktop.sync_from(&laptop);

    // The desktop undoes its edit: a state the laptop had before.
    desktop.write("a.txt", "a\n");
    desktop.snap();
    let report = laptop.sync_from(&desktop);
    assert!(
        matches!(report.plan, Some(Plan::Apply { .. })),
        "{report:?}"
    );
    assert_eq!(laptop.read("a.txt").as_deref(), Some("a\n"));
    assert_eq!(desktop.sync_from(&laptop).plan, Some(Plan::InSync));
}

#[test]
fn merge_follows_a_peer_commit() {
    let (laptop, desktop) = pair();
    laptop.write("a.txt", "laptop, not committed\n");
    laptop.snap();
    let new_head = commit(
        desktop.path(),
        &desktop.git,
        "b.txt",
        "committed on desktop\n",
    );
    desktop.write("c.txt", "desktop, not committed\n");
    desktop.snap();

    let report = laptop.sync_from(&desktop);
    assert!(
        matches!(report.plan, Some(Plan::Merge { .. })),
        "{report:?}"
    );
    assert_eq!(laptop.rev("HEAD"), Some(new_head));
    assert_eq!(
        laptop.read("a.txt").as_deref(),
        Some("laptop, not committed\n")
    );
    assert_eq!(
        laptop.read("b.txt").as_deref(),
        Some("committed on desktop\n")
    );
    assert_eq!(
        laptop.read("c.txt").as_deref(),
        Some("desktop, not committed\n")
    );
    // Only our own edit (and the peer's uncommitted file) show as changes.
    assert_eq!(laptop.staged(), "");
    assert_eq!(
        laptop.git.run(&["status", "--porcelain"]).unwrap(),
        "M a.txt\n?? c.txt" // (output is trimmed)
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

#[test]
fn merge_already_made_elsewhere_is_taken_as_is() {
    let (laptop, desktop) = pair();
    let (phone_dir, phone_git) = clone_of(laptop.path());
    let phone = Machine {
        dir: phone_dir,
        git: phone_git,
        id: "phone",
    };
    phone.snap();

    // The desktop merges the laptop's edit with its own; the phone has only
    // the laptop's edit so far.
    laptop.write("a.txt", "laptop\n");
    laptop.snap();
    desktop.write("b.txt", "desktop\n");
    desktop.snap();
    desktop.sync_from(&laptop);
    phone.sync_from(&laptop);

    // The desktop's merge contains everything the phone has: taken as is.
    let report = phone.sync_from(&desktop);
    assert!(
        matches!(report.plan, Some(Plan::Apply { .. })),
        "{report:?}"
    );
    assert_eq!(phone.read("a.txt").as_deref(), Some("laptop\n"));
    assert_eq!(phone.read("b.txt").as_deref(), Some("desktop\n"));
}
