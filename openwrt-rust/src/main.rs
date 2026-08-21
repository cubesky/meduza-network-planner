use clap::Parser;
use meduza_openwrt::Cli;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        // procd forwards stdout/stderr into syslog. ANSI escape sequences are
        // useful on an interactive terminal but become visible garbage in
        // logread and in the LuCI controller-only log view.
        .with_ansi(false)
        // Keep the Rust target in every syslog line. LuCI uses the stable
        // `meduza_openwrt` target to show a controller-only log stream.
        .with_target(true)
        .without_time()
        .init();

    if let Err(error) = meduza_openwrt::execute(Cli::parse()).await {
        tracing::error!("{error:#}");
        std::process::exit(1);
    }
}
