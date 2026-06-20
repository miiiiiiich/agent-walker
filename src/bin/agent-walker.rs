use anyhow::Result;
use clap::Parser;
use tracing_subscriber::EnvFilter;

use agent_walker::{Args, run};

fn main() -> Result<()> {
    let args = Args::parse();
    init_tracing(args.verbose);
    run(args)
}

fn init_tracing(verbose: bool) {
    let default_level = if verbose { "debug" } else { "warn" };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}
