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

/// Peers fetched from before (they have a `refs/gitsync-peers/<peer>/`
/// namespace), including ones found on the LAN rather than configured.
pub fn fetched_peer_names(git: &Git) -> Result<Vec<String>> {
    let refs = git.run(&["for-each-ref", "--format=%(refname)", "refs/gitsync-peers/"])?;
    let mut names: Vec<String> = refs
        .lines()
        .filter_map(|r| r.strip_prefix("refs/gitsync-peers/")?.split('/').next())
        .map(str::to_string)
        .collect();
    names.dedup();
    Ok(names)
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
    let ssh = ssh_command(git, peer.host_key_alias.as_deref())?;
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
    .map_err(|err| match ssh_hint(&format!("{err:#}"), peer) {
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
///
/// `host_key_alias` (for peers found over mDNS) makes ssh check the host key
/// under that name instead of the IP address.
fn ssh_command(git: &Git, host_key_alias: Option<&str>) -> Result<Option<String>> {
    if std::env::var_os("GIT_SSH_COMMAND").is_some() || std::env::var_os("GIT_SSH").is_some() {
        return Ok(None);
    }
    if git.try_run(&["config", "core.sshCommand"])?.is_some() {
        return Ok(None);
    }
    let mut command = "ssh -o BatchMode=yes -o ConnectTimeout=10 \
         -o ServerAliveInterval=10 -o ServerAliveCountMax=3"
        .to_string();
    if let Some(alias) = host_key_alias {
        // The alias is a validated machine id with a fixed prefix: no quoting needed.
        command.push_str(&format!(" -o HostKeyAlias={alias}"));
    }
    Ok(Some(command))
}

/// Advice for the ssh failures every new setup runs into, since gitsync
/// cannot answer ssh's questions itself.
fn ssh_hint(error: &str, peer: &Peer) -> Option<String> {
    let host = peer
        .url
        .strip_prefix("ssh://")
        .and_then(|rest| rest.split('/').next())
        .unwrap_or("the peer");
    let target = ssh_target(host);
    if error.contains("Host key verification failed") {
        let command = match &peer.host_key_alias {
            Some(alias) => format!("ssh -o HostKeyAlias={alias} {target}"),
            None => format!("ssh {target}"),
        };
        Some(format!(
            "ssh does not know this peer's host key yet; connect once by hand \
             with `{command}` and accept it"
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

/// `user@host:port` from a URL, as arguments for the `ssh` command:
/// `-p port user@host` (ssh takes the port as an option, and IPv6 bare).
pub fn ssh_target(authority: &str) -> String {
    let (user, rest) = match authority.rsplit_once('@') {
        Some((user, rest)) => (format!("{user}@"), rest),
        None => (String::new(), authority),
    };
    let (addr, port) = match rest.strip_prefix('[').and_then(|r| r.split_once(']')) {
        Some((addr, tail)) => (addr, tail.strip_prefix(':')),
        None => match rest.rsplit_once(':') {
            Some((addr, port)) => (addr, Some(port)),
            None => (rest, None),
        },
    };
    match port {
        Some(port) => format!("-p {port} {user}{addr}"),
        None => format!("{user}{addr}"),
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
    use super::{ssh_hint, ssh_target};
    use crate::config::Peer;

    #[test]
    fn ssh_targets() {
        assert_eq!(ssh_target("me@host"), "me@host");
        assert_eq!(ssh_target("me@10.0.0.9:2222"), "-p 2222 me@10.0.0.9");
        assert_eq!(
            ssh_target("me@[2001:db8::5]:2222"),
            "-p 2222 me@2001:db8::5"
        );
        assert_eq!(ssh_target("me@[2001:db8::5]"), "me@2001:db8::5");
    }

    #[test]
    fn hints_for_common_ssh_failures() {
        let peer = Peer::new("desktop", "ssh://me@desktop.local/code/proj");
        let host_key = ssh_hint("Host key verification failed.", &peer).unwrap();
        assert!(host_key.contains("`ssh me@desktop.local`"), "{host_key}");
        let denied = ssh_hint("Permission denied (publickey).", &peer).unwrap();
        assert!(denied.contains("authorized_keys"), "{denied}");
        assert_eq!(ssh_hint("Connection refused", &peer), None);
    }

    #[test]
    fn host_key_hint_for_discovered_peer_uses_the_alias() {
        let mut peer = Peer::new("windows", "ssh://me@10.0.0.9:2222/c/repo");
        peer.host_key_alias = Some("gitsync-windows".into());
        let hint = ssh_hint("Host key verification failed.", &peer).unwrap();
        assert!(
            hint.contains("`ssh -o HostKeyAlias=gitsync-windows -p 2222 me@10.0.0.9`"),
            "{hint}"
        );
    }
}
