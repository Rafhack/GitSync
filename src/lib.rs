//! gitsync: keeps a git repository in sync across machines. See DESIGN.md.

pub mod config;
pub mod fetch;
pub mod git;
pub mod snapshot;
pub mod status;
pub mod watch;
