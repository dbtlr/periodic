mod cli;
mod config;
mod config_edit;
mod config_mutation;
mod daemon;
mod diagnostics;
mod doctor;
mod error;
mod events;
mod executor;
mod ipc;
mod job_block;
mod logs;
mod output;
mod scheduler;
mod self_update;
mod service;
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
        Command::Daemon(cmd) => daemon::run(cmd),
        Command::Service(cmd) => service::run(cmd),
        Command::Jobs(cmd) => run_jobs(cmd),
        Command::Logs(args) => run_logs(&args),
        Command::Reload => run_reload(),
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
        JobsCommand::Add(args) => run_jobs_add(&args),
        JobsCommand::Run(args) => run_jobs_run(&args),
        JobsCommand::Pause(args) => run_jobs_set_enabled(&args, false),
        JobsCommand::Resume(args) => run_jobs_set_enabled(&args, true),
        JobsCommand::Remove(args) => run_jobs_remove(&args),
        JobsCommand::Edit => unimplemented("jobs edit"),
        JobsCommand::History(args) => run_jobs_history(&args),
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

/// `periodic jobs run <id>`: load+validate config, then execute the job in the
/// foreground. Exit 0 success · 1 run failed/timeout/cancelled · 2 usage/invalid.
fn run_jobs_run(args: &cli::JobsRunArgs) -> anyhow::Result<ExitCode> {
    install_sigint_forwarder();
    let path = cli::default_config_path();
    let yaml = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("cannot read {}: {e}", path.display()))?;
    let raw = config::parse(&yaml)
        .map_err(|d| anyhow::anyhow!("config invalid: {} ({})", d.message, d.path))?;
    // Invalid config → usage error (exit 2), no run row (spec §4).
    if validation::validate_config(&raw)
        .iter()
        .any(|d| d.is_error())
    {
        eprintln!("error: config invalid; run `periodic validate` for details");
        return Ok(ExitCode::from(2));
    }
    let effective = config::normalize(&raw);
    let Some(job) = effective
        .jobs
        .iter()
        .find(|j| j.id.as_deref() == Some(args.id.as_str()))
    else {
        eprintln!("error: no such job: {}", args.id);
        return Ok(ExitCode::from(2));
    };
    let conn = state::open(&state::default_db_path())?;
    state::reconcile(&conn, &effective, chrono::Utc::now())?;
    // Disabled jobs run on explicit manual trigger (spec §4) — no gating here.
    let outcome = executor::run_job(
        &conn,
        &state::default_logs_dir(),
        job,
        chrono::Utc::now(),
        &executor::CANCEL,
    )?;
    let rendered = match args.format {
        cli::OutputFormat::Json => output::render_run_json(&outcome),
        cli::OutputFormat::Human => output::render_run_human(&outcome),
    };
    print!("{rendered}");
    Ok(match outcome.status {
        executor::RunStatus::Success => ExitCode::SUCCESS,
        _ => ExitCode::from(1),
    })
}

/// `periodic jobs history <id>`: list recorded runs (exit 1 if job unknown).
fn run_jobs_history(args: &cli::JobsHistoryArgs) -> anyhow::Result<ExitCode> {
    let conn = project_state()?;
    if !state::job_exists(&conn, &args.id)? {
        eprintln!("error: no such job: {}", args.id);
        return Ok(ExitCode::from(1));
    }
    let runs = state::list_runs(&conn, &args.id, args.limit)?;
    let rendered = match args.format {
        cli::OutputFormat::Json => output::render_runs_json(&runs),
        cli::OutputFormat::Human => output::render_runs_human(&runs),
    };
    print!("{rendered}");
    Ok(ExitCode::SUCCESS)
}

/// `periodic logs <id>`: render captured output from the daily JSONL files
/// (exit 1 if the job is unknown — distinct from a known job with no output).
fn run_logs(args: &cli::LogsArgs) -> anyhow::Result<ExitCode> {
    let conn = project_state()?;
    if !state::job_exists(&conn, &args.id)? {
        eprintln!("error: no such job: {}", args.id);
        return Ok(ExitCode::from(1));
    }
    let lines = logs::read_logs(&state::default_logs_dir(), &args.id, args.run.as_deref())?;
    let rendered = match args.format {
        cli::OutputFormat::Json => output::render_logs_json(&lines),
        cli::OutputFormat::Human => output::render_logs_human(&lines),
    };
    print!("{rendered}");
    Ok(ExitCode::SUCCESS)
}

/// `periodic reload`: validate the config, then apply it dual-mode per the config
/// mutation-ownership model. A running daemon owns the live schedule, so reload is
/// requested over IPC (the daemon re-validates and atomically swaps its in-memory
/// table, keeping last-known-good if the new config is invalid). When the daemon is
/// stopped, the on-disk YAML is the source of truth, so validation is the whole job
/// — the next `daemon start` reads it. Never asks a running daemon to load an
/// invalid config: a config error exits 1 before any IPC.
fn run_reload() -> anyhow::Result<ExitCode> {
    let path = cli::default_config_path();
    let display = path.display().to_string();
    let yaml = std::fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("cannot read {display}: {e}"))?;
    let raw = match config::parse(&yaml) {
        Ok(r) => r,
        Err(d) => {
            eprintln!("error: config invalid: {} ({})", d.message, d.path);
            return Ok(ExitCode::from(1));
        }
    };
    if validation::validate_config(&raw)
        .iter()
        .any(|d| d.is_error())
    {
        eprintln!("error: config invalid; run `periodic validate` for details");
        return Ok(ExitCode::from(1));
    }

    let conn = state::open(&state::default_db_path())?;
    let running = matches!(
        state::read_daemon_status(&conn)?,
        Some(s) if s.state == "running"
    );
    if !running {
        println!("config valid; daemon not running (applied on next start)");
        return Ok(ExitCode::SUCCESS);
    }

    let mut client = ipc::Client::connect(&ipc::socket_path())
        .map_err(|e| anyhow::anyhow!("cannot reach daemon (it may have crashed): {e}"))?;
    let req = ipc::Request {
        id: "reload".to_owned(),
        method: "daemon.reload".to_owned(),
        params: serde_json::json!({}),
    };
    match client
        .call(&req)
        .map_err(|e| anyhow::anyhow!("reload request failed: {e}"))?
    {
        ipc::Response::Ok { .. } => {
            println!("reload requested");
            Ok(ExitCode::SUCCESS)
        }
        ipc::Response::Err { error, .. } => {
            eprintln!("error: daemon refused reload: {}", error.message);
            Ok(ExitCode::from(1))
        }
    }
}

/// Apply a config mutation in the right ownership mode (config-mutation-model):
/// directly to disk when the daemon is stopped, or over IPC — the daemon owns
/// the write and reloads — when it is running. The `jobs` mutation commands
/// (PDC-82–85) wire onto this single dispatch.
fn dispatch_mutation(
    mutation: &config_mutation::Mutation,
) -> anyhow::Result<config_mutation::Outcome> {
    use config_mutation::Outcome;
    let conn = state::open(&state::default_db_path())?;
    let running = matches!(
        state::read_daemon_status(&conn)?,
        Some(s) if s.state == "running"
    );
    if running {
        let (method, params) = config_mutation::mutation_to_request(mutation);
        let mut client = ipc::Client::connect(&ipc::socket_path())
            .map_err(|e| anyhow::anyhow!("cannot reach daemon (it may have crashed): {e}"))?;
        let req = ipc::Request {
            id: method.to_owned(),
            method: method.to_owned(),
            params,
        };
        match client
            .call(&req)
            .map_err(|e| anyhow::anyhow!("mutation request failed: {e}"))?
        {
            ipc::Response::Ok { .. } => Ok(Outcome::Applied),
            ipc::Response::Err { error, .. } => Ok(Outcome::Refused(error.message)),
        }
    } else {
        match config_mutation::apply_to_disk(&cli::default_config_path(), mutation) {
            Ok(()) => Ok(Outcome::Applied),
            // Domain refusal → exit 1; an I/O/system failure → exit 2 (propagate).
            Err(config_mutation::ApplyError::Refused(msg)) => Ok(Outcome::Refused(msg)),
            Err(config_mutation::ApplyError::System(e)) => Err(e),
        }
    }
}

/// `periodic jobs pause|resume <id>`: flip a job's `enabled` flag through the
/// dual-mode dispatch and report the result.
fn run_jobs_set_enabled(args: &cli::JobMutateArgs, enable: bool) -> anyhow::Result<ExitCode> {
    let mutation = if enable {
        config_mutation::Mutation::Resume(args.id.clone())
    } else {
        config_mutation::Mutation::Pause(args.id.clone())
    };
    match dispatch_mutation(&mutation)? {
        config_mutation::Outcome::Applied => {
            // `state` uses the frozen active/disabled vocabulary (decision 0002),
            // matching `jobs list`/`status` so the field means the same everywhere.
            let (verb, state) = if enable {
                ("Resumed", "active")
            } else {
                ("Paused", "disabled")
            };
            match args.format {
                cli::OutputFormat::Json => println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &serde_json::json!({ "id": args.id, "state": state })
                    )
                    .expect("mutation result serializes")
                ),
                cli::OutputFormat::Human => println!("{verb}: {}", args.id),
            }
            Ok(ExitCode::SUCCESS)
        }
        config_mutation::Outcome::Refused(msg) => {
            eprintln!("error: {msg}");
            Ok(ExitCode::from(1))
        }
    }
}

/// `periodic jobs add …`: generate a job block from flags and append it through
/// the dual-mode dispatch. Flag-driven only — scaffolding into `$EDITOR` lands
/// with `jobs edit` (PDC-85). The generated block is validated before it is
/// persisted, so an invalid schedule is refused without touching the file.
fn run_jobs_add(args: &cli::JobsAddArgs) -> anyhow::Result<ExitCode> {
    if args.command.is_none() {
        eprintln!(
            "error: --command is required (scaffolding into $EDITOR is not yet available; use `jobs edit`)"
        );
        return Ok(ExitCode::from(1));
    }
    if args.every.is_none() && args.cron.is_none() {
        eprintln!("error: a schedule is required: pass --every (e.g. 15m, day) or --cron");
        return Ok(ExitCode::from(1));
    }
    let Some(id) = job_block::derive_id(args) else {
        eprintln!("error: could not derive a job id from --title or --command; pass --id");
        return Ok(ExitCode::from(1));
    };
    // Reject an invalid id before generating any YAML — this both gives a clear
    // error and closes off YAML-structure injection via a crafted --id.
    if !validation::is_kebab(&id) {
        eprintln!("error: invalid job id {id:?}: job ids must be kebab-case (a-z, 0-9, -)");
        return Ok(ExitCode::from(1));
    }

    let block = job_block::build_block(&id, args);
    match dispatch_mutation(&config_mutation::Mutation::Add(block))? {
        config_mutation::Outcome::Applied => {
            match args.format {
                cli::OutputFormat::Json => println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({ "id": id, "added": true }))
                        .expect("mutation result serializes")
                ),
                cli::OutputFormat::Human => println!("Added: {id}"),
            }
            Ok(ExitCode::SUCCESS)
        }
        config_mutation::Outcome::Refused(msg) => {
            eprintln!("error: {msg}");
            Ok(ExitCode::from(1))
        }
    }
}

/// `periodic jobs remove <id>`: delete a job's block from the config through the
/// dual-mode dispatch. Invoking the command is the confirmation (no prompt); the
/// edit is surgical, validated, and atomic. Run history is unaffected.
fn run_jobs_remove(args: &cli::JobMutateArgs) -> anyhow::Result<ExitCode> {
    match dispatch_mutation(&config_mutation::Mutation::Remove(args.id.clone()))? {
        config_mutation::Outcome::Applied => {
            match args.format {
                cli::OutputFormat::Json => println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &serde_json::json!({ "id": args.id, "removed": true })
                    )
                    .expect("mutation result serializes")
                ),
                cli::OutputFormat::Human => println!("Removed: {}", args.id),
            }
            Ok(ExitCode::SUCCESS)
        }
        config_mutation::Outcome::Refused(msg) => {
            eprintln!("error: {msg}");
            Ok(ExitCode::from(1))
        }
    }
}

/// Forward terminal SIGINT to the executor: set CANCEL so the run's wait loop
/// kills the child's process group instead of orphaning it.
#[cfg(unix)]
fn install_sigint_forwarder() {
    use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};
    extern "C" fn handle(_sig: i32) {
        executor::CANCEL.store(true, std::sync::atomic::Ordering::SeqCst);
    }
    let action = SigAction::new(
        SigHandler::Handler(handle),
        SaFlags::empty(),
        SigSet::empty(),
    );
    // SAFETY: the handler only stores to an AtomicBool (async-signal-safe).
    unsafe {
        let _ = sigaction(Signal::SIGINT, &action);
    }
}

#[cfg(not(unix))]
fn install_sigint_forwarder() {}

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
