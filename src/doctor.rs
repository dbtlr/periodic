//! Diagnostics: runtime health checks, validation summaries, and scheduler
//! diagnostics.
//!
//! Phase 0.4 covers state-database health only (read-only). Daemon liveness and
//! crash-recovery reporting arrive with the daemon in 0.6; the `--format json`
//! doctor contract is deliberately deferred until that health model is complete,
//! so the shape isn't frozen half-built.

use std::process::ExitCode;

use crate::state::{self, DbHealth};

/// Run `periodic doctor`: inspect the state database (read-only) and report.
/// Exit `0` when healthy or not-yet-created, `1` when a problem needs action.
pub(crate) fn run() -> anyhow::Result<ExitCode> {
    let health = state::inspect(&state::default_db_path())?;
    let (text, ok) = report(&health);
    print!("{text}");
    Ok(if ok {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
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
}
