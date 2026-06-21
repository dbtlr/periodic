//! Diagnostics: runtime health checks, validation summaries, and scheduler
//! diagnostics.
//!
//! Phase 0.4 covers state-database health only (read-only). Daemon liveness and
//! crash-recovery reporting arrive with the daemon in 0.6; the `--format json`
//! doctor contract is deliberately deferred until that health model is complete,
//! so the shape isn't frozen half-built.

use std::process::ExitCode;

use chrono::Utc;

use crate::state::{self, DaemonStatus, DbHealth};

/// A daemon whose heartbeat is older than this is treated as crashed/unresponsive.
const DAEMON_STALE_AFTER: chrono::Duration = chrono::Duration::seconds(90);

/// Run `periodic doctor`: inspect the state database (read-only) and report.
/// Exit `0` when healthy or not-yet-created, `1` when a problem needs action.
pub(crate) fn run() -> anyhow::Result<ExitCode> {
    let db_path = state::default_db_path();
    let health = state::inspect(&db_path)?;
    let (db_text, db_ok) = report(&health);

    // Daemon liveness reads the same db read-only. A missing/uncreated db has no
    // daemon row, so an open here is harmless (and never created by `inspect`).
    let daemon_status = if health.schema_version.is_some() {
        let conn = state::open(&db_path)?;
        state::read_daemon_status(&conn)?
    } else {
        None
    };
    let (daemon_text, daemon_ok) = daemon_report(daemon_status.as_ref(), Utc::now());

    print!("{db_text}{daemon_text}");
    Ok(if db_ok && daemon_ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}

/// Render the daemon-liveness line and an ok/not-ok verdict. Pure over an injected
/// `now` and an optional [`DaemonStatus`] so each branch is unit-testable without a
/// live daemon. A stale "running" daemon is the only not-ok branch — a never-started
/// or cleanly stopped daemon is informational.
fn daemon_report(status: Option<&DaemonStatus>, now: chrono::DateTime<Utc>) -> (String, bool) {
    let (line, ok) = match status {
        None => ("daemon: not running".to_owned(), true),
        Some(s) if s.state == "running" => {
            if state::daemon_is_stale(s, now, DAEMON_STALE_AFTER) {
                (
                    "daemon: not responding (stale heartbeat; possible crash)".to_owned(),
                    false,
                )
            } else {
                (format!("daemon: running (pid {})", s.pid), true)
            }
        }
        Some(_) => ("daemon: stopped".to_owned(), true),
    };
    (format!("{line}\n"), ok)
}

/// Render the health report and an overall ok/not-ok verdict. A missing database
/// is informational (created on first use); a schema older than this build will
/// auto-migrate; only a schema *newer* than this build is a problem.
fn report(health: &DbHealth) -> (String, bool) {
    let line = match health.schema_version {
        None => format!(
            "state database: not yet created at {} (created on first use)",
            health.path
        ),
        Some(v) if v == health.expected_version => {
            format!("state database: healthy at {} (schema v{v})", health.path)
        }
        Some(v) if v < health.expected_version => format!(
            "state database: at {}, schema v{v} will upgrade to v{} on next use",
            health.path, health.expected_version
        ),
        Some(v) => format!(
            "state database: at {}, schema v{v} is newer than this build (expected v{}); upgrade periodic",
            health.path, health.expected_version
        ),
    };
    let ok = match health.schema_version {
        Some(v) => v <= health.expected_version,
        None => true,
    };
    (format!("{line}\n"), ok)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn health(version: Option<i64>, expected: i64) -> DbHealth {
        DbHealth {
            path: "/tmp/periodic.db".to_owned(),
            schema_version: version,
            expected_version: expected,
        }
    }

    #[test]
    fn not_created_is_informational_and_ok() {
        let (text, ok) = report(&health(None, 1));
        assert!(ok);
        assert!(text.contains("not yet created"), "got {text}");
    }

    #[test]
    fn matching_schema_is_healthy() {
        let (text, ok) = report(&health(Some(1), 1));
        assert!(ok);
        assert!(text.contains("healthy"), "got {text}");
    }

    #[test]
    fn older_schema_will_upgrade_and_is_ok() {
        let (text, ok) = report(&health(Some(1), 2));
        assert!(ok, "an older schema auto-migrates and is not a failure");
        assert!(
            text.contains("upgrade") || text.contains("migrate"),
            "got {text}"
        );
    }

    #[test]
    fn newer_schema_is_a_problem() {
        let (text, ok) = report(&health(Some(3), 2));
        assert!(!ok, "a db newer than this build should fail the check");
        assert!(text.contains("newer"), "got {text}");
    }

    #[test]
    fn report_always_shows_the_path() {
        let (text, _) = report(&health(Some(1), 1));
        assert!(text.contains("/tmp/periodic.db"));
    }

    mod daemon {
        use super::*;
        use chrono::{DateTime, Duration, Utc};

        fn now() -> DateTime<Utc> {
            DateTime::parse_from_rfc3339("2026-06-21T12:00:00Z")
                .unwrap()
                .with_timezone(&Utc)
        }

        fn status(state: &str, heartbeat_age: Duration) -> DaemonStatus {
            DaemonStatus {
                pid: 4242,
                state: state.to_owned(),
                heartbeat: now() - heartbeat_age,
            }
        }

        #[test]
        fn no_daemon_row_is_not_running_and_ok() {
            let (text, ok) = daemon_report(None, now());
            assert!(ok);
            assert!(text.contains("daemon: not running"), "got {text}");
        }

        #[test]
        fn fresh_running_daemon_reports_pid_and_ok() {
            let s = status("running", Duration::seconds(10));
            let (text, ok) = daemon_report(Some(&s), now());
            assert!(ok);
            assert!(text.contains("daemon: running (pid 4242)"), "got {text}");
        }

        #[test]
        fn stale_running_daemon_is_not_responding_and_not_ok() {
            let s = status("running", Duration::seconds(120));
            let (text, ok) = daemon_report(Some(&s), now());
            assert!(!ok, "a stale daemon must flip the verdict to not-ok");
            assert!(
                text.contains("not responding") && text.contains("possible crash"),
                "got {text}"
            );
        }

        #[test]
        fn stopping_daemon_reports_stopped_and_ok() {
            let s = status("stopping", Duration::seconds(10));
            let (text, ok) = daemon_report(Some(&s), now());
            assert!(ok);
            assert!(text.contains("daemon: stopped"), "got {text}");
        }

        #[test]
        fn stopped_daemon_reports_stopped_and_ok() {
            // Even with an old heartbeat, a cleanly stopped daemon is informational.
            let s = status("stopped", Duration::seconds(600));
            let (text, ok) = daemon_report(Some(&s), now());
            assert!(ok);
            assert!(text.contains("daemon: stopped"), "got {text}");
        }
    }
}
