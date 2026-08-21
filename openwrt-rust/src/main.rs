use clap::Parser;
use meduza_openwrt::Cli;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .without_time()
        .init();

    if let Err(error) = meduza_openwrt::execute(Cli::parse()).await {
        tracing::error!("{error:#}");
        std::process::exit(1);
    }
}
