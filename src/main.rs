use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};

use gitsync::config::Config;
use gitsync::git::Git;
use gitsync::snapshot::{self, Outcome};
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
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "gitsync=info".into()),
        )
        .init();

    let cli = Cli::parse();
    let git = Git::new(cli.dir);

    // Effective settings: config file, then command line flags on top.
    let mut config = Config::load(&git)?;
    if let Some(id) = cli.machine_id {
        config.machine_id = Some(id);
    }
    if let Command::Watch {
        poll_secs,
        debounce_ms,
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
        Command::Watch { .. } => watch::watch(
            &git,
            &machine_id,
            Duration::from_millis(config.debounce_ms),
            Duration::from_secs(config.poll_interval_secs),
        )?,
    }
    Ok(())
}
