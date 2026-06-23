//! Command-line surface: clap definitions, command parsing, table/JSON
//! rendering, and the daemon IPC client (with direct-mode behavior when the
//! daemon is absent). The CLI never drives the scheduler or executor directly.
//!
//! This is the command *skeleton*: the v1 group/subcommand tree from
//! `periodic-cli-design`, with every command stubbed. Per-command flags, the
//! `--format` JSON contract, and norn's wholesale help renderer arrive in later
//! phases as the commands that consume them land.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "periodic", version, propagate_version = true)]
#[command(
    about = "User-space recurring job scheduler",
    arg_required_else_help = true
)]
pub(crate) struct Cli {
    #[arg(
        long,
        global = true,
        help_heading = "Global options",
        help = "Include full diagnostic detail in output"
    )]
    pub(crate) verbose: bool,

    #[arg(
        long,
        global = true,
        help_heading = "Global options",
        help = "Suppress non-essential output"
    )]
    pub(crate) quiet: bool,

    #[command(subcommand)]
    pub(crate) command: Command,
}

/// Top-level v1 command groups (see `periodic-cli-design`).
#[derive(Debug, Subcommand)]
pub(crate) enum Command {
    /// Manage the periodic daemon.
    #[command(subcommand)]
    Daemon(DaemonCommand),
    /// Manage scheduled jobs.
    #[command(subcommand)]
    Jobs(JobsCommand),
    /// Show job run logs.
    Logs(LogsArgs),
    /// Validate the configuration without applying it.
    Validate(ValidateArgs),
    /// Reload the configuration (validated).
    Reload,
    /// Diagnose daemon, config, and runtime health.
    Doctor,
    /// Generate shell completions.
    Completion,
    /// Update periodic in place.
    SelfUpdate(SelfUpdateArgs),
    /// Manage the daemon under the OS service manager.
    #[command(subcommand)]
    Service(ServiceCommand),
}

/// `periodic service …`: register the daemon with the per-user service manager
/// (launchd on macOS, systemd --user on Linux).
#[derive(Debug, Subcommand)]
pub(crate) enum ServiceCommand {
    /// Install and enable the service so the daemon runs at login.
    Install,
    /// Stop, disable, and remove the service.
    Uninstall,
    /// Start the installed service.
    Start,
    /// Stop the running service.
    Stop,
    /// Show the service's status.
    Status,
}

/// `periodic self-update …`
#[derive(Debug, Args)]
pub(crate) struct SelfUpdateArgs {
    /// Update to the latest prerelease (the `-next` channel) instead of stable.
    #[arg(long)]
    pub(crate) next: bool,
    /// Update (or downgrade) to a specific release tag, e.g. `v0.2.0`.
    #[arg(long, value_name = "TAG", conflicts_with = "next")]
    pub(crate) tag: Option<String>,
}

/// `periodic daemon …`
#[derive(Debug, Subcommand)]
pub(crate) enum DaemonCommand {
    /// Start the daemon.
    Start {
        /// Run the scheduler loop in the foreground (the default).
        #[arg(long)]
        foreground: bool,
        /// Re-spawn detached in the background and return the child pid.
        #[arg(long, conflicts_with = "foreground")]
        detach: bool,
    },
    /// Stop the daemon.
    Stop {
        /// Send SIGKILL instead of SIGTERM.
        #[arg(long)]
        force: bool,
    },
    /// Show daemon status.
    Status {
        /// Output format.
        #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
        format: OutputFormat,
    },
}

/// `periodic jobs …`
#[derive(Debug, Subcommand)]
pub(crate) enum JobsCommand {
    /// Add a job.
    Add,
    /// List jobs.
    List(JobsListArgs),
    /// Show one job's status.
    Status(JobsStatusArgs),
    /// Run a job now.
    Run(JobsRunArgs),
    /// Pause a job.
    Pause(JobMutateArgs),
    /// Resume a paused job.
    Resume(JobMutateArgs),
    /// Remove a job.
    Remove(JobMutateArgs),
    /// Edit a job.
    Edit,
    /// Show a job's run history.
    History(JobsHistoryArgs),
}

/// Arguments for `periodic jobs list`.
#[derive(Debug, Args)]
pub(crate) struct JobsListArgs {
    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
    pub(crate) format: OutputFormat,
}

/// Arguments for `periodic jobs pause|resume <id>`.
#[derive(Debug, Args)]
pub(crate) struct JobMutateArgs {
    /// Job id to pause or resume.
    pub(crate) id: String,
    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
    pub(crate) format: OutputFormat,
}

/// Arguments for `periodic jobs status <id>`.
#[derive(Debug, Args)]
pub(crate) struct JobsStatusArgs {
    /// Job id to show.
    pub(crate) id: String,
    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
    pub(crate) format: OutputFormat,
}

/// Arguments for `periodic jobs run <id>`.
#[derive(Debug, Args)]
pub(crate) struct JobsRunArgs {
    /// Job id to run now.
    pub(crate) id: String,
    #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
    pub(crate) format: OutputFormat,
}

/// Arguments for `periodic jobs history <id>`.
#[derive(Debug, Args)]
pub(crate) struct JobsHistoryArgs {
    /// Job id whose runs to show.
    pub(crate) id: String,
    /// Maximum number of runs to show (most recent first).
    #[arg(long, default_value_t = 20)]
    pub(crate) limit: i64,
    #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
    pub(crate) format: OutputFormat,
}

/// Arguments for `periodic logs <id>`.
#[derive(Debug, Args)]
pub(crate) struct LogsArgs {
    /// Job id whose output to show.
    pub(crate) id: String,
    /// Restrict to a single run.
    #[arg(long, value_name = "RUN-ID")]
    pub(crate) run: Option<String>,
    #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
    pub(crate) format: OutputFormat,
}

/// Arguments for `periodic validate`.
#[derive(Debug, Args)]
pub(crate) struct ValidateArgs {
    /// Config file to validate (default: ~/.config/periodic/periodic.config.yaml).
    pub(crate) path: Option<PathBuf>,
    /// Output format.
    #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
    pub(crate) format: OutputFormat,
}

/// Output format for `periodic validate`.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub(crate) enum OutputFormat {
    Human,
    Json,
}

pub(crate) fn default_config_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(".config/periodic/periodic.config.yaml")
}

/// Parse the command line. Thin wrapper so `main` stays declarative.
pub(crate) fn parse() -> Cli {
    Cli::parse()
}
