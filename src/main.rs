use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};

use gitsync::git::Git;
use gitsync::snapshot::{self, Outcome};
use gitsync::watch;

/// Keep a git repository, including uncommitted work, in sync across machines.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Repository to work on (default: current directory).
    #[arg(short = 'C', global = true, default_value = ".")]
    dir: PathBuf,

    /// Name of this machine in ref names (default: hostname).
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
        /// Snapshot at least this often, even without file events.
        #[arg(long, default_value_t = 30)]
        poll_secs: u64,
        /// Wait for this much quiet after a change before snapshotting.
        #[arg(long, default_value_t = 1000)]
        debounce_ms: u64,
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
    let machine_id = match cli.machine_id {
        Some(id) => {
            snapshot::validate_machine_id(&id)?;
            id
        }
        None => snapshot::default_machine_id()?,
    };

    match cli.command {
        Command::Snapshot => match snapshot::snapshot(&git, &machine_id)? {
            Outcome::Created(commit) => {
                println!("{commit} {}", snapshot::wip_ref(&machine_id))
            }
            Outcome::Unchanged => println!("no changes since last snapshot"),
        },
        Command::Watch {
            poll_secs,
            debounce_ms,
        } => watch::watch(
            &git,
            &machine_id,
            Duration::from_millis(debounce_ms),
            Duration::from_secs(poll_secs),
        )?,
    }
    Ok(())
}
