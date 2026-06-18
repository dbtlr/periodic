//! Rendering helpers: human-readable tables and the frozen JSON contract (and
//! future TUI formatting). Structured formats never carry color.

use serde::Serialize;

use crate::diagnostics::Diagnostic;

#[derive(Serialize)]
pub(crate) struct Summary {
    pub(crate) jobs: usize,
    pub(crate) errors: usize,
    pub(crate) warnings: usize,
}

#[derive(Serialize)]
pub(crate) struct Report<'a> {
    pub(crate) ok: bool,
    pub(crate) config_path: &'a str,
    pub(crate) summary: Summary,
    pub(crate) diagnostics: &'a [Diagnostic],
}

pub(crate) fn build_report<'a>(
    config_path: &'a str,
    jobs: usize,
    diagnostics: &'a [Diagnostic],
) -> Report<'a> {
    let errors = diagnostics.iter().filter(|d| d.is_error()).count();
    let warnings = diagnostics.len() - errors;
    Report {
        ok: errors == 0,
        config_path,
        summary: Summary {
            jobs,
            errors,
            warnings,
        },
        diagnostics,
    }
}

pub(crate) fn render_json(report: &Report) -> String {
    serde_json::to_string_pretty(report).expect("Report serializes")
}

/// Human render. NO_COLOR-safe: a leading glyph carries severity; color (added
/// in a later phase via the shared palette) is always glyph-backed (ADR 0003).
pub(crate) fn render_human(report: &Report) -> String {
    let mut s = String::new();
    for d in report.diagnostics {
        let glyph = if d.is_error() { "✗" } else { "!" };
        let job = d
            .job
            .as_deref()
            .map(|j| format!(" [{j}]"))
            .unwrap_or_default();
        s.push_str(&format!(
            "{glyph} {}{job}: {} ({})\n",
            d.code, d.message, d.path
        ));
    }
    let verdict = if report.ok { "valid" } else { "invalid" };
    s.push_str(&format!(
        "\n{}: {} job(s), {} error(s), {} warning(s)\n",
        verdict, report.summary.jobs, report.summary.errors, report.summary.warnings
    ));
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::Diagnostic;

    #[test]
    fn json_matches_golden() {
        let diags = vec![
            Diagnostic::error(
                "schedule.non_divisor",
                "every: 45m is not a divisor of 60",
                "jobs[0].schedule.every",
            )
            .with_job("cleanup"),
        ];
        let report = build_report("~/.config/periodic/periodic.config.yaml", 1, &diags);
        let json = render_json(&report);
        let expected = include_str!("../tests/golden/validate_basic.json");
        assert_eq!(json.trim(), expected.trim());
    }

    #[test]
    fn ok_true_when_only_warnings() {
        let diags = vec![Diagnostic::warning(
            "timeout.very_long",
            "x",
            "jobs[0].timeout",
        )];
        let report = build_report("p", 1, &diags);
        assert!(report.ok, "warnings do not flip ok to false");
        assert_eq!(report.summary.warnings, 1);
    }
}
