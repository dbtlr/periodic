//! Daemon orchestration: lifecycle, startup/shutdown, and wiring scheduler ↔
//! executor ↔ state ↔ IPC, including shutdown draining.
//!
//! Per ADR 0006/0007 this is **synchronous**: three concerns run as OS threads
//! coordinated by an `mpsc` control channel and a shared `Arc<AtomicBool>` stop
//! flag. The scheduler loop (this module's main thread) owns the
//! [`ScheduleTable`]; the IPC server runs [`ipc::serve`] on its own thread; a
//! signal handler flips the stop flag on SIGTERM/SIGINT. No async runtime.

use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread;
use std::time::Duration;

use chrono::{DateTime, Utc};
use rusqlite::Connection;
use serde_json::json;

use crate::cli::{DaemonCommand, OutputFormat};
use crate::scheduler::ScheduleTable;
use crate::state::{self, DaemonStatus};
use crate::{config, validation};

/// Cap on the scheduler's idle sleep, so the heartbeat stays fresh even when no
/// job is due for a long time.
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// Grace period to let in-flight run threads finish on shutdown before their
/// cancel flags are set and the daemon exits.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(10);

/// A daemon that claims "running" but whose heartbeat is older than this is
/// treated as crashed — a new daemon may then claim liveness, and `stop`/`status`
/// report it as not-running.
const DAEMON_STALE_AFTER: chrono::Duration = chrono::Duration::seconds(90);

/// Control-channel events delivered to the scheduler loop from other threads.
enum ControlEvent {
    /// Re-read, validate, and rebuild the schedule from config on disk (IPC).
    Reload,
    /// Wake the loop so it re-checks the stop flag (signal watcher). The loop
    /// blocks in `recv_timeout`; setting the stop flag alone would not interrupt
    /// it, so the watcher also sends this to make shutdown prompt.
    Wake,
}

/// Route `periodic daemon …` to its handler.
pub(crate) fn run(cmd: DaemonCommand) -> anyhow::Result<ExitCode> {
    match cmd {
        DaemonCommand::Start { foreground, detach } => start(foreground, detach),
        DaemonCommand::Stop { force } => stop(force),
        DaemonCommand::Status { format } => status(format),
    }
}

// ─── start ───────────────────────────────────────────────────────────────────

/// `periodic daemon start`. Without `--detach` (or with `--foreground`) this runs
/// the scheduler loop in the foreground until signalled. With `--detach` it
/// re-spawns itself detached and returns immediately after printing the child pid.
fn start(_foreground: bool, detach: bool) -> anyhow::Result<ExitCode> {
    // Refuse to start a second daemon when a live one already holds liveness.
    let db_path = state::default_db_path();
    let conn = state::open(&db_path)?;
    if let Some(status) = state::read_daemon_status(&conn)? {
        let live = status.state == "running"
            && !state::daemon_is_stale(&status, Utc::now(), DAEMON_STALE_AFTER);
        if live {
            eprintln!("error: daemon already running (pid {})", status.pid);
            return Ok(ExitCode::from(1));
        }
    }
    drop(conn);

    if detach {
        return spawn_detached();
    }
    run_foreground(&db_path)
}

/// Re-exec self as a detached background process running the foreground loop.
/// Prints the child pid and exits 0; the child outlives this parent.
#[cfg(unix)]
fn spawn_detached() -> anyhow::Result<ExitCode> {
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    let exe = std::env::current_exe()?;
    let mut cmd = Command::new(exe);
    cmd.args(["daemon", "start", "--foreground"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: setsid is async-signal-safe and the only post-fork action; it
    // detaches the child into its own session so it survives this parent exiting.
    unsafe {
        cmd.pre_exec(|| {
            nix::unistd::setsid()
                .map(|_| ())
                .map_err(std::io::Error::from)
        });
    }
    let child = cmd.spawn()?;
    println!("daemon started (pid {})", child.id());
    Ok(ExitCode::SUCCESS)
}

#[cfg(not(unix))]
fn spawn_detached() -> anyhow::Result<ExitCode> {
    anyhow::bail!("`periodic daemon start --detach` is only supported on unix")
}

/// The foreground startup sequence and scheduler loop.
fn run_foreground(db_path: &std::path::Path) -> anyhow::Result<ExitCode> {
    let conn = state::open(db_path)?;
    let effective = load_validated_config()?;

    let now = Utc::now();
    state::reconcile(&conn, &effective, now)?;
    let interrupted = state::recover_interrupted_runs(&conn, now)?;
    if interrupted > 0 {
        tracing::info!(
            count = interrupted,
            "interrupted runs recovered from prior daemon"
        );
    }
    let pid = std::process::id() as i32;
    state::record_daemon_started(&conn, pid, now)?;
    tracing::info!(pid, jobs = effective.jobs.len(), "daemon started");

    let stop = Arc::new(AtomicBool::new(false));
    let (control_tx, control_rx) = mpsc::channel::<ControlEvent>();
    install_signal_handlers(&stop, control_tx.clone());

    // IPC server thread: answers status/reload, observes the same stop flag.
    let ipc_stop = Arc::clone(&stop);
    let ipc_tx = control_tx.clone();
    let ipc_pid = pid;
    let ipc_handle = thread::spawn(move || {
        let path = crate::ipc::socket_path();
        let handler = move |req: crate::ipc::Request| ipc_handler(req, &ipc_tx, ipc_pid);
        if let Err(e) = crate::ipc::serve(&path, &ipc_stop, handler) {
            tracing::warn!(error = %e, "ipc server exited with error");
        }
    });

    let table = ScheduleTable::build(&effective, now);
    scheduler_loop(&conn, table, &stop, &control_rx);

    // Graceful shutdown: stop the IPC server and join it.
    stop.store(true, Ordering::SeqCst);
    let _ = ipc_handle.join();
    state::record_daemon_state(&conn, "stopped", Utc::now())?;
    tracing::info!("daemon stopped");
    Ok(ExitCode::SUCCESS)
}

/// The scheduler loop: heartbeat, dispatch the due set to run threads, then sleep
/// until the next wake or a control event. Returns when `stop` is set or the
/// control channel disconnects. Run threads are tracked so shutdown can drain them.
fn scheduler_loop(
    conn: &Connection,
    mut table: ScheduleTable,
    stop: &AtomicBool,
    control_rx: &mpsc::Receiver<ControlEvent>,
) {
    let mut runs: Vec<RunHandle> = Vec::new();

    while !stop.load(Ordering::SeqCst) {
        let now = Utc::now();
        if let Err(e) = state::record_daemon_heartbeat(conn, now) {
            tracing::warn!(error = %e, "heartbeat write failed");
        }

        runs.retain(|r| !r.finished());
        let active: std::collections::HashSet<String> =
            runs.iter().map(|r| r.job_id.clone()).collect();
        let due = table.pop_due(now);
        for run in dispatch_due(conn, due, &active, now) {
            runs.push(run);
        }

        let dur = sleep_until(table.next_wake(), Utc::now());
        match control_rx.recv_timeout(dur) {
            Ok(ControlEvent::Reload) => {
                tracing::info!("reload requested");
                match reload(conn) {
                    // PDC-75 fills richer reload validation/diffing; a reconcile +
                    // table rebuild from freshly-validated config is the baseline.
                    Ok(new_table) => table = new_table,
                    Err(e) => tracing::warn!(error = %e, "reload failed; keeping current schedule"),
                }
            }
            // Wake-up to re-check the stop flag at the top of the loop.
            Ok(ControlEvent::Wake) => {}
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    // Graceful shutdown: stop dispatching, mark stopping, drain in-flight runs.
    let _ = state::record_daemon_state(conn, "stopping", Utc::now());
    tracing::info!("daemon stopping");
    drain_runs(runs);
}

/// Compute the scheduler's next sleep duration: from now until `next_wake`,
/// clamped to `[0, HEARTBEAT_INTERVAL]` so the heartbeat never goes stale.
fn sleep_until(next_wake: Option<DateTime<Utc>>, now: DateTime<Utc>) -> Duration {
    match next_wake {
        Some(wake) => {
            let delta = wake - now;
            let secs = delta.num_milliseconds().max(0) as u64;
            Duration::from_millis(secs).min(HEARTBEAT_INTERVAL)
        }
        None => HEARTBEAT_INTERVAL,
    }
}

/// A handle on a dispatched run thread plus its per-run cancel flag, so shutdown
/// can request cancellation and join.
struct RunHandle {
    job_id: String,
    handle: thread::JoinHandle<()>,
    cancel: Arc<AtomicBool>,
    done: Arc<AtomicBool>,
}

impl RunHandle {
    fn finished(&self) -> bool {
        self.done.load(Ordering::SeqCst)
    }
}

/// Claim and dispatch each due occurrence. `create_run` with the occurrence_key is
/// the dedupe gate AND the real run row: a restart that re-derives the same key sees
/// `false` (the row already exists) and skips re-dispatch; a freshly-claimed `pending`
/// row is handed to a thread that opens its **own** connection (Connection isn't
/// `Sync`) and calls [`crate::executor::execute_run`] on that same row. One row per
/// scheduled fire, with the correct `scheduled` trigger. Returns the spawned handles.
fn dispatch_due(
    conn: &Connection,
    due: Vec<crate::scheduler::DueRun>,
    active: &std::collections::HashSet<String>,
    now: DateTime<Utc>,
) -> Vec<RunHandle> {
    let mut handles = Vec::new();
    for d in due {
        let Some(job_id) = d.job.id.as_deref() else {
            continue;
        };
        let config_hash = config::job_config_hash(&d.job);
        let run_id = format!("{}-{}", job_id, now.timestamp_micros());
        let claimed = match state::create_run(
            conn,
            &run_id,
            job_id,
            &config_hash,
            "scheduled",
            Some(&d.occurrence_key),
            now,
        ) {
            Ok(claimed) => claimed,
            Err(e) => {
                tracing::warn!(error = %e, job = job_id, "failed to claim occurrence");
                continue;
            }
        };
        if !claimed {
            // Already ran (restart/reload dup); skip.
            continue;
        }
        // Overlap policy = skip (the only v1 policy): if a prior run of this job is
        // still in flight, record this occurrence as skipped_overlap (decision B —
        // non-executing outcomes are real run rows) and do not start a second run.
        if active.contains(job_id) {
            if let Err(e) = state::finish_run(conn, &run_id, "skipped_overlap", None, None, now) {
                tracing::warn!(error = %e, job = job_id, "failed to record skipped_overlap");
            }
            tracing::info!(job = job_id, occurrence = %d.occurrence_key, "skipped (overlap)");
            continue;
        }
        tracing::info!(job = job_id, occurrence = %d.occurrence_key, "dispatching scheduled run");
        handles.push(spawn_run(d.job, run_id));
    }
    handles
}

/// Spawn a run thread that executes the already-created run row `run_id`: it opens
/// its own state connection (Connection isn't `Sync`) and runs the executor on it.
fn spawn_run(job: crate::config::EffectiveJob, run_id: String) -> RunHandle {
    let cancel = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let run_job_id = job.id.as_deref().unwrap_or("").to_owned();
    let thread_cancel = Arc::clone(&cancel);
    let thread_done = Arc::clone(&done);
    let handle = thread::spawn(move || {
        let job_id = job.id.as_deref().unwrap_or("").to_owned();
        let result = (|| {
            let conn = state::open(&state::default_db_path())?;
            crate::executor::execute_run(
                &conn,
                &state::default_logs_dir(),
                &job,
                &run_id,
                Utc::now(),
                &thread_cancel,
            )
        })();
        if let Err(e) = result {
            tracing::warn!(error = %e, job = job_id, "scheduled run errored");
        }
        thread_done.store(true, Ordering::SeqCst);
    });
    RunHandle {
        job_id: run_job_id,
        handle,
        cancel,
        done,
    }
}

/// Drain in-flight run threads on shutdown: wait up to the grace period, then
/// cancel any stragglers and join them all.
fn drain_runs(runs: Vec<RunHandle>) {
    if runs.is_empty() {
        return;
    }
    tracing::info!(count = runs.len(), "waiting for in-flight runs to finish");
    let deadline = std::time::Instant::now() + SHUTDOWN_GRACE;
    loop {
        if runs.iter().all(|r| r.finished()) || std::time::Instant::now() >= deadline {
            break;
        }
        thread::sleep(Duration::from_millis(50));
    }
    for run in &runs {
        if !run.finished() {
            run.cancel.store(true, Ordering::SeqCst);
        }
    }
    for run in runs {
        let _ = run.handle.join();
    }
}

/// Re-read config, reconcile the projection, and rebuild the schedule table.
fn reload(conn: &Connection) -> anyhow::Result<ScheduleTable> {
    let effective = load_validated_config()?;
    let now = Utc::now();
    state::reconcile(conn, &effective, now)?;
    Ok(ScheduleTable::build(&effective, now))
}

/// Load, validate, and normalize the config on disk (mirrors `project_state`).
fn load_validated_config() -> anyhow::Result<config::EffectiveConfig> {
    let path = crate::cli::default_config_path();
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
    Ok(config::normalize(&raw))
}

// ─── IPC handler ───────────────────────────────────────────────────────────────

/// Answer one IPC request. `daemon.status` returns a liveness snapshot;
/// `daemon.reload` queues a reload on the control channel. Unknown methods error.
fn ipc_handler(
    req: crate::ipc::Request,
    control_tx: &Sender<ControlEvent>,
    pid: i32,
) -> crate::ipc::Response {
    use crate::ipc::Response;
    match req.method.as_str() {
        "daemon.status" => Response::ok(req.id, json!({ "state": "running", "pid": pid })),
        "daemon.reload" => match control_tx.send(ControlEvent::Reload) {
            Ok(()) => Response::ok(req.id, json!({ "reloading": true })),
            Err(_) => Response::err(req.id, "shutting_down", "daemon is shutting down"),
        },
        other => Response::err(req.id, "unknown_method", format!("unknown method: {other}")),
    }
}

// ─── signal handling ───────────────────────────────────────────────────────────

/// Shared stop flag toggled by the SIGTERM/SIGINT handler. A static is required
/// because the C signal handler cannot capture state.
static SIGNAL_STOP: AtomicBool = AtomicBool::new(false);

/// Install a SIGTERM + SIGINT handler that flips the stop flag. The scheduler
/// loop and IPC server both observe it and exit. A watcher thread bridges the
/// async-signal-safe static into the daemon's `Arc<AtomicBool>` stop flag.
#[cfg(unix)]
fn install_signal_handlers(stop: &Arc<AtomicBool>, control_tx: Sender<ControlEvent>) {
    use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, sigaction};
    extern "C" fn handle(_sig: i32) {
        SIGNAL_STOP.store(true, Ordering::SeqCst);
    }
    let action = SigAction::new(
        SigHandler::Handler(handle),
        SaFlags::empty(),
        SigSet::empty(),
    );
    // SAFETY: the handler only stores to an AtomicBool (async-signal-safe).
    unsafe {
        let _ = sigaction(Signal::SIGTERM, &action);
        let _ = sigaction(Signal::SIGINT, &action);
    }
    let stop = Arc::clone(stop);
    thread::spawn(move || {
        while !SIGNAL_STOP.load(Ordering::SeqCst) {
            thread::sleep(Duration::from_millis(100));
        }
        stop.store(true, Ordering::SeqCst);
        // Wake the scheduler loop, which is otherwise blocked in `recv_timeout`.
        let _ = control_tx.send(ControlEvent::Wake);
    });
}

#[cfg(not(unix))]
fn install_signal_handlers(_stop: &Arc<AtomicBool>, _control_tx: Sender<ControlEvent>) {}

// ─── stop ──────────────────────────────────────────────────────────────────────

/// `periodic daemon stop`. Idempotent: a missing or already-stopped daemon exits
/// 0 with a note. Otherwise SIGTERM (or SIGKILL with `--force`) the recorded pid.
#[cfg(unix)]
fn stop(force: bool) -> anyhow::Result<ExitCode> {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;

    let conn = state::open(&state::default_db_path())?;
    let Some(status) = state::read_daemon_status(&conn)? else {
        println!("daemon not running");
        return Ok(ExitCode::SUCCESS);
    };
    let live = status.state == "running"
        && !state::daemon_is_stale(&status, Utc::now(), DAEMON_STALE_AFTER);
    if !live {
        println!("daemon not running");
        return Ok(ExitCode::SUCCESS);
    }
    let sig = if force {
        Signal::SIGKILL
    } else {
        Signal::SIGTERM
    };
    match kill(Pid::from_raw(status.pid), sig) {
        Ok(()) => {
            let verb = if force { "killed" } else { "signalled to stop" };
            println!("daemon {verb} (pid {})", status.pid);
            Ok(ExitCode::SUCCESS)
        }
        Err(nix::errno::Errno::ESRCH) => {
            // The pid is gone (crashed without cleanup); treat as not running.
            println!("daemon not running");
            Ok(ExitCode::SUCCESS)
        }
        Err(e) => {
            eprintln!("error: failed to signal daemon (pid {}): {e}", status.pid);
            Ok(ExitCode::from(1))
        }
    }
}

#[cfg(not(unix))]
fn stop(_force: bool) -> anyhow::Result<ExitCode> {
    anyhow::bail!("`periodic daemon stop` is only supported on unix")
}

// ─── status ────────────────────────────────────────────────────────────────────

/// `periodic daemon status`. Renders daemon liveness from the recorded snapshot.
fn status(format: OutputFormat) -> anyhow::Result<ExitCode> {
    let conn = state::open(&state::default_db_path())?;
    let snapshot = state::read_daemon_status(&conn)?;
    let view = DaemonView::from_status(snapshot.as_ref(), Utc::now());
    let rendered = match format {
        OutputFormat::Json => view.render_json(),
        OutputFormat::Human => view.render_human(),
    };
    print!("{rendered}");
    Ok(ExitCode::SUCCESS)
}

/// A rendered view of daemon liveness, computed purely over a [`DaemonStatus`]
/// snapshot and an injected `now` so the four states (running / stale / stopped /
/// none) are unit-testable without a live daemon.
struct DaemonView {
    state: String,
    pid: Option<i32>,
    running: bool,
}

impl DaemonView {
    fn from_status(status: Option<&DaemonStatus>, now: DateTime<Utc>) -> Self {
        match status {
            None => DaemonView {
                state: "not running".to_owned(),
                pid: None,
                running: false,
            },
            Some(s) if s.state == "running" => {
                if state::daemon_is_stale(s, now, DAEMON_STALE_AFTER) {
                    DaemonView {
                        state: "not responding".to_owned(),
                        pid: Some(s.pid),
                        running: false,
                    }
                } else {
                    DaemonView {
                        state: "running".to_owned(),
                        pid: Some(s.pid),
                        running: true,
                    }
                }
            }
            Some(s) => DaemonView {
                state: s.state.clone(),
                pid: Some(s.pid),
                running: false,
            },
        }
    }

    fn render_human(&self) -> String {
        match self.pid {
            Some(pid) => format!("daemon: {} (pid {pid})\n", self.state),
            None => format!("daemon: {}\n", self.state),
        }
    }

    fn render_json(&self) -> String {
        let obj = json!({
            "daemon": {
                "state": self.state,
                "pid": self.pid,
                "running": self.running,
            }
        });
        format!(
            "{}\n",
            serde_json::to_string_pretty(&obj).expect("daemon status serializes")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(s: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(s, 0).unwrap()
    }

    fn status(state: &str, heartbeat: DateTime<Utc>, pid: i32) -> DaemonStatus {
        DaemonStatus {
            pid,
            state: state.to_owned(),
            heartbeat,
        }
    }

    // ── status rendering ──────────────────────────────────────────────────────

    #[test]
    fn view_none_is_not_running() {
        let v = DaemonView::from_status(None, at(1000));
        assert!(!v.running);
        assert_eq!(v.pid, None);
        assert!(v.render_human().contains("not running"));
        assert!(v.render_json().contains("\"running\": false"));
    }

    #[test]
    fn view_fresh_running_reports_pid() {
        let s = status("running", at(1000), 4242);
        let v = DaemonView::from_status(Some(&s), at(1030));
        assert!(v.running);
        assert_eq!(v.pid, Some(4242));
        assert!(v.render_human().contains("running (pid 4242)"));
        assert!(v.render_json().contains("\"running\": true"));
        assert!(v.render_json().contains("\"pid\": 4242"));
    }

    #[test]
    fn view_stale_running_is_not_responding_and_not_running() {
        // heartbeat 1000, now 1000 + 91s > 90s stale window.
        let s = status("running", at(1000), 7);
        let v = DaemonView::from_status(Some(&s), at(1091));
        assert!(!v.running, "a stale daemon is reported as not running");
        assert!(v.render_human().contains("not responding"));
        assert!(v.render_json().contains("\"running\": false"));
    }

    #[test]
    fn view_stopped_is_not_running() {
        let s = status("stopped", at(1000), 9);
        let v = DaemonView::from_status(Some(&s), at(9999));
        assert!(!v.running);
        assert!(v.render_human().contains("stopped"));
        assert_eq!(v.pid, Some(9));
    }

    // ── sleep clamp ───────────────────────────────────────────────────────────

    #[test]
    fn sleep_until_clamps_far_wake_to_heartbeat_interval() {
        let now = at(1000);
        let far = at(1000 + 600); // 10 minutes out
        assert_eq!(sleep_until(Some(far), now), HEARTBEAT_INTERVAL);
    }

    #[test]
    fn sleep_until_none_is_heartbeat_interval() {
        assert_eq!(sleep_until(None, at(1000)), HEARTBEAT_INTERVAL);
    }

    #[test]
    fn sleep_until_past_wake_is_zero() {
        let now = at(2000);
        let past = at(1000);
        assert_eq!(sleep_until(Some(past), now), Duration::ZERO);
    }

    #[test]
    fn sleep_until_near_wake_is_the_delta() {
        let now = at(1000);
        let soon = at(1005);
        assert_eq!(sleep_until(Some(soon), now), Duration::from_secs(5));
    }

    // ── dispatch dedupe ───────────────────────────────────────────────────────

    fn temp_conn() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let conn = state::open(&dir.path().join("periodic.db")).unwrap();
        (dir, conn)
    }

    fn minute_job(id: &str) -> crate::config::EffectiveJob {
        crate::config::EffectiveJob {
            id: Some(id.into()),
            title: None,
            enabled: true,
            schedule: crate::config::NormalizedSchedule::MinuteAligned { every_minutes: 15 },
            command: "true".into(),
            args: vec![],
            cwd: None,
            timeout_secs: None,
            timezone: Some("UTC".into()),
            overlap_policy: crate::config::OverlapPolicy::Skip,
            missed_run_policy: crate::config::MissedRunPolicy::Skip,
            max_retries: 0,
            tags: vec![],
        }
    }

    fn due(job: crate::config::EffectiveJob, key: &str) -> crate::scheduler::DueRun {
        crate::scheduler::DueRun {
            job,
            occurrence_key: key.into(),
            scheduled_for: at(1000),
        }
    }

    #[test]
    fn fresh_occurrence_claims_and_dispatches() {
        let (_d, conn) = temp_conn();
        let handles = dispatch_due(
            &conn,
            vec![due(minute_job("a"), "a:minute:t")],
            &std::collections::HashSet::new(),
            at(1000),
        );
        assert_eq!(
            handles.len(),
            1,
            "a fresh occurrence_key dispatches one run"
        );
        for h in handles {
            let _ = h.handle.join();
        }
        // Exactly one claim row exists for the occurrence.
        let n: i64 = conn
            .query_row(
                "select count(*) from runs where occurrence_key = 'a:minute:t'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn duplicate_occurrence_does_not_dispatch() {
        let (_d, conn) = temp_conn();
        // Pre-claim the occurrence (simulating a run before a restart).
        state::create_run(
            &conn,
            "pre",
            "a",
            "h",
            "scheduled",
            Some("a:minute:t"),
            at(900),
        )
        .unwrap();
        let handles = dispatch_due(
            &conn,
            vec![due(minute_job("a"), "a:minute:t")],
            &std::collections::HashSet::new(),
            at(1000),
        );
        assert!(
            handles.is_empty(),
            "a duplicate occurrence_key (create_run=false) must not dispatch"
        );
    }

    #[test]
    fn active_job_occurrence_is_recorded_skipped_overlap() {
        let (_d, conn) = temp_conn();
        let mut active = std::collections::HashSet::new();
        active.insert("a".to_string());
        let handles = dispatch_due(
            &conn,
            vec![due(minute_job("a"), "a:minute:t2")],
            &active,
            at(2000),
        );
        assert!(
            handles.is_empty(),
            "an in-flight job does not start a second run"
        );
        let status: String = conn
            .query_row(
                "select status from runs where occurrence_key = 'a:minute:t2'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            status, "skipped_overlap",
            "the skipped occurrence is recorded"
        );
    }
}
