use clap::Parser;
use meduza_openwrt::{Cli, Command};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let machine_json = matches!(cli.command.as_ref(), Some(Command::Status { json: true }));
    if !machine_json {
        let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            // procd forwards stdout/stderr into syslog. ANSI escape sequences
            // become visible garbage in logread and the LuCI log view.
            .with_ansi(false)
            // Keep the Rust target in every syslog line. LuCI uses this stable
            // target to show a controller-only log stream.
            .with_target(true)
            .without_time()
            .init();
    }

    if let Err(error) = meduza_openwrt::execute(cli).await {
        // `status --json` is an RPC/CGI boundary: stdout must contain exactly
        // one JSON document on success, and failures are signalled only by the
        // exit code so merged stderr cannot corrupt a caller's JSON parser.
        if !machine_json {
            tracing::error!("{error:#}");
        }
        std::process::exit(1);
    }
}
