mod cli;
mod config;
mod daemon;
mod doctor;
mod error;
mod events;
mod executor;
mod ipc;
mod output;
mod scheduler;
mod state;
mod util;
mod validation;

use anyhow::Context;

fn main() -> anyhow::Result<()> {
    let cli = cli::parse();
    init_logging(cli.verbose, cli.quiet).context("initializing periodic")?;
    tracing::debug!(version = env!("CARGO_PKG_VERSION"), "periodic starting");
    cli::dispatch(cli)
}

/// Install the global tracing subscriber. `RUST_LOG` wins when set; otherwise
/// the level follows `--verbose` / `--quiet`, defaulting to `info`. Returns a
/// typed [`error::Error`] so the binary boundary can add context.
fn init_logging(verbose: bool, quiet: bool) -> error::Result<()> {
    use tracing_subscriber::{EnvFilter, fmt};

    let default = if verbose {
        "debug"
    } else if quiet {
        "warn"
    } else {
        "info"
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default));
    fmt()
        .with_env_filter(filter)
        .try_init()
        .map_err(|e| error::Error::Logging(e.to_string()))?;
    Ok(())
}
