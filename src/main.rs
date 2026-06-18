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
    init_logging().context("initializing periodic")?;
    tracing::debug!(version = env!("CARGO_PKG_VERSION"), "periodic starting");
    Ok(())
}

/// Install the global tracing subscriber, honoring `RUST_LOG` and defaulting to
/// `info`. Returns a typed [`error::Error`] so the binary boundary can add context.
fn init_logging() -> error::Result<()> {
    use tracing_subscriber::{EnvFilter, fmt};

    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    fmt()
        .with_env_filter(filter)
        .try_init()
        .map_err(|e| error::Error::Logging(e.to_string()))?;
    Ok(())
}
