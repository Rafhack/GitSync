//! Fetches a peer's branches and snapshots over plain `git fetch` (usually
//! ssh). See DESIGN.md §5.
//!
//! Pull-only: we never write to the peer. Its branches land in a private
//! namespace and never touch our own branches:
//!
//! ```text
//! peer refs/heads/*            → refs/gitsync-peers/<peer>/heads/*
//! peer refs/gitsync/<peer>/*   → refs/gitsync/<peer>/*
//! ```

use std::collections::BTreeMap;

use anyhow::{Context, Result};

use crate::config::Peer;
use crate::git::Git;

/// Where a peer's branches are stored locally.
pub fn peer_heads_prefix(peer: &str) -> String {
    format!("refs/gitsync-peers/{peer}/heads/")
}

/// One ref that changed during a fetch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefChange {
    pub name: String,
    /// `None` if the ref is new.
    pub old: Option<String>,
    /// `None` if the ref was deleted (the peer deleted it).
    pub new: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchReport {
    pub peer: String,
    pub changes: Vec<RefChange>,
    /// False if the peer has no snapshot under `refs/gitsync/<peer>/`, which
    /// usually means its `machine_id` differs from the name we gave it.
    pub has_snapshot: bool,
}

/// Fetches one peer. Fails if the peer is unreachable or ssh needs a password.
pub fn fetch_peer(git: &Git, peer: &Peer) -> Result<FetchReport> {
    let heads = peer_heads_prefix(&peer.name);
    let snapshots = format!("refs/gitsync/{}/", peer.name);
    let before = list_refs(git, &[&heads, &snapshots])?;

    let heads_spec = format!("+refs/heads/*:{heads}*");
    let snapshots_spec = format!("+{snapshots}*:{snapshots}*");
    let ssh = ssh_command(git)?;
    let mut envs = vec![("GIT_TERMINAL_PROMPT", "0")];
    if let Some(ssh) = &ssh {
        envs.push(("GIT_SSH_COMMAND", ssh.as_str()));
    }
    git.run_env(
        &[
            "fetch",
            "--quiet",
            "--no-tags",
            "--prune",
            // FETCH_HEAD belongs to the user's own `git fetch`/`git pull`.
            "--no-write-fetch-head",
            &peer.url,
            &heads_spec,
            &snapshots_spec,
        ],
        &envs,
    )
    .map_err(|err| match ssh_hint(&format!("{err:#}"), &peer.url) {
        Some(hint) => err.context(hint),
        None => err,
    })
    .with_context(|| format!("fetching from peer {:?} ({})", peer.name, peer.url))?;

    let after = list_refs(git, &[&heads, &snapshots])?;
    let has_snapshot = after.keys().any(|name| name.starts_with(&snapshots));
    Ok(FetchReport {
        peer: peer.name.clone(),
        changes: diff_refs(&before, &after),
        has_snapshot,
    })
}

/// ssh must never stop to ask for a password or a host key: gitsync runs
/// unattended. Only used when the user has not set up their own ssh command.
fn ssh_command(git: &Git) -> Result<Option<String>> {
    if std::env::var_os("GIT_SSH_COMMAND").is_some() || std::env::var_os("GIT_SSH").is_some() {
        return Ok(None);
    }
    if git.try_run(&["config", "core.sshCommand"])?.is_some() {
        return Ok(None);
    }
    Ok(Some(
        "ssh -o BatchMode=yes -o ConnectTimeout=10 -o ServerAliveInterval=10 -o ServerAliveCountMax=3"
            .to_string(),
    ))
}

/// Advice for the ssh failures every new setup runs into, since gitsync
/// cannot answer ssh's questions itself.
fn ssh_hint(error: &str, url: &str) -> Option<String> {
    let host = url
        .strip_prefix("ssh://")
        .and_then(|rest| rest.split('/').next())
        .unwrap_or("the peer");
    if error.contains("Host key verification failed") {
        Some(format!(
            "ssh does not know this peer's host key yet; connect once by hand \
             with `ssh {host}` and accept it"
        ))
    } else if error.contains("Permission denied") {
        Some(format!(
            "ssh needs key login without a password; add your public key to \
             ~/.ssh/authorized_keys on {host} (or load it into ssh-agent)"
        ))
    } else {
        None
    }
}

fn list_refs(git: &Git, prefixes: &[&str]) -> Result<BTreeMap<String, String>> {
    let mut args = vec!["for-each-ref", "--format=%(objectname) %(refname)"];
    args.extend(prefixes);
    let out = git.run(&args)?;
    Ok(out
        .lines()
        .filter_map(|line| line.split_once(' '))
        .map(|(oid, name)| (name.to_string(), oid.to_string()))
        .collect())
}

fn diff_refs(
    before: &BTreeMap<String, String>,
    after: &BTreeMap<String, String>,
) -> Vec<RefChange> {
    let mut names: Vec<&String> = before.keys().chain(after.keys()).collect();
    names.sort();
    names.dedup();
    names
        .into_iter()
        .filter(|name| before.get(*name) != after.get(*name))
        .map(|name| RefChange {
            name: name.clone(),
            old: before.get(name).cloned(),
            new: after.get(name).cloned(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::ssh_hint;

    #[test]
    fn hints_for_common_ssh_failures() {
        let url = "ssh://me@desktop.local/code/proj";
        let host_key = ssh_hint("Host key verification failed.", url).unwrap();
        assert!(host_key.contains("ssh me@desktop.local"), "{host_key}");
        let denied = ssh_hint("Permission denied (publickey).", url).unwrap();
        assert!(denied.contains("authorized_keys"), "{denied}");
        assert_eq!(ssh_hint("Connection refused", url), None);
    }
}
