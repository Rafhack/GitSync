//! The gitsync transport end to end (DESIGN.md §11): pairing, fetching over
//! TLS, and two `gitsync watch` syncing each other. Everything runs on
//! localhost with static peers and fixed ports: no ssh server and no mDNS
//! needed. Each "machine" has its own GITSYNC_CONFIG_DIR (key, trust list).

mod common;

use std::fs;
use std::io::{BufRead, BufReader};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::mpsc;
use std::thread::sleep;
use std::time::{Duration, Instant};

use common::{clone_of, repo, write};
use gitsync::git::Git;
use tempfile::TempDir;

const BIN: &str = env!("CARGO_BIN_EXE_gitsync");

/// A machine: its own key and trust list (in `home`).
struct Machine {
    home: TempDir,
}

impl Machine {
    fn new() -> Self {
        Self {
            home: TempDir::new().unwrap(),
        }
    }

    fn gitsync(&self, dir: &Path, args: &[&str]) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.arg("-C")
            .arg(dir)
            .args(args)
            .env("GITSYNC_CONFIG_DIR", self.home.path());
        cmd
    }

    fn run(&self, dir: &Path, args: &[&str]) -> Output {
        self.gitsync(dir, args).output().unwrap()
    }

    fn key(&self) -> String {
        let out = self.run(self.home.path(), &["--machine-id", "x", "id"]);
        let text = String::from_utf8(out.stdout).unwrap();
        text.lines()
            .find_map(|l| l.strip_prefix("key"))
            .unwrap()
            .trim()
            .to_string()
    }

    /// Trusts `other` under `name`.
    fn trust(&self, name: &str, other: &Machine) {
        let out = self.run(self.home.path(), &["trust", name, &other.key()]);
        assert!(out.status.success(), "{}", text(&out));
    }

    fn trust_list(&self) -> String {
        fs::read_to_string(self.home.path().join("trusted_peers.toml")).unwrap_or_default()
    }
}

/// Kills the child when dropped, so a failing test leaves nothing running.
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn wait_until(what: &str, done: impl FnMut() -> bool) {
    wait_until_or_show(what, &[], done)
}

/// Like [`wait_until`], showing these logs if it times out.
fn wait_until_or_show(what: &str, logs: &[&Path], mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !done() {
        if Instant::now() > deadline {
            let logs: Vec<String> = logs
                .iter()
                .map(|log| fs::read_to_string(log).unwrap_or_default())
                .collect();
            panic!("timed out waiting for {what}\n{}", logs.join("\n---\n"));
        }
        sleep(Duration::from_millis(100));
    }
}

/// A file's text with `\n` line ends (git may check files out with `\r\n`
/// on Windows), or "" if it does not exist.
fn read(path: &Path) -> String {
    fs::read_to_string(path)
        .unwrap_or_default()
        .replace("\r\n", "\n")
}

fn rev(git: &Git, name: &str) -> Option<String> {
    git.try_run(&["rev-parse", "--verify", "-q", name]).unwrap()
}

fn config(dir: &Path, text: &str) {
    fs::write(dir.join(".git/gitsync.toml"), text).unwrap();
}

/// Starts `gitsync watch` and waits until it serves.
fn watch(machine: &Machine, dir: &Path) -> (Running, PathBuf) {
    let log = dir.join(".git/test-watch.log");
    let child = machine
        .gitsync(dir, &["watch", "--log-file", log.to_str().unwrap()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let running = Running(child);
    wait_until("watch to serve", || {
        fs::read_to_string(&log).is_ok_and(|t| t.contains("serving paired machines"))
    });
    (running, log)
}

/// Starts a waiting `gitsync pair` and returns it with the code it printed.
fn wait_for_pairing(machine: &Machine, name: &str, port: u16) -> (Running, String) {
    let mut child = machine
        .gitsync(
            machine.home.path(),
            &["--machine-id", name, "pair", "--port", &port.to_string()],
        )
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            let _ = tx.send(line);
        }
    });
    let code = loop {
        let line = rx.recv_timeout(Duration::from_secs(20)).expect("a code");
        if let Some(code) = line.strip_prefix("pairing code: ") {
            break code.to_string();
        }
    };
    (Running(child), code)
}

#[test]
fn pairing_by_code_trusts_both_ways() {
    let (desktop, laptop) = (Machine::new(), Machine::new());
    let port = free_port();
    let (mut waiting, code) = wait_for_pairing(&desktop, "desktop", port);

    let host = format!("127.0.0.1:{port}");
    let out = laptop.run(
        laptop.home.path(),
        &["--machine-id", "laptop", "pair", &code, "--host", &host],
    );
    assert!(out.status.success(), "{}", text(&out));
    assert!(text(&out).contains("paired with desktop"), "{}", text(&out));
    assert!(waiting.0.wait().unwrap().success());

    // Each trusts the other's real key, under its name.
    assert!(laptop.trust_list().contains("machine = \"desktop\""));
    assert!(laptop.trust_list().contains(&desktop.key()));
    assert!(desktop.trust_list().contains("machine = \"laptop\""));
    assert!(desktop.trust_list().contains(&laptop.key()));
}

#[test]
fn pairing_with_a_wrong_code_trusts_nothing() {
    let (desktop, laptop) = (Machine::new(), Machine::new());
    let port = free_port();
    let (mut waiting, code) = wait_for_pairing(&desktop, "desktop", port);
    let wrong = if code == "000-000" {
        "000-001"
    } else {
        "000-000"
    };

    let host = format!("127.0.0.1:{port}");
    let out = laptop.run(
        laptop.home.path(),
        &["--machine-id", "laptop", "pair", wrong, "--host", &host],
    );
    assert!(!out.status.success());
    assert!(text(&out).contains("codes did not match"), "{}", text(&out));
    // One attempt only: the waiting side gave up too.
    assert!(!waiting.0.wait().unwrap().success());
    assert_eq!(laptop.trust_list(), "");
    assert_eq!(desktop.trust_list(), "");
}

#[test]
fn fetches_over_the_gitsync_transport_and_only_from_paired_machines() {
    let (desk_dir, desk_git) = repo();
    let (lap_dir, lap_git) = clone_of(desk_dir.path());
    let (desktop, laptop) = (Machine::new(), Machine::new());
    desktop.trust("laptop", &laptop);
    laptop.trust("desktop", &desktop);

    let port = free_port();
    config(
        desk_dir.path(),
        &format!("machine_id = \"desktop\"\ndiscovery = \"static\"\n[serve]\nport = {port}\n"),
    );
    let peer = format!(
        "machine_id = \"laptop\"\ndiscovery = \"static\"\n\
         [[peers]]\nname = \"desktop\"\nurl = \"gitsync://127.0.0.1:{port}\"\n"
    );
    config(lap_dir.path(), &peer);
    write(desk_dir.path(), "wip.txt", "unsaved\n");
    // Something the desktop fetched from a third machine: never offered.
    desk_git
        .run(&["update-ref", "refs/gitsync-peers/phone/heads/main", "HEAD"])
        .unwrap();
    let (_watch, _log) = watch(&desktop, desk_dir.path());

    let out = laptop.run(lap_dir.path(), &["fetch"]);
    assert!(out.status.success(), "{}", text(&out));
    assert_eq!(
        rev(&lap_git, "refs/gitsync-peers/desktop/heads/main"),
        rev(&desk_git, "refs/heads/main")
    );
    assert_eq!(
        rev(&lap_git, "refs/gitsync/desktop/wip"),
        rev(&desk_git, "refs/gitsync/desktop/wip")
    );
    assert!(rev(&lap_git, "refs/gitsync/desktop/wip").is_some());

    // What the server offers, seen through the same connection.
    let repo_id = lap_git
        .run(&["rev-list", "--max-parents=0", "HEAD"])
        .unwrap();
    let ext = |repo: &str| format!("ext::{BIN} connect desktop 127.0.0.1:{port} {repo} %S");
    let ls = Command::new("git")
        .args([
            "-c",
            "protocol.ext.allow=always",
            "ls-remote",
            &ext(&repo_id),
        ])
        .env("GITSYNC_CONFIG_DIR", laptop.home.path())
        .output()
        .unwrap();
    assert!(ls.status.success(), "{}", text(&ls));
    let refs = String::from_utf8_lossy(&ls.stdout);
    assert!(refs.contains("refs/heads/main"), "{refs}");
    assert!(!refs.contains("gitsync-peers"), "{refs}");

    // Another repo id: refused, with the reason.
    let other = Command::new("git")
        .args([
            "-c",
            "protocol.ext.allow=always",
            "ls-remote",
            &ext("abc123"),
        ])
        .env("GITSYNC_CONFIG_DIR", laptop.home.path())
        .output()
        .unwrap();
    assert!(!other.status.success());
    assert!(
        text(&other).contains("does not serve that repo"),
        "{}",
        text(&other)
    );

    // A machine that trusts the desktop, but the desktop does not trust it.
    let stranger = Machine::new();
    stranger.trust("desktop", &desktop);
    let out = stranger.run(lap_dir.path(), &["fetch"]);
    assert!(!out.status.success());
    assert!(
        text(&out).contains("does not trust this machine"),
        "{}",
        text(&out)
    );

    // A machine with no pairing for the desktop does not even connect.
    let out = Machine::new().run(lap_dir.path(), &["fetch"]);
    assert!(!out.status.success());
    assert!(
        text(&out).contains("not paired with desktop"),
        "{}",
        text(&out)
    );

    // The desktop's key paired under another name: whoever answers as
    // "desktop" must show the key paired as "desktop".
    let confused = Machine::new();
    confused.trust("server", &desktop);
    confused.trust("desktop", &stranger);
    let out = confused.run(lap_dir.path(), &["fetch"]);
    assert!(!out.status.success());
    assert!(
        text(&out).contains("not the key paired as desktop"),
        "{}",
        text(&out)
    );
}

#[test]
fn two_watches_sync_over_the_gitsync_transport() {
    let (desk_dir, _desk_git) = repo();
    let (lap_dir, _lap_git) = clone_of(desk_dir.path());
    let (desktop, laptop) = (Machine::new(), Machine::new());
    desktop.trust("laptop", &laptop);
    laptop.trust("desktop", &desktop);

    let (desk_port, lap_port) = (free_port(), free_port());
    let setup = |me: &str, port: u16, peer: &str, peer_port: u16| {
        format!(
            "machine_id = \"{me}\"\ndiscovery = \"static\"\npoll_interval_secs = 1\n\
             [serve]\nport = {port}\n\
             [[peers]]\nname = \"{peer}\"\nurl = \"gitsync://127.0.0.1:{peer_port}\"\n"
        )
    };
    config(
        desk_dir.path(),
        &setup("desktop", desk_port, "laptop", lap_port),
    );
    config(
        lap_dir.path(),
        &setup("laptop", lap_port, "desktop", desk_port),
    );
    let (_desk, desk_log) = watch(&desktop, desk_dir.path());
    let (_lap, lap_log) = watch(&laptop, lap_dir.path());
    let logs = [desk_log.as_path(), lap_log.as_path()];

    write(desk_dir.path(), "a.txt", "edited on the desktop\n");
    wait_until_or_show("the edit to reach the laptop", &logs, || {
        read(&lap_dir.path().join("a.txt")) == "edited on the desktop\n"
    });

    write(lap_dir.path(), "new.txt", "from the laptop\n");
    wait_until_or_show("the new file to reach the desktop", &logs, || {
        read(&desk_dir.path().join("new.txt")) == "from the laptop\n"
    });
}
