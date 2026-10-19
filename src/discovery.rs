//! Finding peers on the LAN with mDNS / DNS-SD (DESIGN.md §4).
//!
//! Every `gitsync watch` announces a `_gitsync._tcp.local.` service per repo,
//! with TXT records saying who it is, which project, how to reach it over
//! ssh, and a `gen` fingerprint that changes whenever it has something new.
//! Peers browse for that service and fetch as soon as a `gen` changes.
//!
//! Announcements are untrusted input: anyone on the LAN can send them. They
//! are only used to *find* a peer; the data itself always comes over ssh, and
//! ssh checks the peer's host key under a fixed name (`gitsync-<peer>`), so a
//! fake announcement can at most cause a failed connection.

use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::net::IpAddr;
use std::sync::{mpsc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use tracing::{debug, warn};

use crate::config::{Config, Peer};
use crate::git::Git;
use crate::snapshot::validate_machine_id;

pub const SERVICE_TYPE: &str = "_gitsync._tcp.local.";

/// What this machine announces for one repo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Announcement {
    pub machine: String,
    /// The repo's first commit: only peers with the same project match.
    pub repo: String,
    pub user: String,
    pub path: String,
    pub port: u16,
    /// Changes whenever our branches or snapshot change.
    pub gen: String,
}

impl Announcement {
    /// What to announce for this repo right now. `None` before the first
    /// commit, since there is no repo id to match peers on yet.
    pub fn for_repo(git: &Git, machine_id: &str, config: &Config) -> Result<Option<Self>> {
        let Some(repo) = repo_id(git)? else {
            return Ok(None);
        };
        let user = match &config.advertise.user {
            Some(user) => user.clone(),
            None => current_user()?,
        };
        let path = match &config.advertise.path {
            Some(path) => path.clone(),
            None => ssh_path(&git.run(&["rev-parse", "--show-toplevel"])?),
        };
        validate_user(&user).context(
            "peers could not use this machine's ssh user; set [advertise] user in .git/gitsync.toml",
        )?;
        validate_path(&path).context(
            "peers could not use this repo's path; set [advertise] path in .git/gitsync.toml",
        )?;
        Ok(Some(Self {
            machine: machine_id.to_string(),
            repo,
            user,
            path,
            port: config.advertise.port.unwrap_or(22),
            gen: gen(git, machine_id)?,
        }))
    }

    fn instance_name(&self) -> String {
        format!("{}-{}", self.machine, &self.repo[..self.repo.len().min(12)])
    }

    fn txt(&self) -> HashMap<String, String> {
        HashMap::from([
            ("machine".into(), self.machine.clone()),
            ("repo".into(), self.repo.clone()),
            ("user".into(), self.user.clone()),
            ("path".into(), self.path.clone()),
            ("gen".into(), self.gen.clone()),
        ])
    }
}

/// A peer for this repo, found on the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub peer: Peer,
    pub gen: String,
    /// The mDNS instance name, to recognize its removal.
    pub fullname: String,
}

/// Why an announcement was not taken as a peer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Skip {
    /// Another project, or this very machine: not interesting.
    NotOurs,
    /// Malformed or suspicious; the reason is logged.
    Invalid(String),
    /// Fine, but no usable address resolved yet (more records follow).
    NoAddressYet,
}

/// Turns a peer's announcement into a [`Peer`] we can fetch from.
pub fn parse(
    txt: &HashMap<String, String>,
    addresses: &[IpAddr],
    port: u16,
    fullname: &str,
    our_repo: &str,
    our_machine: &str,
) -> Result<Found, Skip> {
    let field = |key: &str| {
        txt.get(key)
            .map(String::as_str)
            .ok_or_else(|| Skip::Invalid(format!("no `{key}` in announcement")))
    };
    let (machine, repo, user, path, gen) = (
        field("machine")?,
        field("repo")?,
        field("user")?,
        field("path")?,
        field("gen")?,
    );
    if repo != our_repo || machine == our_machine {
        return Err(Skip::NotOurs);
    }
    validate_machine_id(machine).map_err(|e| Skip::Invalid(format!("{e:#}")))?;
    validate_user(user).map_err(|e| Skip::Invalid(format!("{e:#}")))?;
    validate_path(path).map_err(|e| Skip::Invalid(format!("{e:#}")))?;
    if gen.len() > 64 {
        return Err(Skip::Invalid("`gen` too long".into()));
    }
    let Some(ip) = best_address(addresses) else {
        return Err(Skip::NoAddressYet);
    };
    let host = match ip {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => format!("[{v6}]"),
    };
    let port = if port == 22 {
        String::new()
    } else {
        format!(":{port}")
    };
    Ok(Found {
        peer: Peer {
            name: machine.to_string(),
            url: format!("ssh://{user}@{host}{port}{path}"),
            host_key_alias: Some(host_key_alias(machine)),
        },
        gen: gen.to_string(),
        fullname: fullname.to_string(),
    })
}

/// The name ssh files a discovered peer's host key under.
pub fn host_key_alias(peer: &str) -> String {
    format!("gitsync-{peer}")
}

/// An ssh user name that is safe to put in a URL: no `@`, `/`, spaces, and
/// nothing ssh could read as an option.
pub fn validate_user(user: &str) -> Result<()> {
    let ok = (1..=64).contains(&user.len())
        && !user.starts_with('-')
        && user
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if !ok {
        bail!("{user:?} is not a usable ssh user name");
    }
    Ok(())
}

/// An absolute repo path for an ssh URL, without control characters.
pub fn validate_path(path: &str) -> Result<()> {
    if !path.starts_with('/') || path.len() > 1024 || path.chars().any(char::is_control) {
        bail!("{path:?} is not a usable repo path (must start with /)");
    }
    Ok(())
}

/// Prefers an IPv4 address, then a global IPv6 one; never loopback or
/// link-local (those need extra scope information to be usable).
fn best_address(addresses: &[IpAddr]) -> Option<IpAddr> {
    let mut v4: Vec<_> = addresses
        .iter()
        .filter_map(|ip| match ip {
            IpAddr::V4(v4) if !v4.is_loopback() && !v4.is_link_local() => Some(*ip),
            _ => None,
        })
        .collect();
    v4.sort();
    let mut v6: Vec<_> = addresses
        .iter()
        .filter_map(|ip| match ip {
            IpAddr::V6(v6) if !v6.is_loopback() && (v6.segments()[0] & 0xffc0) != 0xfe80 => {
                Some(*ip)
            }
            _ => None,
        })
        .collect();
    v6.sort();
    v4.into_iter().chain(v6).next()
}

/// The repo's root commit (the first, if several), or `None` before the
/// first commit.
fn repo_id(git: &Git) -> Result<Option<String>> {
    if git
        .try_run(&["rev-parse", "--verify", "-q", "HEAD"])?
        .is_none()
    {
        return Ok(None);
    }
    let roots = git.run(&["rev-list", "--max-parents=0", "HEAD"])?;
    let mut roots: Vec<&str> = roots.lines().collect();
    roots.sort();
    Ok(roots.first().map(|r| r.to_string()))
}

/// A fingerprint of everything a peer would fetch from us: our branches and
/// our snapshot. Equal state gives an equal value, so restarting gitsync
/// does not make peers fetch for nothing.
pub fn gen(git: &Git, machine_id: &str) -> Result<String> {
    let refs = git.run(&[
        "for-each-ref",
        "--format=%(objectname) %(refname)",
        "refs/heads/",
        &format!("refs/gitsync/{machine_id}/"),
    ])?;
    let mut hasher = DefaultHasher::new();
    refs.hash(&mut hasher);
    Ok(format!("{:016x}", hasher.finish()))
}

/// The repo folder as it appears in an ssh URL. On Windows, gitsync's peers
/// reach it through Git Bash (the ssh server's shell, DESIGN.md §5), which
/// spells `C:/Users/me` as `/c/Users/me`.
pub fn ssh_path(toplevel: &str) -> String {
    let path = toplevel.replace('\\', "/");
    let bytes = path.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        format!("/{}{}", (bytes[0] as char).to_ascii_lowercase(), &path[2..])
    } else {
        path
    }
}

/// The login name peers should use. On Windows, `USERNAME` is the account
/// name (ssh sessions also set `USER`, as `domain\name`); the domain part is
/// dropped and the name lowercased (Windows names are case-insensitive).
fn current_user() -> Result<String> {
    let vars: &[&str] = if cfg!(windows) {
        &["USERNAME", "USER"]
    } else {
        &["USER", "LOGNAME"]
    };
    let user = vars
        .iter()
        .find_map(|var| std::env::var(var).ok().filter(|v| !v.is_empty()))
        .context("cannot tell the current user name; set [advertise] user in the config")?;
    Ok(if cfg!(windows) {
        windows_login(&user)
    } else {
        user
    })
}

/// `AzureAD\Name` or `DOMAIN\Name` → `name`.
fn windows_login(user: &str) -> String {
    user.rsplit('\\').next().unwrap_or(user).to_lowercase()
}

/// A peer appearing, changing, or leaving the network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerEvent {
    Found(Found),
    Lost(String),
}

/// The mDNS side of gitsync: announces this machine and finds peers.
pub struct Mdns {
    daemon: ServiceDaemon,
    hostname: String,
    current: Mutex<Option<(String, Announcement)>>,
}

impl Mdns {
    pub fn start() -> Result<Self> {
        let daemon = ServiceDaemon::new().context("starting mDNS")?;
        let host = gethostname::gethostname().to_string_lossy().into_owned();
        let host: String = host
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        Ok(Self {
            daemon,
            hostname: format!("{}.local.", host.trim_matches('-')),
            current: Mutex::new(None),
        })
    }

    /// Announces `announcement`, unless that exact one is already out.
    /// Returns true if something new was announced.
    pub fn announce(&self, announcement: &Announcement) -> Result<bool> {
        let mut current = self.current.lock().unwrap_or_else(|p| p.into_inner());
        if current.as_ref().map(|(_, a)| a) == Some(announcement) {
            return Ok(false);
        }
        let info = ServiceInfo::new(
            SERVICE_TYPE,
            &announcement.instance_name(),
            &self.hostname,
            "",
            announcement.port,
            announcement.txt(),
        )
        .context("building mDNS announcement")?
        .enable_addr_auto();
        let fullname = info.get_fullname().to_string();
        // A different instance name (machine or repo changed): retract the old one.
        if let Some((old, _)) = current.as_ref().filter(|(old, _)| *old != fullname) {
            let _ = self.daemon.unregister(old);
        }
        self.daemon.register(info).context("announcing over mDNS")?;
        *current = Some((fullname, announcement.clone()));
        Ok(true)
    }

    /// Starts browsing; events for peers of `our_repo` arrive on the
    /// returned channel until this `Mdns` is dropped.
    pub fn browse(&self, our_repo: &str, our_machine: &str) -> Result<mpsc::Receiver<PeerEvent>> {
        let events = self.daemon.browse(SERVICE_TYPE).context("browsing mDNS")?;
        let (tx, rx) = mpsc::channel();
        let mut browser = Browser {
            our_repo: our_repo.to_string(),
            our_machine: our_machine.to_string(),
            names: HashMap::new(),
            last_v4: HashMap::new(),
            pending: HashMap::new(),
            warned: HashSet::new(),
        };
        thread::spawn(move || loop {
            let wait = browser
                .pending
                .values()
                .map(|(_, due)| due.saturating_duration_since(Instant::now()))
                .min()
                .unwrap_or(Duration::from_secs(3600));
            let out = match events.recv_timeout(wait) {
                Ok(event) => browser.handle(event),
                Err(mdns_sd::RecvTimeoutError::Timeout) => browser.flush_pending(),
                Err(_) => break,
            };
            for event in out {
                if tx.send(event).is_err() {
                    return;
                }
            }
        });
        Ok(rx)
    }
}

/// How long to hold back a peer heard with only an IPv6 address: the IPv4
/// address usually arrives in the next packet, and IPv4 is preferred
/// (IPv6 global addresses are often temporary "privacy" addresses).
const IPV4_GRACE: Duration = Duration::from_millis(1500);

/// Turns raw mDNS events into [`PeerEvent`]s for one repo.
struct Browser {
    our_repo: String,
    our_machine: String,
    /// fullname → peer name, to report which peer left.
    names: HashMap<String, String>,
    /// fullname → last IPv4 address seen. Some updates carry only part of the
    /// address list; keeping the IPv4 address stops the URL from flipping
    /// between IPv4 and IPv6.
    last_v4: HashMap<String, IpAddr>,
    /// Peers heard with only IPv6 so far, released at the deadline.
    pending: HashMap<String, (Found, Instant)>,
    /// Bad announcements already warned about, to warn once.
    warned: HashSet<String>,
}

impl Browser {
    fn handle(&mut self, event: ServiceEvent) -> Vec<PeerEvent> {
        match event {
            ServiceEvent::ServiceResolved(info) => {
                let txt = info
                    .txt_properties
                    .iter()
                    .map(|p| (p.key().to_string(), p.val_str().to_string()))
                    .collect();
                let mut addresses: Vec<IpAddr> =
                    info.addresses.iter().map(|a| a.to_ip_addr()).collect();
                let usable_v4 = |ip: &IpAddr| matches!(ip, IpAddr::V4(v4) if !v4.is_loopback() && !v4.is_link_local());
                match addresses.iter().find(|ip| usable_v4(ip)) {
                    Some(v4) => {
                        self.last_v4.insert(info.fullname.clone(), *v4);
                    }
                    None => addresses.extend(self.last_v4.get(&info.fullname)),
                }
                let has_v4 = addresses.iter().any(usable_v4);
                let parsed = parse(
                    &txt,
                    &addresses,
                    info.port,
                    &info.fullname,
                    &self.our_repo,
                    &self.our_machine,
                );
                match parsed {
                    Ok(found) if has_v4 => {
                        self.pending.remove(&found.fullname);
                        self.names
                            .insert(found.fullname.clone(), found.peer.name.clone());
                        vec![PeerEvent::Found(found)]
                    }
                    Ok(found) => {
                        // Only IPv6 so far: wait a moment for the IPv4 address.
                        let due = self
                            .pending
                            .get(&found.fullname)
                            .map(|(_, due)| *due)
                            .unwrap_or_else(|| Instant::now() + IPV4_GRACE);
                        self.pending.insert(found.fullname.clone(), (found, due));
                        vec![]
                    }
                    Err(Skip::NotOurs | Skip::NoAddressYet) => vec![],
                    Err(Skip::Invalid(reason)) => {
                        if self.warned.insert(format!("{}\0{reason}", info.fullname)) {
                            warn!("ignoring mDNS announcement {}: {reason}", info.fullname);
                        }
                        vec![]
                    }
                }
            }
            ServiceEvent::ServiceRemoved(_, fullname) => {
                self.last_v4.remove(&fullname);
                self.pending.remove(&fullname);
                match self.names.remove(&fullname) {
                    Some(name) => vec![PeerEvent::Lost(name)],
                    None => vec![],
                }
            }
            other => {
                debug!("mDNS: {other:?}");
                vec![]
            }
        }
    }

    /// Releases peers whose IPv4 grace time is over, with what we have.
    fn flush_pending(&mut self) -> Vec<PeerEvent> {
        let now = Instant::now();
        let due: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, (_, due))| *due <= now)
            .map(|(fullname, _)| fullname.clone())
            .collect();
        due.into_iter()
            .filter_map(|fullname| self.pending.remove(&fullname))
            .map(|(found, _)| {
                self.names
                    .insert(found.fullname.clone(), found.peer.name.clone());
                PeerEvent::Found(found)
            })
            .collect()
    }
}

impl Mdns {
    /// Withdraws our announcement, so peers forget us now rather than when
    /// the record expires. Safe to call more than once.
    pub fn goodbye(&self) {
        let current = self
            .current
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        if let Some((fullname, _)) = current {
            if let Ok(done) = self.daemon.unregister(&fullname) {
                let _ = done.recv_timeout(std::time::Duration::from_secs(1));
            }
        }
    }
}

impl Drop for Mdns {
    fn drop(&mut self) {
        self.goodbye();
        let _ = self.daemon.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn txt(machine: &str) -> HashMap<String, String> {
        HashMap::from([
            ("machine".into(), machine.into()),
            ("repo".into(), "abc".into()),
            ("user".into(), "rafael".into()),
            ("path".into(), "/c/Users/rafael/My Repos/demo".into()),
            ("gen".into(), "1".into()),
        ])
    }

    fn ips(list: &[&str]) -> Vec<IpAddr> {
        list.iter().map(|s| s.parse().unwrap()).collect()
    }

    #[test]
    fn builds_ssh_url_from_announcement() {
        let found = parse(
            &txt("windows"),
            &ips(&["192.168.15.228"]),
            22,
            "f",
            "abc",
            "laptop",
        )
        .unwrap();
        assert_eq!(found.peer.name, "windows");
        assert_eq!(
            found.peer.url,
            "ssh://rafael@192.168.15.228/c/Users/rafael/My Repos/demo"
        );
        assert_eq!(
            found.peer.host_key_alias.as_deref(),
            Some("gitsync-windows")
        );

        let v6 = parse(
            &txt("w"),
            &ips(&["fe80::1", "2001:db8::5"]),
            2222,
            "f",
            "abc",
            "me",
        )
        .unwrap();
        assert_eq!(
            v6.peer.url,
            "ssh://rafael@[2001:db8::5]:2222/c/Users/rafael/My Repos/demo"
        );
    }

    #[test]
    fn prefers_ipv4_and_skips_unusable_addresses() {
        let best = best_address(&ips(&[
            "2001:db8::5",
            "127.0.0.1",
            "169.254.1.1",
            "10.0.0.9",
        ]));
        assert_eq!(best, Some("10.0.0.9".parse().unwrap()));
        assert_eq!(best_address(&ips(&["fe80::1", "::1"])), None);
    }

    #[test]
    fn ignores_other_repos_and_ourselves() {
        let ip = ips(&["10.0.0.9"]);
        assert_eq!(
            parse(&txt("windows"), &ip, 22, "f", "other", "laptop"),
            Err(Skip::NotOurs)
        );
        assert_eq!(
            parse(&txt("laptop"), &ip, 22, "f", "abc", "laptop"),
            Err(Skip::NotOurs)
        );
    }

    #[test]
    fn rejects_malicious_announcements() {
        let ip = ips(&["10.0.0.9"]);
        let with = |key: &str, val: &str| {
            let mut t = txt("windows");
            t.insert(key.into(), val.into());
            parse(&t, &ip, 22, "f", "abc", "laptop")
        };
        for (key, val) in [
            ("machine", "../../heads/main"),
            ("machine", "a b"),
            ("user", "-oProxyCommand=evil"),
            ("user", "me@evil"),
            ("path", "relative/path"),
            ("path", "/tmp/x\nrm"),
        ] {
            assert!(
                matches!(with(key, val), Err(Skip::Invalid(_))),
                "accepted {key}={val:?}"
            );
        }
        let mut no_gen = txt("windows");
        no_gen.remove("gen");
        assert!(matches!(
            parse(&no_gen, &ip, 22, "f", "abc", "laptop"),
            Err(Skip::Invalid(_))
        ));
    }

    #[test]
    fn windows_logins_lose_the_domain() {
        assert_eq!(
            windows_login(r"AzureAD\RafaelCaetanodaSilva"),
            "rafaelcaetanodasilva"
        );
        assert_eq!(windows_login("Rafael"), "rafael");
    }

    #[test]
    fn windows_paths_become_git_bash_paths() {
        assert_eq!(ssh_path("C:/Users/Me/repos/Demo"), "/c/Users/Me/repos/Demo");
        assert_eq!(ssh_path(r"D:\work\x"), "/d/work/x");
        assert_eq!(ssh_path("/home/me/demo"), "/home/me/demo");
    }
}
