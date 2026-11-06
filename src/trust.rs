//! The machines this one trusts on the gitsync transport (DESIGN.md §11):
//! `trusted_peers.toml` next to the identity, mapping each peer's machine id
//! to its key fingerprint. Filled by `gitsync pair` or `gitsync trust`.
//!
//! The file is read again on every use, so a pairing made while `watch`
//! runs takes effect at once.

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::identity::{config_dir, normalize};

const FILE_NAME: &str = "trusted_peers.toml";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Trusted {
    pub machine: String,
    pub key: String,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    #[serde(default, rename = "peer")]
    peers: Vec<Trusted>,
}

#[derive(Debug, Clone)]
pub struct TrustList {
    path: PathBuf,
    pub peers: Vec<Trusted>,
}

impl TrustList {
    /// The trust list in [`config_dir`] (empty if there is none yet).
    pub fn load() -> Result<Self> {
        Self::load_in(&config_dir()?)
    }

    pub fn load_in(dir: &Path) -> Result<Self> {
        let path = dir.join(FILE_NAME);
        let peers = match fs::read_to_string(&path) {
            Ok(text) => {
                toml::from_str::<File>(&text)
                    .with_context(|| format!("in {}", path.display()))?
                    .peers
            }
            Err(err) if err.kind() == ErrorKind::NotFound => Vec::new(),
            Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
        };
        Ok(Self { path, peers })
    }

    /// The key paired under `machine`.
    pub fn key_for(&self, machine: &str) -> Option<&str> {
        self.peers
            .iter()
            .find(|p| p.machine == machine)
            .map(|p| p.key.as_str())
    }

    /// The machine a key belongs to.
    pub fn machine_for(&self, key: &str) -> Option<&str> {
        self.peers
            .iter()
            .find(|p| p.key == key)
            .map(|p| p.machine.as_str())
    }

    /// Trusts `key` as `machine`, replacing whatever was trusted under that
    /// name or for that key before (a reinstalled or renamed machine).
    /// Returns the entries it replaced.
    pub fn add(&mut self, machine: &str, key: &str) -> Result<Vec<Trusted>> {
        let key = normalize(key)?;
        let (replaced, kept) = self
            .peers
            .drain(..)
            .partition(|p| p.machine == machine || p.key == key);
        self.peers = kept;
        self.peers.push(Trusted {
            machine: machine.to_string(),
            key,
        });
        self.peers.sort_by(|a, b| a.machine.cmp(&b.machine));
        Ok(replaced)
    }

    /// Stops trusting `machine`. Returns false if it was not trusted.
    pub fn remove(&mut self, machine: &str) -> bool {
        let before = self.peers.len();
        self.peers.retain(|p| p.machine != machine);
        self.peers.len() != before
    }

    /// Writes the list back (under a temporary name first, so a reader never
    /// sees half a file).
    pub fn save(&self) -> Result<()> {
        let dir = self.path.parent().expect("file in a folder");
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let text = toml::to_string(&File {
            peers: self.peers.clone(),
        })?;
        let tmp = dir.join(format!("{FILE_NAME}.{}.tmp", std::process::id()));
        fs::write(&tmp, text).with_context(|| format!("writing {}", tmp.display()))?;
        fs::rename(&tmp, &self.path).with_context(|| format!("writing {}", self.path.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::identity::fingerprint;

    #[test]
    fn add_replace_remove_and_reload() {
        let dir = tempfile::TempDir::new().unwrap();
        let (a, b) = (fingerprint(b"a"), fingerprint(b"b"));
        let mut list = TrustList::load_in(dir.path()).unwrap();
        assert!(list.peers.is_empty());

        assert!(list.add("desktop", &a).unwrap().is_empty());
        // Typed without dashes, in lowercase: stored normalized.
        list.add("pi", &b.replace('-', "").to_lowercase()).unwrap();
        list.save().unwrap();
        let list = TrustList::load_in(dir.path()).unwrap();
        assert_eq!(list.key_for("pi"), Some(b.as_str()));
        assert_eq!(list.machine_for(&a), Some("desktop"));

        // Re-pairing the same name replaces the old key; the same key under a
        // new name replaces the old name.
        let mut list = list;
        let c = fingerprint(b"c");
        assert_eq!(list.add("desktop", &c).unwrap()[0].key, a);
        assert_eq!(list.add("server", &c).unwrap()[0].machine, "desktop");
        assert_eq!(list.key_for("desktop"), None);

        assert!(list.remove("server"));
        assert!(!list.remove("server"));
        assert!(list.add("x", "not a key").is_err());
    }
}
