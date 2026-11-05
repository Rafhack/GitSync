//! gitsync: keeps a git repository in sync across machines. See DESIGN.md.

pub mod apply;
pub mod config;
pub mod connect;
pub mod discovery;
pub mod fetch;
pub mod git;
pub mod identity;
pub mod lock;
pub mod log_file;
pub mod pair;
pub mod serve;
pub mod service;
pub mod snapshot;
pub mod status;
pub mod tls;
pub mod trust;
pub mod watch;
