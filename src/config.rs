//! Per-repo settings in `.git/gitsync.toml`. See DESIGN.md §7.
//!
//! The file is optional: when it is missing every setting has a default.
//! Command line flags override the file.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde::Deserialize;

use crate::git::Git;
use crate::snapshot::{default_machine_id, validate_machine_id};

pub const FILE_NAME: &str = "gitsync.toml";

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Name of this machine in ref names. `None` means "use the hostname".
    pub machine_id: Option<String>,
    /// Snapshot at least this often, even without file events.
    pub poll_interval_secs: u64,
    /// Wait for this much quiet after a change before snapshotting.
    pub debounce_ms: u64,
    /// How peers are found.
    pub discovery: Discovery,
    /// Peers listed by hand (always used, whatever `discovery` says).
    pub peers: Vec<Peer>,
    /// How peers fetch from this machine (DESIGN.md §11).
    pub transport: Transport,
    /// The gitsync transport's listener.
    pub serve: Serve,
    /// How peers should reach this machine over ssh, announced over mDNS.
    pub advertise: Advertise,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    /// gitsync's own TLS connection between paired machines.
    #[default]
    Gitsync,
    /// `git fetch` over ssh.
    Ssh,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Serve {
    /// TCP port to listen on (default: any free port, announced over mDNS).
    pub port: Option<u16>,
}

/// Overrides for what this machine announces (DESIGN.md §4). Each field
/// defaults to something sensible, so the whole table is optional.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Advertise {
    /// ssh user peers log in as (default: the current user).
    pub user: Option<String>,
    /// Repo path in the ssh URL (default: the repo's folder; on Windows in
    /// Git Bash form, `/c/Users/...`).
    pub path: Option<String>,
    /// ssh port (default: 22).
    pub port: Option<u16>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            machine_id: None,
            poll_interval_secs: 30,
            debounce_ms: 1000,
            discovery: Discovery::default(),
            peers: Vec::new(),
            transport: Transport::default(),
            serve: Serve::default(),
            advertise: Advertise::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Discovery {
    /// Find peers on the LAN automatically (DESIGN.md §4).
    #[default]
    Mdns,
    /// Only use the `[[peers]]` listed in the file.
    Static,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Peer {
    /// The peer's machine id (used in ref names, like `machine_id`).
    pub name: String,
    /// Where to fetch from, e.g. `ssh://rafael@192.168.1.20/home/rafael/code/myproj`.
    pub url: String,
    /// For peers found over mDNS: ssh checks the host key under this name
    /// instead of the IP, so a new IP needs no new trust and another host
    /// cannot pose as this peer. Never set from the config file.
    #[serde(skip)]
    pub host_key_alias: Option<String>,
}

impl Peer {
    /// A peer listed by hand.
    pub fn new(name: impl Into<String>, url: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            url: url.into(),
            host_key_alias: None,
        }
    }
}

impl Config {
    /// Where the config file lives for this repo.
    pub fn path(git: &Git) -> Result<PathBuf> {
        Ok(git.git_dir()?.join(FILE_NAME))
    }

    /// Reads the repo's config file, or returns the defaults if there is none.
    pub fn load(git: &Git) -> Result<Self> {
        Ok(Self::load_file(&Self::path(git)?)?.unwrap_or_default())
    }

    /// Reads a config file. `Ok(None)` if the file does not exist.
    pub fn load_file(path: &Path) -> Result<Option<Self>> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == ErrorKind::NotFound => return Ok(None),
            Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
        };
        let config = Self::parse(&text).with_context(|| format!("in {}", path.display()))?;
        Ok(Some(config))
    }

    /// Parses and checks the contents of a config file.
    pub fn parse(text: &str) -> Result<Self> {
        let config: Self = toml::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    /// The machine id to use: `machine_id` from the file, else the hostname.
    pub fn machine_id_or_default(&self) -> Result<String> {
        match &self.machine_id {
            Some(id) => Ok(id.clone()),
            None => default_machine_id(),
        }
    }

    /// Checks the settings make sense (also after command line overrides).
    pub fn validate(&self) -> Result<()> {
        if let Some(id) = &self.machine_id {
            validate_machine_id(id)?;
        }
        if let Some(user) = &self.advertise.user {
            crate::discovery::validate_user(user).context("[advertise] user")?;
        }
        if let Some(path) = &self.advertise.path {
            crate::discovery::validate_path(path).context("[advertise] path")?;
        }
        if self.poll_interval_secs == 0 {
            bail!("poll_interval_secs must be at least 1");
        }
        for (i, peer) in self.peers.iter().enumerate() {
            validate_machine_id(&peer.name)
                .with_context(|| format!("peer name {:?}", peer.name))?;
            if peer.url.trim().is_empty() {
                bail!("peer {:?} has an empty url", peer.name);
            }
            if peer.url.starts_with("gitsync:") {
                gitsync_address(&peer.url).with_context(|| format!("peer {:?}", peer.name))?;
            }
            if Some(&peer.name) == self.machine_id.as_ref() {
                bail!("peer {:?} has the same name as this machine", peer.name);
            }
            if self.peers[..i].iter().any(|other| other.name == peer.name) {
                bail!("peer {:?} is listed twice", peer.name);
            }
        }
        Ok(())
    }
}

/// `host:port` from a `gitsync://host:port` URL. The host is a name, an
/// IPv4 address or a bracketed IPv6 address; the port is required.
pub fn gitsync_address(url: &str) -> Result<&str> {
    let Some(addr) = url.strip_prefix("gitsync://") else {
        bail!("{url:?} is not a gitsync:// URL");
    };
    let (host, port) = match addr.rsplit_once(':') {
        Some((host, port)) if !host.is_empty() => (host, port),
        _ => bail!("{url:?} needs a port, like gitsync://192.168.1.30:7431"),
    };
    let host_ok = match host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        Some(v6) => v6.parse::<std::net::Ipv6Addr>().is_ok(),
        None => host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-')),
    };
    if !host_ok || !matches!(port.parse::<u16>(), Ok(p) if p > 0) {
        bail!("{url:?} is not a usable gitsync:// URL (gitsync://host:port)");
    }
    Ok(addr)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_file_gives_defaults() {
        assert_eq!(Config::parse("").unwrap(), Config::default());
        assert_eq!(Config::default().transport, Transport::Gitsync);
    }

    #[test]
    fn gitsync_urls() {
        assert_eq!(
            gitsync_address("gitsync://10.0.0.9:7431").unwrap(),
            "10.0.0.9:7431"
        );
        assert_eq!(
            gitsync_address("gitsync://[2001:db8::5]:7431").unwrap(),
            "[2001:db8::5]:7431"
        );
        assert_eq!(
            gitsync_address("gitsync://pi.local:1").unwrap(),
            "pi.local:1"
        );
        for bad in [
            "gitsync://10.0.0.9",
            "gitsync://:7431",
            "gitsync://10.0.0.9:0",
            "gitsync://a b:7431",
            "gitsync://x%y:7431",
            "gitsync://[nope]:7431",
            "ssh://x",
        ] {
            assert!(gitsync_address(bad).is_err(), "accepted {bad}");
        }
    }

    #[test]
    fn parses_the_design_doc_example() {
        let config = Config::parse(
            r#"
            machine_id = "laptop"
            poll_interval_secs = 10
            debounce_ms = 500
            discovery = "static"
            transport = "ssh"
            [serve]
            port = 7431
            [[peers]]
            name = "desktop"
            url = "ssh://rafael@192.168.1.20/home/rafael/code/myproj"
            [[peers]]
            name = "server"
            url = "gitsync://192.168.1.30:7431"
            "#,
        )
        .unwrap();
        assert_eq!(config.transport, Transport::Ssh);
        assert_eq!(config.serve.port, Some(7431));
        assert_eq!(config.peers[1].url, "gitsync://192.168.1.30:7431");
        assert_eq!(config.machine_id.as_deref(), Some("laptop"));
        assert_eq!(config.poll_interval_secs, 10);
        assert_eq!(config.debounce_ms, 500);
        assert_eq!(config.discovery, Discovery::Static);
        assert_eq!(config.peers[0].name, "desktop");
    }

    #[test]
    fn parses_advertise_overrides() {
        let config = Config::parse(
            "[advertise]\nuser = \"rafael\"\npath = \"/c/Users/rafael/My Repos/x\"\nport = 2222\n",
        )
        .unwrap();
        assert_eq!(config.advertise.user.as_deref(), Some("rafael"));
        assert_eq!(config.advertise.port, Some(2222));
        for bad in [
            "[advertise]\nuser = \"-oProxyCommand=x\"",
            "[advertise]\npath = \"relative\"",
            "[advertise]\nhost = \"x\"",
        ] {
            assert!(Config::parse(bad).is_err(), "accepted: {bad}");
        }
    }

    #[test]
    fn rejects_mistakes() {
        for bad in [
            "poll_intervall_secs = 5",   // typo in a key
            "poll_interval_secs = 0",    // would spin
            "discovery = \"bluetooth\"", // unknown discovery
            "transport = \"carrier-pigeon\"",
            "[serve]\nport = 70000",
            "[[peers]]\nname = \"x\"\nurl = \"gitsync://10.0.0.9\"",
            "machine_id = \"a/b\"", // not one ref component
            "[[peers]]\nname = \"x y\"\nurl = \"u\"",
            "[[peers]]\nname = \"x\"\nurl = \"\"",
            "[[peers]]\nname = \"x\"\nurl = \"u\"\n[[peers]]\nname = \"x\"\nurl = \"v\"",
            "machine_id = \"x\"\n[[peers]]\nname = \"x\"\nurl = \"u\"",
        ] {
            assert!(Config::parse(bad).is_err(), "accepted: {bad}");
        }
    }
}
