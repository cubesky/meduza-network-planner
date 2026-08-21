use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "meduza-openwrt", version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Poll etcd, reconcile on /commit changes, and publish health.
    Daemon,
    /// Apply a complete, previously captured snapshot once.
    Apply { snapshot: PathBuf },
    /// Restore the durable last-known-good snapshot and exit.
    Recover,
    /// Stop only Meduza-owned VPN runtimes and restore FRR.
    RuntimeStop,
    /// Remove all strongly-owned runtime, UCI and persistent resources.
    Purge,
    /// Print locally observed tunnel state.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Validate platform dependencies, settings and durable state.
    Doctor,
}
