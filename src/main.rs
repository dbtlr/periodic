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
mod self_update;
mod state;
mod util;
mod validation;

use anyhow::Context;

use cli::Command;

fn main() -> anyhow::Result<()> {
    let cli = cli::parse();
    init_logging(cli.verbose, cli.quiet).context("initializing periodic")?;
    tracing::debug!(version = env!("CARGO_PKG_VERSION"), "periodic starting");
    dispatch(cli)
}

/// Route a parsed command to its handler. Commands without an implementation yet
/// report so explicitly.
fn dispatch(cli: cli::Cli) -> anyhow::Result<()> {
    match cli.command {
        Command::SelfUpdate(args) => self_update::run(args.next, args.tag),
        Command::Daemon(_) => unimplemented("daemon"),
        Command::Jobs(_) => unimplemented("jobs"),
        Command::Logs => unimplemented("logs"),
        Command::Validate => unimplemented("validate"),
        Command::Reload => unimplemented("reload"),
        Command::Doctor => unimplemented("doctor"),
        Command::Completion => unimplemented("completion"),
    }
}

fn unimplemented(name: &str) -> anyhow::Result<()> {
    anyhow::bail!("`periodic {name}` is not implemented yet")
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
