use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};

use gitsync::apply::{self as sync, sync_peer, Plan, Side};
use gitsync::config::{Config, Discovery, Peer};
use gitsync::discovery::{ipv6_note, Announcement, Found, Mdns, PeerEvent};
use gitsync::fetch::{fetch_peer, fetched_peer_names, ssh_target};
use gitsync::git::Git;
use gitsync::log_file::LogFile;
use gitsync::service::Service;
use gitsync::snapshot::{self, Outcome};
use gitsync::status::Status;
use gitsync::watch;

/// Keep a git repository, including uncommitted work, in sync across machines.
///
/// Settings are read from .git/gitsync.toml when it exists; flags override it.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Repository to work on (default: current directory).
    #[arg(short = 'C', global = true, default_value = ".")]
    dir: PathBuf,

    /// Name of this machine in ref names (default: config file, then hostname).
    #[arg(long, global = true)]
    machine_id: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Save uncommitted work to refs/gitsync/<machine>/wip once.
    Snapshot,
    /// Snapshot automatically whenever files change. Ctrl+C to stop.
    Watch {
        /// Snapshot at least this often, even without file events
        /// (default: poll_interval_secs from the config, else 30).
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
        poll_secs: Option<u64>,
        /// Wait for this much quiet after a change before snapshotting
        /// (default: debounce_ms from the config, else 1000).
        #[arg(long)]
        debounce_ms: Option<u64>,
        /// Write the log to this file instead of the terminal (the background
        /// service uses this on Windows and macOS).
        #[arg(long, value_name = "FILE")]
        log_file: Option<PathBuf>,
    },
    /// Run `gitsync watch` for this repo in the background, from login on.
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Show this machine's snapshot, settings and known peers.
    Status,
    /// Fetch branches and snapshots from the peers in the config and, with
    /// mDNS discovery, the ones on the LAN (or just one peer).
    Fetch {
        /// Only this peer (its name).
        peer: Option<String>,
    },
    /// Apply what was fetched, when safe: the peer's snapshot to your files,
    /// and fast-forwards of branches that are behind.
    Apply {
        /// Only this peer (its `name` in the config).
        peer: Option<String>,
    },
    /// Show conflicts with peers (both sides changed the same lines), or
    /// settle one: the chosen side wins where both changed the same lines;
    /// changes made on only one side are kept either way.
    Resolve {
        /// Take the peer's version of the conflicting lines.
        #[arg(long, value_name = "PEER", conflicts_with = "keep_mine")]
        take: Option<String>,
        /// Keep your version of the conflicting lines (with every peer in
        /// conflict, or just PEER).
        #[arg(long, value_name = "PEER", num_args = 0..=1, default_missing_value = "")]
        keep_mine: Option<String>,
    },
    /// List the peers for this repo announcing themselves on the LAN (mDNS).
    Discover {
        /// How long to listen.
        #[arg(long, default_value_t = 3)]
        secs: u64,
    },
}

#[derive(Subcommand)]
enum ServiceAction {
    /// Set up the service for this repo and start it (again, if it was
    /// installed already: picks up a new gitsync program or config).
    Install,
    /// Stop the service and remove it.
    Uninstall,
    /// Start the installed service.
    Start,
    /// Stop the service until `start` or the next login.
    Stop,
    /// Is it installed and running, and where is its log.
    Status,
    /// What the Windows task runs: keeps `gitsync watch` running.
    #[command(hide = true)]
    Run {
        #[arg(long, value_name = "FILE")]
        log_file: PathBuf,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let filter = || {
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "gitsync=info".into())
    };
    match &cli.command {
        Command::Watch {
            log_file: Some(path),
            ..
        } => tracing_subscriber::fmt()
            .with_env_filter(filter())
            .with_ansi(false)
            .with_writer(Mutex::new(LogFile::open(path)?))
            .init(),
        Command::Service {
            action: ServiceAction::Run { log_file },
        } => tracing_subscriber::fmt()
            .with_env_filter(filter())
            .with_ansi(false)
            .with_writer(gitsync::service::append_to(log_file.clone()))
            .init(),
        _ => tracing_subscriber::fmt()
            .with_env_filter(filter())
            .with_ansi(std::io::stdout().is_terminal())
            .init(),
    }

    let git = Git::new(cli.dir);

    // Effective settings: config file, then command line flags on top.
    let mut config = Config::load(&git)?;
    if let Some(id) = cli.machine_id {
        config.machine_id = Some(id);
    }
    if let Command::Watch {
        poll_secs,
        debounce_ms,
        ..
    } = &cli.command
    {
        config.poll_interval_secs = poll_secs.unwrap_or(config.poll_interval_secs);
        config.debounce_ms = debounce_ms.unwrap_or(config.debounce_ms);
    }
    config.validate()?;
    let machine_id = config.machine_id_or_default()?;

    match cli.command {
        Command::Snapshot => match snapshot::snapshot(&git, &machine_id)? {
            Outcome::Created(commit) => {
                println!("{commit} {}", snapshot::wip_ref(&machine_id))
            }
            Outcome::Unchanged => println!("no changes since last snapshot"),
        },
        Command::Watch { .. } => watch::watch(&git, &machine_id, &config)?,
        Command::Fetch { peer } => fetch(&git, &machine_id, &config, peer.as_deref())?,
        Command::Apply { peer } => apply(&git, &machine_id, &config, peer.as_deref())?,
        Command::Service { action } => service(&git, action)?,
        Command::Resolve { take, keep_mine } => {
            resolve(&git, &machine_id, &config, take, keep_mine)?
        }
        Command::Discover { secs } => discover(&git, &machine_id, &config, secs)?,
        Command::Status => print!("{}", Status::collect(&git, &machine_id, config)?),
    }
    Ok(())
}

/// `gitsync fetch`: fetch every configured peer (or one), print what changed.
/// Keeps going if one peer fails, but exits with an error at the end.
fn fetch(git: &Git, machine_id: &str, config: &Config, only: Option<&str>) -> Result<()> {
    let mut peers = config.peers.clone();
    if config.discovery == Discovery::Mdns {
        for found in listen(git, machine_id, config, Duration::from_secs(2))? {
            // A configured peer's address wins over an announced one.
            if !peers.iter().any(|p| p.name == found.peer.name) {
                peers.push(found.peer);
            }
        }
    }
    let peers = select(peers, only)?;
    let mut failed = 0;
    for peer in &peers {
        match fetch_peer(git, peer) {
            Ok(report) => {
                if report.changes.is_empty() {
                    println!("{}: up to date", peer.name);
                }
                for change in &report.changes {
                    let what = match (&change.old, &change.new) {
                        (None, Some(new)) => format!("new {}", &new[..12]),
                        (Some(old), Some(new)) => format!("{}..{}", &old[..12], &new[..12]),
                        (_, None) => "deleted".to_string(),
                    };
                    println!("{}: {} {what}", peer.name, change.name);
                }
                if !report.has_snapshot {
                    println!(
                        "{}: note: no snapshot found; is gitsync running there with machine_id = {:?}?",
                        peer.name, peer.name
                    );
                }
            }
            Err(err) => {
                eprintln!("{}: {err:#}", peer.name);
                failed += 1;
            }
        }
    }
    if failed > 0 {
        bail!("{failed} peer(s) could not be fetched");
    }
    Ok(())
}

/// `gitsync apply`: apply what was already fetched, peer by peer: the
/// configured peers and any found on the LAN and fetched before.
fn apply(git: &Git, machine_id: &str, config: &Config, only: Option<&str>) -> Result<()> {
    for peer in select(known_peers(git, machine_id, config)?, only)? {
        let report = sync_peer(git, machine_id, &peer.name)?;
        match &report.plan {
            Some(plan @ (Plan::Apply { .. } | Plan::Merge { .. })) => {
                let verb = if matches!(plan, Plan::Merge { .. }) {
                    "merged"
                } else {
                    "applied"
                };
                println!("{}: {verb}", peer.name);
                for file in &report.applied_files {
                    println!("  {file}");
                }
            }
            Some(plan) => println!("{}: {plan}", peer.name),
            None => {}
        }
        for (branch, old, new) in &report.branches {
            println!(
                "{}: fast-forwarded {branch} {}..{}",
                peer.name,
                &old[..12],
                &new[..12]
            );
        }
    }
    Ok(())
}

/// `gitsync service ...`
fn service(git: &Git, action: ServiceAction) -> Result<()> {
    if let ServiceAction::Run { log_file } = &action {
        return gitsync::service::supervise(git, log_file);
    }
    let service = Service::for_repo(git)?;
    match action {
        ServiceAction::Install => {
            let state = service.state()?;
            if !state.running && watch::is_running(git)? {
                bail!(
                    "gitsync watch is already running for this repo; stop it (Ctrl+C) \
                     and run this again"
                );
            }
            service.install()?;
            println!("installed and started {}", service.name);
            println!(
                "  runs:  {} {}",
                service.exe.display(),
                service.program_args().join(" ")
            );
            println!("  from:  {}", service.location()?);
            println!("  log:   {}", service.log_hint());
            if let Some(note) = service.linger_note() {
                println!("  note:  {note}");
            }
            println!("it starts again at every login; `gitsync service uninstall` removes it");
        }
        ServiceAction::Uninstall => {
            if !service.state()?.installed {
                bail!("no gitsync service is installed for this repo");
            }
            service.uninstall()?;
            println!("stopped and removed {}", service.name);
        }
        ServiceAction::Start => {
            if !service.state()?.installed {
                bail!("no gitsync service is installed for this repo (`gitsync service install`)");
            }
            service.start()?;
            println!("started {}", service.name);
        }
        ServiceAction::Stop => {
            service.stop()?;
            println!(
                "stopped {} (it starts again at the next login)",
                service.name
            );
        }
        ServiceAction::Run { .. } => unreachable!("handled above"),
        ServiceAction::Status => {
            let state = service.state()?;
            let text = match (state.installed, state.running) {
                (false, _) => "not installed (`gitsync service install` sets it up)",
                (true, true) => "running",
                (true, false) => "installed, not running (`gitsync service start`)",
            };
            println!("service  {}: {text}", service.name);
            if state.installed {
                println!("  from:  {}", service.location()?);
                println!(
                    "  runs:  {} {}",
                    service.exe.display(),
                    service.program_args().join(" ")
                );
                println!("  log:   {}", service.log_hint());
            }
        }
    }
    Ok(())
}

/// Every peer that was fetched or is in the config, except this machine.
fn known_peers(git: &Git, machine_id: &str, config: &Config) -> Result<Vec<Peer>> {
    let mut peers = config.peers.clone();
    for name in fetched_peer_names(git)? {
        if name != machine_id && !peers.iter().any(|p| p.name == name) {
            peers.push(Peer::new(name, ""));
        }
    }
    Ok(peers)
}

/// `gitsync resolve`: list conflicts, or settle them (`--take <peer>`,
/// `--keep-mine [peer]`).
fn resolve(
    git: &Git,
    machine_id: &str,
    config: &Config,
    take: Option<String>,
    keep_mine: Option<String>,
) -> Result<()> {
    let (side, only) = match (take, keep_mine) {
        (Some(peer), _) => (Some(Side::Theirs), Some(peer)),
        (None, Some(peer)) => (Some(Side::Mine), Some(peer).filter(|p| !p.is_empty())),
        (None, None) => (None, None),
    };
    // Compare against what is on disk now (and let `git diff` below show it).
    snapshot::snapshot(git, machine_id)?;
    let mut conflicts = Vec::new();
    for peer in select(known_peers(git, machine_id, config)?, only.as_deref())? {
        if let Plan::Conflict { files } = sync::plan(git, machine_id, &peer.name)? {
            conflicts.push((peer.name, files));
        }
    }
    if conflicts.is_empty() {
        println!("no conflicts");
        return Ok(());
    }

    let Some(side) = side else {
        for (peer, files) in &conflicts {
            let theirs = snapshot::wip_ref(peer);
            let mine = snapshot::wip_ref(machine_id);
            println!("{peer}: both sides changed these files in ways that do not merge");
            println!("(the same lines, or one side deleted the file):");
            for file in files {
                println!("  {file}");
            }
            println!("  compare:            git diff {mine} {theirs} -- <file>");
            println!("  see their version:  git show {theirs}:<file>");
        }
        println!();
        println!("settle it with one of:");
        println!("  gitsync resolve --take <peer>   their version of the conflicting lines");
        println!("  gitsync resolve --keep-mine     your version (edit the files first to combine by hand)");
        println!("changes made on only one side are kept either way");
        return Ok(());
    };

    for (peer, _) in &conflicts {
        let report = sync::resolve(git, machine_id, peer, side)?;
        let whose = if side == Side::Mine {
            "yours"
        } else {
            "theirs"
        };
        println!("{peer}: resolved, keeping {whose} where both changed the same lines");
        for file in &report.applied_files {
            println!("  {file}");
        }
    }
    Ok(())
}

/// All `peers`, or just the one named `only`.
fn select(peers: Vec<Peer>, only: Option<&str>) -> Result<Vec<Peer>> {
    let peers: Vec<_> = peers
        .into_iter()
        .filter(|peer| only.is_none_or(|name| peer.name == name))
        .collect();
    if peers.is_empty() {
        match only {
            Some(name) => bail!("no peer named {name:?} (configured or found on the LAN)"),
            None => bail!(
                "no peers yet; with discovery = \"mdns\" they are found while \
                 `gitsync watch` runs on them, or list them as [[peers]] in .git/gitsync.toml"
            ),
        }
    }
    Ok(peers)
}

/// Peers for this repo announcing themselves on the LAN, heard within `wait`.
fn listen(git: &Git, machine_id: &str, config: &Config, wait: Duration) -> Result<Vec<Found>> {
    let Some(ours) = Announcement::for_repo(git, machine_id, config)? else {
        bail!("this repo has no commits yet, so it has no id peers could match");
    };
    let mdns = Mdns::start()?;
    let events = mdns.browse(&ours.repo, machine_id)?;
    let deadline = Instant::now() + wait;
    let mut found: Vec<Found> = Vec::new();
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        match events.recv_timeout(left) {
            Ok(PeerEvent::Found(peer)) => {
                found.retain(|f| f.peer.name != peer.peer.name);
                found.push(peer);
            }
            Ok(PeerEvent::Lost(name)) => found.retain(|f| f.peer.name != name),
            Err(_) => break,
        }
    }
    found.sort_by(|a, b| a.peer.name.cmp(&b.peer.name));
    Ok(found)
}

/// `gitsync discover`: who is out there, and is ssh ready to reach them?
fn discover(git: &Git, machine_id: &str, config: &Config, secs: u64) -> Result<()> {
    let found = listen(git, machine_id, config, Duration::from_secs(secs))?;
    if found.is_empty() {
        println!("no peers for this repo found on the LAN within {secs}s");
        println!("(is `gitsync watch` running on them, with discovery = \"mdns\"?)");
        return Ok(());
    }
    for f in &found {
        println!(
            "{}  {}  (gen {}){}",
            f.peer.name,
            f.peer.url,
            f.gen,
            ipv6_note(&f.peer.url)
        );
        let alias = f.peer.host_key_alias.as_deref().unwrap_or_default();
        if !host_key_known(alias) {
            let target = f
                .peer
                .url
                .strip_prefix("ssh://")
                .and_then(|rest| rest.split('/').next())
                .unwrap_or_default();
            println!("  ssh does not know this peer yet; connect once by hand and accept its key:");
            println!("  ssh -o HostKeyAlias={alias} {}", ssh_target(target));
        }
    }
    Ok(())
}

/// True if `~/.ssh/known_hosts` has a key filed under `name`.
fn host_key_known(name: &str) -> bool {
    std::process::Command::new("ssh-keygen")
        .args(["-F", name])
        .output()
        .map(|out| out.status.success() && !out.stdout.is_empty())
        .unwrap_or(false)
}
