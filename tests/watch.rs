//! `gitsync watch` as a process: one per repo, and its log file.

mod common;

use std::fs;
use std::process::{Child, Command, Stdio};
use std::thread::sleep;
use std::time::{Duration, Instant};

use common::repo;
use gitsync::watch::{is_running, running_pid};

fn gitsync(dir: &std::path::Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gitsync"));
    cmd.arg("-C").arg(dir).args(args);
    cmd
}

/// Kills the child when dropped, so a failing test leaves nothing running.
struct Running(Child);

impl Drop for Running {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn wait_until(mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !done() {
        assert!(Instant::now() < deadline, "timed out");
        sleep(Duration::from_millis(100));
    }
}

#[test]
fn only_one_watch_per_repo_and_it_can_log_to_a_file() {
    let (dir, git) = repo();
    // No mDNS: this test is about the process, not the network.
    fs::write(
        dir.path().join(".git/gitsync.toml"),
        "machine_id = \"tester\"\ndiscovery = \"static\"\n",
    )
    .unwrap();
    let log = dir.path().join(".git/test-logs/watch.log");
    assert!(!is_running(&git).unwrap());

    let first = gitsync(dir.path(), &["watch", "--log-file", log.to_str().unwrap()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut first = Running(first);
    wait_until(|| fs::read_to_string(&log).is_ok_and(|text| text.contains("snapshot")));
    assert!(is_running(&git).unwrap());
    assert_eq!(running_pid(&git).unwrap(), Some(first.0.id()));

    let second = gitsync(dir.path(), &["watch"]).output().unwrap();
    assert!(!second.status.success());
    let message = String::from_utf8_lossy(&second.stderr);
    assert!(message.contains("already running"), "{message}");

    // The log file is plain text (no terminal colors).
    let text = fs::read_to_string(&log).unwrap();
    assert!(text.contains("watching"), "{text}");
    assert!(!text.contains('\u{1b}'), "{text}");

    // The OS releases the lock when the process ends, however it ends.
    first.0.kill().unwrap();
    first.0.wait().unwrap();
    assert!(!is_running(&git).unwrap());
    assert_eq!(running_pid(&git).unwrap(), None);
}

fn kill(pid: u32) {
    let status = if cfg!(windows) {
        Command::new("taskkill")
            .args(["/PID", &pid.to_string(), "/F"])
            .stdout(Stdio::null())
            .status()
    } else {
        Command::new("kill").args(["-9", &pid.to_string()]).status()
    };
    assert!(status.unwrap().success());
}

#[test]
fn service_loop_starts_watch_again_when_it_stops() {
    let (dir, git) = repo();
    fs::write(
        dir.path().join(".git/gitsync.toml"),
        "machine_id = \"tester\"\ndiscovery = \"static\"\n",
    )
    .unwrap();
    let log = dir.path().join(".git/test-logs/service.log");

    let service = gitsync(
        dir.path(),
        &["service", "run", "--log-file", log.to_str().unwrap()],
    )
    .stdout(Stdio::null())
    .stderr(Stdio::null())
    .spawn()
    .unwrap();
    let _service = Running(service);
    let mut first = None;
    wait_until(|| {
        first = running_pid(&git).unwrap();
        first.is_some()
    });

    // watch dies; the loop notices, logs it and starts a new one.
    kill(first.unwrap());
    let mut second = None;
    wait_until(|| {
        second = running_pid(&git).unwrap();
        second.is_some_and(|pid| Some(pid) != first)
    });
    let text = fs::read_to_string(&log).unwrap();
    assert!(text.contains("starting it again in 10 s"), "{text}");

    // Ending the loop does not end the watch it started: clean up.
    drop(_service);
    kill(second.unwrap());
}
