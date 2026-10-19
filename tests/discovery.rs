//! Real mDNS between two repos on this machine (multicast loops back to the
//! sender, so one process can play both sides). Needs a network interface
//! with multicast, which any LAN-connected machine has.

mod common;

use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use common::{clone_of, repo, write};
use gitsync::config::Config;
use gitsync::discovery::{Announcement, Found, Mdns, PeerEvent};
use gitsync::snapshot::snapshot;

/// Waits for the first event matching `want`, skipping others.
fn wait_for(events: &Receiver<PeerEvent>, want: impl Fn(&PeerEvent) -> bool) -> PeerEvent {
    let deadline = Instant::now() + Duration::from_secs(15);
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match events.recv_timeout(left) {
            Ok(event) if want(&event) => return event,
            Ok(_) => continue,
            Err(_) => break,
        }
    }
    panic!("no matching mDNS event within 15 s");
}

fn found(event: PeerEvent) -> Found {
    match event {
        PeerEvent::Found(found) => found,
        other => panic!("expected Found, got {other:?}"),
    }
}

#[test]
fn peers_find_each_other_and_see_news() {
    let (laptop_dir, laptop) = repo();
    let (desk_dir, desktop) = clone_of(laptop_dir.path());
    let config = Config::default();

    let laptop_ann = Announcement::for_repo(&laptop, "laptop", &config)
        .unwrap()
        .unwrap();
    let browser = Mdns::start().unwrap();
    let events = browser.browse(&laptop_ann.repo, "laptop").unwrap();

    // The desktop announces itself.
    let desk_mdns = Mdns::start().unwrap();
    let first = Announcement::for_repo(&desktop, "desktop", &config)
        .unwrap()
        .unwrap();
    assert_eq!(first.repo, laptop_ann.repo, "clones share the repo id");
    assert!(desk_mdns.announce(&first).unwrap());
    assert!(
        !desk_mdns.announce(&first).unwrap(),
        "same announcement twice is a no-op"
    );

    let seen = found(wait_for(&events, |e| matches!(e, PeerEvent::Found(_))));
    assert_eq!(seen.peer.name, "desktop");
    assert_eq!(seen.gen, first.gen);
    assert!(
        seen.peer.url.ends_with(&first.path),
        "{} should end with {}",
        seen.peer.url,
        first.path
    );
    assert_eq!(seen.peer.host_key_alias.as_deref(), Some("gitsync-desktop"));

    // News on the desktop: a new snapshot changes its gen.
    write(desk_dir.path(), "a.txt", "changed\n");
    snapshot(&desktop, "desktop").unwrap();
    let second = Announcement::for_repo(&desktop, "desktop", &config)
        .unwrap()
        .unwrap();
    assert_ne!(second.gen, first.gen);
    desk_mdns.announce(&second).unwrap();
    let news = found(wait_for(
        &events,
        |e| matches!(e, PeerEvent::Found(f) if f.gen != first.gen),
    ));
    assert_eq!(news.gen, second.gen);

    // The desktop stops: it says goodbye.
    drop(desk_mdns);
    let gone = wait_for(&events, |e| matches!(e, PeerEvent::Lost(_)));
    assert_eq!(gone, PeerEvent::Lost("desktop".into()));
}

#[test]
fn other_projects_are_not_reported() {
    let (_a_dir, a) = repo();
    // A separate repo with a different first commit.
    let (b_dir, b) = repo();
    write(b_dir.path(), "a.txt", "other project\n");
    b.run(&["commit", "-q", "--amend", "-am", "different root"])
        .unwrap();
    let config = Config::default();
    let a_ann = Announcement::for_repo(&a, "a-machine", &config)
        .unwrap()
        .unwrap();
    let b_ann = Announcement::for_repo(&b, "b-machine", &config)
        .unwrap()
        .unwrap();
    assert_ne!(a_ann.repo, b_ann.repo);

    let browser = Mdns::start().unwrap();
    let events = browser.browse(&a_ann.repo, "a-machine").unwrap();
    let b_mdns = Mdns::start().unwrap();
    b_mdns.announce(&b_ann).unwrap();

    std::thread::sleep(Duration::from_secs(3));
    let reported: Vec<_> = events.try_iter().collect();
    assert!(reported.is_empty(), "{reported:?}");
}
