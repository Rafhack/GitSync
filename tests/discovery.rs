//! Real mDNS between two repos on this machine (multicast loops back to the
//! sender, so one process can play both sides). Needs a network interface
//! with multicast, which any LAN-connected machine has.

mod common;

use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use common::{clone_of, repo, write};
use gitsync::config::Config;
use gitsync::discovery::{Announcement, Found, Mdns, PeerEvent, Reach};
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
    let (ssh, port) = Reach::ssh(&desktop, &config).unwrap();
    let Reach::Ssh { path, .. } = &ssh else {
        unreachable!()
    };

    let laptop_ann = Announcement::for_repo(&laptop, "laptop", &ssh, port)
        .unwrap()
        .unwrap();
    let browser = Mdns::start().unwrap();
    let events = browser.browse(&laptop_ann.repo, "laptop").unwrap();

    // The desktop announces itself.
    let desk_mdns = Mdns::start().unwrap();
    let first = Announcement::for_repo(&desktop, "desktop", &ssh, port)
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
        seen.peer.url.ends_with(path.as_str()),
        "{} should end with {path}",
        seen.peer.url,
    );
    assert_eq!(seen.peer.host_key_alias.as_deref(), Some("gitsync-desktop"));

    // News on the desktop: a new snapshot changes its gen.
    write(desk_dir.path(), "a.txt", "changed\n");
    snapshot(&desktop, "desktop").unwrap();
    let second = Announcement::for_repo(&desktop, "desktop", &ssh, port)
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
    let reach = Reach::Gitsync { key: "K".into() };
    let a_ann = Announcement::for_repo(&a, "a-machine", &reach, 7431)
        .unwrap()
        .unwrap();
    let b_ann = Announcement::for_repo(&b, "b-machine", &reach, 7431)
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

#[test]
fn gitsync_transport_peers_announce_port_and_key() {
    let (laptop_dir, laptop) = repo();
    let (_desk_dir, desktop) = clone_of(laptop_dir.path());
    let key = gitsync::identity::fingerprint(b"desktop key");
    let reach = Reach::Gitsync { key: key.clone() };
    let ours = Announcement::for_repo(&laptop, "laptop", &reach, 1)
        .unwrap()
        .unwrap();
    let browser = Mdns::start().unwrap();
    let events = browser.browse(&ours.repo, "laptop").unwrap();

    let desk_mdns = Mdns::start().unwrap();
    let theirs = Announcement::for_repo(&desktop, "gs-desktop", &reach, 41234)
        .unwrap()
        .unwrap();
    desk_mdns.announce(&theirs).unwrap();

    let seen = found(wait_for(
        &events,
        |e| matches!(e, PeerEvent::Found(f) if f.peer.name == "gs-desktop"),
    ));
    assert!(seen.peer.url.starts_with("gitsync://"), "{}", seen.peer.url);
    assert!(seen.peer.url.ends_with(":41234"), "{}", seen.peer.url);
    assert_eq!(seen.key_hint, Some(key));
}
