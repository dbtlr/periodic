//! Runtime events: structs and enums, serialization, and emission helpers.

use chrono::{DateTime, Utc};
use rusqlite::Connection;

use crate::error::Result;

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

/// Append a lifecycle event. The id scopes to the attempt (when present) so that
/// retries emitting the same `kind` under one injected `now` don't collide: each
/// attempt has a distinct `attempt_id`, and within one scope every emitted kind is
/// distinct (e.g. `attempt_started` then `attempt_succeeded`/`attempt_failed`).
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
    let id = format!("ev-{scope}-{}-{}", kind.as_str(), now.timestamp_micros());
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
    fn event_kind_as_str_is_stable() {
        assert_eq!(EventKind::AttemptFailed.as_str(), "attempt_failed");
        assert_eq!(EventKind::RunTimeout.as_str(), "run_timeout");
    }
}
