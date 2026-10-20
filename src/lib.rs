//! gitsync: keeps a git repository in sync across machines. See DESIGN.md.

pub mod apply;
pub mod config;
pub mod discovery;
pub mod fetch;
pub mod git;
pub mod lock;
pub mod log_file;
pub mod service;
pub mod snapshot;
pub mod status;
pub mod watch;
