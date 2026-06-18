mod cli;
mod config;
mod daemon;
mod diagnostics;
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

use std::process::ExitCode;

fn main() -> ExitCode {
    let cli = cli::parse();
    if let Err(e) = init_logging(cli.verbose, cli.quiet) {
        eprintln!("error: {e:#}");
        return ExitCode::from(2);
    }
    tracing::debug!(version = env!("CARGO_PKG_VERSION"), "periodic starting");
    match dispatch(cli) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::from(2)
        }
    }
}

/// Route a parsed command to its handler. Commands without an implementation yet
/// report so explicitly.
fn dispatch(cli: cli::Cli) -> anyhow::Result<ExitCode> {
    use cli::Command;
    match cli.command {
        Command::Validate(args) => Ok(run_validate(&args)),
        Command::SelfUpdate(args) => {
            self_update::run(args.next, args.tag).map(|()| ExitCode::SUCCESS)
        }
        Command::Daemon(_) => unimplemented("daemon"),
        Command::Jobs(_) => unimplemented("jobs"),
        Command::Logs => unimplemented("logs"),
        Command::Reload => unimplemented("reload"),
        Command::Doctor => unimplemented("doctor"),
        Command::Completion => unimplemented("completion"),
    }
}

/// Orchestrate `periodic validate`: read → parse → validate → render → exit code.
fn run_validate(args: &cli::ValidateArgs) -> ExitCode {
    let path = args.path.clone().unwrap_or_else(cli::default_config_path);
    let display = path.display().to_string();

    let yaml = match std::fs::read_to_string(&path) {
        Ok(y) => y,
        Err(e) => {
            eprintln!("error: cannot read {display}: {e}");
            return ExitCode::from(2);
        }
    };

    let (jobs, diagnostics) = match config::parse(&yaml) {
        Ok(cfg) => {
            let n = cfg.jobs.len();
            (n, validation::validate_config(&cfg))
        }
        Err(d) => (0, vec![d]),
    };

    let report = output::build_report(&display, jobs, &diagnostics);
    let rendered = match args.format {
        cli::OutputFormat::Json => output::render_json(&report),
        cli::OutputFormat::Human => output::render_human(&report),
    };
    println!("{rendered}");

    if report.ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

fn unimplemented(name: &str) -> anyhow::Result<ExitCode> {
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
