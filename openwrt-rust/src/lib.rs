pub mod agent;
pub mod atomic;
pub mod cli;
pub mod command;
pub mod config;
pub mod etcd;
pub mod firewall;
pub mod model;
pub mod ownership;
pub mod reconciler;
pub mod render;
pub mod report;
pub mod runtime;
pub mod state;

pub const OWNER: &str = "meduza-openwrt-rust-v1";

pub use cli::{Cli, Command};

pub async fn execute(cli: Cli) -> anyhow::Result<()> {
    // Root-prefixing remains an internal test facility on `Paths`; exposing it
    // through the production CLI would mix offline files with the live host's
    // process and network namespaces.
    let paths = state::Paths::from_root(None);
    let runner = command::SystemRunner;

    match cli.command.unwrap_or(Command::Daemon) {
        Command::Daemon => agent::Agent::load(paths, runner)?.serve().await,
        Command::Apply { snapshot } => {
            let settings = config::Settings::load(&runner)?;
            let snapshot = state::Snapshot::read_from(&snapshot)?;
            reconciler::Reconciler::new(paths, runner).apply(&settings, &snapshot)
        }
        Command::Recover => {
            let settings = config::Settings::load(&runner)?;
            reconciler::Reconciler::new(paths, runner).recover(&settings)
        }
        Command::RuntimeStop => reconciler::Reconciler::new(paths, runner).runtime_stop(),
        Command::Purge => reconciler::Reconciler::new(paths, runner).purge(),
        Command::Status { json } => {
            reconciler::Reconciler::new(paths.clone(), runner).migrate_layout()?;
            report::print_status(&paths, &runner, json)
        }
        Command::Doctor => reconciler::doctor(&paths, &runner),
    }
}
