//! Shared diagnostic types: the `Vec<Diagnostic>` interface between the config
//! loader, the validation engine, and the renderers. The `Serialize` shape is
//! part of the frozen `--format json` contract (ADR 0002) — additive only.

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Severity {
    Error,
    Warning,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Diagnostic {
    pub(crate) severity: Severity,
    pub(crate) code: &'static str,
    pub(crate) message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) job: Option<String>,
    pub(crate) path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) column: Option<usize>,
}

impl Diagnostic {
    pub(crate) fn error(
        code: &'static str,
        message: impl Into<String>,
        path: impl Into<String>,
    ) -> Self {
        Self {
            severity: Severity::Error,
            code,
            message: message.into(),
            job: None,
            path: path.into(),
            line: None,
            column: None,
        }
    }

    pub(crate) fn warning(
        code: &'static str,
        message: impl Into<String>,
        path: impl Into<String>,
    ) -> Self {
        Self {
            severity: Severity::Warning,
            code,
            message: message.into(),
            job: None,
            path: path.into(),
            line: None,
            column: None,
        }
    }

    pub(crate) fn with_job(mut self, job: impl Into<String>) -> Self {
        self.job = Some(job.into());
        self
    }

    pub(crate) fn with_location(mut self, line: usize, column: usize) -> Self {
        self.line = Some(line);
        self.column = Some(column);
        self
    }

    pub(crate) fn is_error(&self) -> bool {
        matches!(self.severity, Severity::Error)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_serializes_to_frozen_shape() {
        let d = Diagnostic::error(
            "schedule.non_divisor",
            "every: 45m is not a divisor of 60",
            "jobs[0].schedule.every",
        )
        .with_job("cleanup");
        let v = serde_json::to_value(&d).unwrap();
        assert_eq!(v["severity"], "error");
        assert_eq!(v["code"], "schedule.non_divisor");
        assert_eq!(v["job"], "cleanup");
        assert_eq!(v["path"], "jobs[0].schedule.every");
        assert!(v.get("line").is_none(), "line omitted when absent");
    }
}
