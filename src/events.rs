//! Runtime events: structs and enums, serialization, and emission helpers.

use std::sync::atomic::{AtomicU64, Ordering};

use chrono::{DateTime, Utc};
use rusqlite::Connection;

use crate::error::Result;

/// Process-wide monotonic suffix for event ids. Timestamp + (scope, kind) alone do
/// not guarantee uniqueness once the daemon emits the same kind twice in one scope
/// under a single clock; this counter makes every emitted id distinct.
static EVENT_SEQ: AtomicU64 = AtomicU64::new(0);

/// Lifecycle event types emitted by the executor. `as_str` is the stable wire
/// form persisted in `events.event_type`.
#[derive(Debug, Clone, Copy)]
pub(crate) enum EventKind {
    RunStarted,
    AttemptStarted,
    AttemptSucceeded,
    AttemptFailed,
    RunSucceeded,
    RunFailed,
    RunTimeout,
    RunCancelled,
}

impl EventKind {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            EventKind::RunStarted => "run_started",
            EventKind::AttemptStarted => "attempt_started",
            EventKind::AttemptSucceeded => "attempt_succeeded",
            EventKind::AttemptFailed => "attempt_failed",
            EventKind::RunSucceeded => "run_succeeded",
            EventKind::RunFailed => "run_failed",
            EventKind::RunTimeout => "run_timeout",
            EventKind::RunCancelled => "run_cancelled",
        }
    }

    /// Severity tag stored in `events.level`. Failures/timeouts are `error`.
    fn level(self) -> &'static str {
        match self {
            EventKind::AttemptFailed
            | EventKind::RunFailed
            | EventKind::RunTimeout
            | EventKind::RunCancelled => "error",
            _ => "info",
        }
    }
}

/// Append a lifecycle event. The id is `ev-<scope>-<kind>-<micros>-<seq>`, where
/// `scope` is the attempt (when present) else the run, and `seq` is a process-wide
/// monotonic counter. The counter guarantees uniqueness even when the daemon emits
/// the same kind twice in one scope under a single injected `now`.
pub(crate) fn emit(
    conn: &Connection,
    kind: EventKind,
    job_id: &str,
    run_id: &str,
    attempt_id: Option<&str>,
    message: &str,
    now: DateTime<Utc>,
) -> Result<()> {
    let ts = now.to_rfc3339();
    let scope = attempt_id.unwrap_or(run_id);
    let seq = EVENT_SEQ.fetch_add(1, Ordering::Relaxed);
    let id = format!(
        "ev-{scope}-{}-{}-{seq}",
        kind.as_str(),
        now.timestamp_micros()
    );
    conn.execute(
        "insert into events (id, job_id, run_id, attempt_id, level, event_type, message, created_at)
         values (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        rusqlite::params![id, job_id, run_id, attempt_id, kind.level(), kind.as_str(), message, ts],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    #[test]
    fn emit_writes_event_row_with_type_and_ids() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::state::open(&dir.path().join("p.db")).unwrap();
        let now = Utc.timestamp_opt(1000, 0).unwrap();
        emit(
            &conn,
            EventKind::RunStarted,
            "cleanup",
            "r1",
            None,
            "run started",
            now,
        )
        .unwrap();
        let (etype, job, run): (String, Option<String>, Option<String>) = conn
            .query_row(
                "select event_type, job_id, run_id from events where run_id='r1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(etype, "run_started");
        assert_eq!(job.as_deref(), Some("cleanup"));
        assert_eq!(run.as_deref(), Some("r1"));
    }

    #[test]
    fn same_kind_in_one_scope_under_one_clock_does_not_collide() {
        // The daemon will reuse the executor in a loop; two emits of the same kind
        // in one scope under a single injected `now` must both persist, not abort on
        // a primary-key collision.
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::state::open(&dir.path().join("p.db")).unwrap();
        let now = Utc.timestamp_opt(1000, 0).unwrap();
        emit(&conn, EventKind::RunStarted, "j", "r1", None, "first", now).unwrap();
        let second = emit(&conn, EventKind::RunStarted, "j", "r1", None, "second", now);
        assert!(second.is_ok(), "second emit must not collide: {second:?}");
        let n: i64 = conn
            .query_row("select count(*) from events where run_id='r1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn event_kind_as_str_is_stable() {
        assert_eq!(EventKind::AttemptFailed.as_str(), "attempt_failed");
        assert_eq!(EventKind::RunTimeout.as_str(), "run_timeout");
    }
}
