//! Process execution: spawning, process groups, timeouts, retries,
//! cancellation, output streaming, and shutdown draining. Consumes run intents
//! and emits events.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use crate::config::EffectiveJob;
use crate::error::Result;
use crate::events::{self, EventKind};
use crate::logs::{DailyLogWriter, LogRecord};
use crate::state;

/// Grace between SIGTERM and SIGKILL on timeout/cancel (config knob deferred).
const KILL_GRACE: Duration = Duration::from_secs(5);

/// Set true by the SIGINT handler installed in `main`; the wait loop forwards a
/// kill to the run's process group when it sees this.
pub(crate) static CANCEL: AtomicBool = AtomicBool::new(false);

/// Terminal outcome of a run (manual subset of run statuses).
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum RunStatus {
    Success,
    Failed,
    Timeout,
    Cancelled,
}

impl RunStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            RunStatus::Success => "success",
            RunStatus::Failed => "failed",
            RunStatus::Timeout => "timeout",
            RunStatus::Cancelled => "cancelled",
        }
    }
}

/// What `jobs run` reports and renders.
#[derive(Debug)]
pub(crate) struct RunOutcome {
    pub(crate) id: String,
    pub(crate) job_id: String,
    pub(crate) status: RunStatus,
    pub(crate) started_at: String,
    pub(crate) finished_at: String,
    pub(crate) exit_code: Option<i32>,
    pub(crate) attempts: u32,
}

/// How a single attempt ended.
#[derive(Clone, Copy)]
enum AttemptResult {
    Exited(i32),
    Timeout,
    Cancelled,
}

/// Execute `job` once (single attempt — the retry loop wraps this in Task 8),
/// recording run/attempt/event rows and tee-ing output to the terminal + JSONL.
pub(crate) fn run_job(
    conn: &rusqlite::Connection,
    logs_dir: &Path,
    job: &EffectiveJob,
    now: DateTime<Utc>,
    cancel: &AtomicBool,
) -> Result<RunOutcome> {
    let job_id = job.id.as_deref().unwrap_or("");
    let run_id = format!("{job_id}-{}", now.timestamp_micros());
    let config_hash = crate::config::job_config_hash(job);

    state::create_run(conn, &run_id, job_id, &config_hash, "manual", now)?;
    state::mark_run_running(conn, &run_id, now)?;
    events::emit(
        conn,
        EventKind::RunStarted,
        job_id,
        &run_id,
        None,
        "run started",
        now,
    )?;

    let writer = Arc::new(Mutex::new(DailyLogWriter::new(logs_dir.to_path_buf())));

    let mut attempt_number: i64 = 1;
    let (status, exit_code, attempts) = loop {
        let attempt_id = format!("{run_id}-{attempt_number}");
        let result = run_attempt(
            conn,
            job,
            job_id,
            &run_id,
            &attempt_id,
            attempt_number,
            &writer,
            now,
            cancel,
        )?;
        match result {
            AttemptResult::Exited(0) => break (RunStatus::Success, Some(0), attempt_number as u32),
            AttemptResult::Exited(c) => {
                // Retry only normal non-zero exits, up to max_retries (immediate, no backoff).
                if (attempt_number as u32) <= job.max_retries {
                    attempt_number += 1;
                    continue;
                }
                break (RunStatus::Failed, Some(c), attempt_number as u32);
            }
            AttemptResult::Timeout => break (RunStatus::Timeout, None, attempt_number as u32), // terminal
            AttemptResult::Cancelled => break (RunStatus::Cancelled, None, attempt_number as u32), // terminal
        }
    };

    let run_finished = Utc::now();
    finalize(conn, job_id, &run_id, status, exit_code, run_finished)?;
    Ok(RunOutcome {
        id: run_id,
        job_id: job_id.to_owned(),
        status,
        started_at: now.to_rfc3339(),
        finished_at: run_finished.to_rfc3339(),
        exit_code,
        attempts,
    })
}

/// Persist terminal run state + outcome columns + a closing event.
fn finalize(
    conn: &rusqlite::Connection,
    job_id: &str,
    run_id: &str,
    status: RunStatus,
    exit_code: Option<i32>,
    run_finished: DateTime<Utc>,
) -> Result<()> {
    state::finish_run(conn, run_id, status.as_str(), exit_code, None, run_finished)?;
    state::update_job_outcome(
        conn,
        job_id,
        run_id,
        status == RunStatus::Success,
        run_finished,
    )?;
    let kind = match status {
        RunStatus::Success => EventKind::RunSucceeded,
        RunStatus::Failed => EventKind::RunFailed,
        RunStatus::Timeout => EventKind::RunTimeout,
        RunStatus::Cancelled => EventKind::RunCancelled,
    };
    events::emit(
        conn,
        kind,
        job_id,
        run_id,
        None,
        status.as_str(),
        run_finished,
    )?;
    Ok(())
}

/// Spawn one process attempt, tee its output, and wait for it.
#[allow(clippy::too_many_arguments)]
fn run_attempt(
    conn: &rusqlite::Connection,
    job: &EffectiveJob,
    job_id: &str,
    run_id: &str,
    attempt_id: &str,
    attempt_number: i64,
    writer: &Arc<Mutex<DailyLogWriter>>,
    now: DateTime<Utc>,
    cancel: &AtomicBool,
) -> Result<AttemptResult> {
    let mut cmd = Command::new(&job.command);
    cmd.args(&job.args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(cwd) = &job.cwd {
        cmd.current_dir(cwd);
    }
    set_process_group(&mut cmd);

    state::start_attempt(conn, attempt_id, run_id, attempt_number, None, now)?;
    events::emit(
        conn,
        EventKind::AttemptStarted,
        job_id,
        run_id,
        Some(attempt_id),
        &format!("attempt {attempt_number}"),
        now,
    )?;

    let mut child = match cmd.spawn() {
        Ok(c) => c,
        Err(e) => {
            // Spawn failure (e.g. command not found) is a failed attempt.
            let spawn_failed_at = Utc::now();
            state::finish_attempt(
                conn,
                attempt_id,
                "failed",
                Some(127),
                None,
                Some(&e.to_string()),
                spawn_failed_at,
            )?;
            events::emit(
                conn,
                EventKind::AttemptFailed,
                job_id,
                run_id,
                Some(attempt_id),
                &e.to_string(),
                spawn_failed_at,
            )?;
            return Ok(AttemptResult::Exited(127));
        }
    };
    let pid = child.id() as i32;
    state::start_attempt_pid(conn, attempt_id, pid)?;

    let out = child.stdout.take().expect("piped stdout");
    let err = child.stderr.take().expect("piped stderr");
    let t_out = spawn_reader(
        out,
        "stdout",
        job_id,
        run_id,
        attempt_number as u32,
        writer.clone(),
    );
    let t_err = spawn_reader(
        err,
        "stderr",
        job_id,
        run_id,
        attempt_number as u32,
        writer.clone(),
    );

    let (tx, rx) = mpsc::channel();
    let waiter = thread::spawn(move || {
        let _ = tx.send(child.wait());
    });

    let result = wait_loop(&rx, pid, job.timeout_secs, cancel);
    let attempt_finished = Utc::now();
    let _ = waiter.join();
    let _ = t_out.join();
    let _ = t_err.join();

    let (status, code, sig) = match result {
        AttemptResult::Exited(0) => ("success", Some(0), None),
        AttemptResult::Exited(c) => ("failed", Some(c), None),
        AttemptResult::Timeout => ("timeout", None, Some(15)),
        AttemptResult::Cancelled => ("cancelled", None, Some(15)),
    };
    state::finish_attempt(conn, attempt_id, status, code, sig, None, attempt_finished)?;
    let ev = if matches!(result, AttemptResult::Exited(0)) {
        EventKind::AttemptSucceeded
    } else {
        EventKind::AttemptFailed
    };
    events::emit(
        conn,
        ev,
        job_id,
        run_id,
        Some(attempt_id),
        status,
        attempt_finished,
    )?;
    Ok(result)
}

/// Read `reader` line-by-line, tee-ing to the terminal and the shared daily log.
fn spawn_reader<R: std::io::Read + Send + 'static>(
    reader: R,
    stream: &'static str,
    job_id: &str,
    run_id: &str,
    attempt: u32,
    writer: Arc<Mutex<DailyLogWriter>>,
) -> thread::JoinHandle<()> {
    let (job_id, run_id) = (job_id.to_owned(), run_id.to_owned());
    thread::spawn(move || {
        for line in BufReader::new(reader).lines() {
            let Ok(text) = line else {
                break;
            };
            if stream == "stderr" {
                let _ = writeln!(std::io::stderr(), "{text}");
            } else {
                let _ = writeln!(std::io::stdout(), "{text}");
            }
            let rec = LogRecord {
                ts: Utc::now().to_rfc3339(),
                job_id: job_id.clone(),
                run_id: run_id.clone(),
                attempt,
                stream: stream.to_owned(),
                text,
            };
            if let Ok(mut w) = writer.lock() {
                let _ = w.append(&rec);
            }
        }
    })
}

/// Poll for the child to exit, honoring an optional timeout and the per-run cancel
/// flag. On timeout/cancel: SIGTERM the group, wait `KILL_GRACE`, then SIGKILL.
fn wait_loop(
    rx: &mpsc::Receiver<std::io::Result<std::process::ExitStatus>>,
    pid: i32,
    timeout_secs: Option<u64>,
    cancel: &AtomicBool,
) -> AttemptResult {
    let tick = Duration::from_millis(100);
    let deadline = timeout_secs.map(|s| Instant::now() + Duration::from_secs(s));
    loop {
        match rx.recv_timeout(tick) {
            Ok(Ok(status)) => return AttemptResult::Exited(status.code().unwrap_or(-1)),
            Ok(Err(_)) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                return AttemptResult::Exited(-1);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        if cancel.load(Ordering::SeqCst) {
            kill_group(pid);
            return drain_after_kill(rx, pid, AttemptResult::Cancelled);
        }
        if let Some(d) = deadline
            && Instant::now() >= d
        {
            kill_group(pid);
            return drain_after_kill(rx, pid, AttemptResult::Timeout);
        }
    }
}

/// After SIGTERM-ing the group, wait up to KILL_GRACE for exit, else SIGKILL and
/// reap. Returns the supplied terminal result.
fn drain_after_kill(
    rx: &mpsc::Receiver<std::io::Result<std::process::ExitStatus>>,
    pid: i32,
    result: AttemptResult,
) -> AttemptResult {
    match rx.recv_timeout(KILL_GRACE) {
        Ok(_) => result,
        Err(_) => {
            force_kill_group(pid);
            let _ = rx.recv();
            result
        }
    }
}

#[cfg(unix)]
use unix_signals::{force_kill_group, kill_group, set_process_group};

#[cfg(unix)]
mod unix_signals {
    use std::os::unix::process::CommandExt;
    use std::process::Command;

    use nix::sys::signal::{Signal, killpg};
    use nix::unistd::{Pid, setsid};

    /// Put the child in its own session/process group so the whole tree can be
    /// signaled and the terminal's Ctrl-C (sent to the parent's group) does not
    /// reach it implicitly.
    pub(super) fn set_process_group(cmd: &mut Command) {
        // SAFETY: setsid is async-signal-safe and the only post-fork action.
        unsafe {
            cmd.pre_exec(|| setsid().map(|_| ()).map_err(std::io::Error::from));
        }
    }

    /// SIGTERM the child's process group (pgid == pid after setsid).
    pub(super) fn kill_group(pid: i32) {
        let _ = killpg(Pid::from_raw(pid), Signal::SIGTERM);
    }

    /// SIGKILL the child's process group.
    pub(super) fn force_kill_group(pid: i32) {
        let _ = killpg(Pid::from_raw(pid), Signal::SIGKILL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::EffectiveJob;
    use chrono::{TimeZone, Utc};

    fn now() -> DateTime<Utc> {
        Utc.timestamp_opt(1_700_000_000, 0).unwrap()
    }

    fn job(id: &str, command: &str, args: &[&str]) -> EffectiveJob {
        EffectiveJob {
            id: Some(id.into()),
            title: None,
            enabled: true,
            schedule: crate::config::NormalizedSchedule::MinuteAligned { every_minutes: 15 },
            command: command.into(),
            args: args.iter().map(|s| s.to_string()).collect(),
            cwd: None,
            timeout_secs: None,
            timezone: None,
            overlap_policy: crate::config::OverlapPolicy::Skip,
            missed_run_policy: crate::config::MissedRunPolicy::Skip,
            max_retries: 0,
            tags: vec![],
        }
    }

    /// Run with a fresh, un-cancelled per-run flag (the common case in tests).
    fn run_job_t(
        conn: &rusqlite::Connection,
        logs_dir: &Path,
        job: &EffectiveJob,
        now: DateTime<Utc>,
    ) -> Result<RunOutcome> {
        run_job(conn, logs_dir, job, now, &AtomicBool::new(false))
    }

    fn fixture() -> (tempfile::TempDir, rusqlite::Connection) {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::state::open(&dir.path().join("p.db")).unwrap();
        (dir, conn)
    }

    fn seed(conn: &rusqlite::Connection, id: &str) {
        conn.execute(
            "insert into jobs_state (job_id, state, config_hash, schedule_kind, updated_at)
             values (?1, 'active', 'h', 'minute', '2026-06-20T00:00:00Z')",
            [id],
        )
        .unwrap();
    }

    #[test]
    fn successful_command_records_success_run() {
        let (dir, conn) = fixture();
        seed(&conn, "ok");
        let out = run_job_t(
            &conn,
            &dir.path().join("logs"),
            &job("ok", "true", &[]),
            now(),
        )
        .unwrap();
        assert!(matches!(out.status, RunStatus::Success));
        assert_eq!(out.exit_code, Some(0));
        let status: String = conn
            .query_row("select status from runs where id=?1", [&out.id], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(status, "success");
    }

    #[test]
    fn failing_command_records_failed_run_and_increments_failures() {
        let (dir, conn) = fixture();
        seed(&conn, "bad");
        let out = run_job_t(
            &conn,
            &dir.path().join("logs"),
            &job("bad", "false", &[]),
            now(),
        )
        .unwrap();
        assert!(matches!(out.status, RunStatus::Failed));
        assert_eq!(out.exit_code, Some(1));
        let fails: i64 = conn
            .query_row(
                "select consecutive_failures from jobs_state where job_id='bad'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(fails, 1);
    }

    #[test]
    fn stdout_is_captured_to_daily_log() {
        let (dir, conn) = fixture();
        seed(&conn, "echoer");
        let logs = dir.path().join("logs");
        let out = run_job_t(&conn, &logs, &job("echoer", "echo", &["hi-there"]), now()).unwrap();
        let recs = crate::logs::read_logs(&logs, "echoer", Some(&out.id)).unwrap();
        assert!(
            recs.iter()
                .any(|r| r.stream == "stdout" && r.text.contains("hi-there")),
            "captured stdout, got {recs:?}"
        );
    }

    #[test]
    fn spawn_failure_records_failed_run_and_attempt() {
        let (dir, conn) = fixture();
        seed(&conn, "nosuchbin");
        let out = run_job_t(
            &conn,
            &dir.path().join("logs"),
            &job("nosuchbin", "periodic_no_such_command_xyz", &[]),
            now(),
        )
        .unwrap();
        assert!(
            matches!(out.status, RunStatus::Failed),
            "expected Failed, got {:?}",
            out.status
        );
        let attempt_status: String = conn
            .query_row(
                "select status from run_attempts where run_id=?1",
                [&out.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(attempt_status, "failed");
    }

    fn job_to(
        id: &str,
        command: &str,
        args: &[&str],
        timeout_secs: Option<u64>,
        retries: u32,
    ) -> EffectiveJob {
        let mut j = job(id, command, args);
        j.timeout_secs = timeout_secs;
        j.max_retries = retries;
        j
    }

    #[test]
    fn timeout_terminates_and_marks_timeout() {
        let (dir, conn) = fixture();
        seed(&conn, "slow");
        let out = run_job_t(
            &conn,
            &dir.path().join("logs"),
            &job_to("slow", "sleep", &["30"], Some(1), 0),
            now(),
        )
        .unwrap();
        assert!(
            matches!(out.status, RunStatus::Timeout),
            "got {:?}",
            out.status
        );
    }

    #[test]
    fn retries_on_failure_then_records_attempts() {
        let (dir, conn) = fixture();
        seed(&conn, "retry");
        let out = run_job_t(
            &conn,
            &dir.path().join("logs"),
            &job_to("retry", "false", &[], None, 2),
            now(),
        )
        .unwrap();
        assert!(matches!(out.status, RunStatus::Failed));
        assert_eq!(out.attempts, 3);
        let n: i64 = conn
            .query_row(
                "select count(*) from run_attempts where run_id=?1",
                [&out.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 3);
    }

    #[test]
    fn timeout_is_not_retried() {
        let (dir, conn) = fixture();
        seed(&conn, "slow2");
        let out = run_job_t(
            &conn,
            &dir.path().join("logs"),
            &job_to("slow2", "sleep", &["30"], Some(1), 3),
            now(),
        )
        .unwrap();
        assert!(matches!(out.status, RunStatus::Timeout));
        assert_eq!(out.attempts, 1, "timeout is terminal, not retried");
    }

    #[test]
    fn injected_cancel_flag_terminates_the_run() {
        // Per-run cancellation: a flag already set before dispatch aborts the run.
        // This is the loop-safe replacement for a process-global CANCEL — concurrent
        // daemon runs each carry their own flag, so one cancel can't kill another run.
        let (dir, conn) = fixture();
        seed(&conn, "cancelme");
        let cancel = AtomicBool::new(true);
        let out = run_job(
            &conn,
            &dir.path().join("logs"),
            &job_to("cancelme", "sleep", &["5"], None, 0),
            now(),
            &cancel,
        )
        .unwrap();
        assert!(
            matches!(out.status, RunStatus::Cancelled),
            "got {:?}",
            out.status
        );
    }

    #[test]
    fn terminal_event_stamps_real_finish_time_not_injected_start() {
        // Durations are read from event timestamps; the closing event must carry the
        // real finish instant, not the injected start `now`, or durations understate.
        let (dir, conn) = fixture();
        seed(&conn, "stamp");
        let out = run_job_t(
            &conn,
            &dir.path().join("logs"),
            &job("stamp", "true", &[]),
            now(),
        )
        .unwrap();
        let created: String = conn
            .query_row(
                "select created_at from events where run_id=?1 and event_type='run_succeeded'",
                [&out.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_ne!(
            created,
            now().to_rfc3339(),
            "terminal event must not stamp the injected start time"
        );
        assert_eq!(
            created, out.finished_at,
            "terminal event time == run finished_at"
        );
    }
}
