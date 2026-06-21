mod cli;
mod config;
mod daemon;
mod diagnostics;
mod doctor;
mod error;
mod events;
mod executor;
mod ipc;
mod logs;
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
        Command::Jobs(cmd) => run_jobs(cmd),
        Command::Logs => unimplemented("logs"),
        Command::Reload => unimplemented("reload"),
        Command::Doctor => doctor::run(),
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

/// Route `periodic jobs …`. Only the read commands are implemented in phase 0.4.
fn run_jobs(cmd: cli::JobsCommand) -> anyhow::Result<ExitCode> {
    use cli::JobsCommand;
    match cmd {
        JobsCommand::List(args) => run_jobs_list(&args),
        JobsCommand::Status(args) => run_jobs_status(&args),
        JobsCommand::Add => unimplemented("jobs add"),
        JobsCommand::Run => unimplemented("jobs run"),
        JobsCommand::Pause => unimplemented("jobs pause"),
        JobsCommand::Resume => unimplemented("jobs resume"),
        JobsCommand::Remove => unimplemented("jobs remove"),
        JobsCommand::Edit => unimplemented("jobs edit"),
        JobsCommand::History => unimplemented("jobs history"),
    }
}

/// Load + validate the config and reconcile it into the state DB, returning an
/// open connection. The read commands call this so `list`/`status` reflect the
/// current config with freshly computed next-run times — there is no daemon yet
/// to maintain the projection.
fn project_state() -> anyhow::Result<rusqlite::Connection> {
    let path = cli::default_config_path();
    let display = path.display().to_string();
    let yaml = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("cannot read {display}: {e}"))?;
    let raw = config::parse(&yaml)
        .map_err(|d| anyhow::anyhow!("config invalid: {} ({})", d.message, d.path))?;
    if validation::validate_config(&raw)
        .iter()
        .any(|d| d.is_error())
    {
        anyhow::bail!("config invalid; run `periodic validate` for details");
    }
    let effective = config::normalize(&raw);
    let conn = state::open(&state::default_db_path())?;
    state::reconcile(&conn, &effective, chrono::Utc::now())?;
    Ok(conn)
}

/// `periodic jobs list`: reconcile, then render every job projection.
fn run_jobs_list(args: &cli::JobsListArgs) -> anyhow::Result<ExitCode> {
    let conn = project_state()?;
    let jobs = state::list_job_states(&conn)?;
    let rendered = match args.format {
        cli::OutputFormat::Json => output::render_jobs_json(&jobs),
        cli::OutputFormat::Human => output::render_jobs_human(&jobs),
    };
    println!("{rendered}");
    Ok(ExitCode::SUCCESS)
}

/// `periodic jobs status <id>`: reconcile, then render one job (exit 1 if absent).
fn run_jobs_status(args: &cli::JobsStatusArgs) -> anyhow::Result<ExitCode> {
    let conn = project_state()?;
    match state::get_job_state(&conn, &args.id)? {
        Some(job) => {
            let rendered = match args.format {
                cli::OutputFormat::Json => output::render_job_json(&job),
                cli::OutputFormat::Human => output::render_job_human(&job),
            };
            println!("{rendered}");
            Ok(ExitCode::SUCCESS)
        }
        None => {
            eprintln!("error: no such job: {}", args.id);
            Ok(ExitCode::from(1))
        }
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
