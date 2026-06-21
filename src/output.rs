//! Rendering helpers: human-readable tables and the frozen JSON contract (and
//! future TUI formatting). Structured formats never carry color.

use serde::Serialize;

use crate::diagnostics::Diagnostic;
use crate::executor::{RunOutcome, RunStatus};
use crate::logs::LogRecord;
use crate::state::{JobStateRow, RunRow};

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

// ─── jobs list / status ──────────────────────────────────────────────────────

/// JSON envelope for `periodic jobs list` — a stable object so sibling fields
/// can be added without breaking the frozen contract (decision 0002).
#[derive(Serialize)]
struct JobsReport<'a> {
    jobs: &'a [JobStateRow],
}

/// JSON envelope for `periodic jobs status <id>`.
#[derive(Serialize)]
struct JobReport<'a> {
    job: &'a JobStateRow,
}

/// `jobs list --format json`: `{ "jobs": [ … ] }`.
pub(crate) fn render_jobs_json(jobs: &[JobStateRow]) -> String {
    serde_json::to_string_pretty(&JobsReport { jobs }).expect("jobs report serializes")
}

/// `jobs status --format json`: `{ "job": { … } }`.
pub(crate) fn render_job_json(job: &JobStateRow) -> String {
    serde_json::to_string_pretty(&JobReport { job }).expect("job report serializes")
}

/// `jobs list` human table. No color yet (added with the shared palette in a
/// later phase, ADR 0003); columns are space-aligned and a missing next run
/// renders as an em dash.
pub(crate) fn render_jobs_human(jobs: &[JobStateRow]) -> String {
    if jobs.is_empty() {
        return "no jobs configured\n".to_owned();
    }
    let id_w = col_width(jobs.iter().map(|j| j.job_id.len()), "ID");
    let state_w = col_width(jobs.iter().map(|j| j.state.len()), "STATE");
    let kind_w = col_width(jobs.iter().map(|j| j.schedule_kind.len()), "SCHEDULE");

    let mut s = String::new();
    s.push_str(&format!(
        "{:<id_w$}  {:<state_w$}  {:<kind_w$}  {}\n",
        "ID", "STATE", "SCHEDULE", "NEXT RUN"
    ));
    for j in jobs {
        let next = j.next_run_at.as_deref().unwrap_or("—");
        s.push_str(&format!(
            "{:<id_w$}  {:<state_w$}  {:<kind_w$}  {next}\n",
            j.job_id, j.state, j.schedule_kind
        ));
    }
    s.push_str(&format!("\n{} job(s)\n", jobs.len()));
    s
}

/// `jobs status <id>` human detail block.
pub(crate) fn render_job_human(job: &JobStateRow) -> String {
    let next = job.next_run_at.as_deref().unwrap_or("—");
    format!(
        "job:       {}\nstate:     {}\nschedule:  {}\nnext run:  {next}\nconfig:    {}\nupdated:   {}\n",
        job.job_id, job.state, job.schedule_kind, job.config_hash, job.updated_at
    )
}

/// Header-aware column width: the widest cell, but never narrower than the header.
fn col_width(cells: impl Iterator<Item = usize>, header: &str) -> usize {
    cells.max().unwrap_or(0).max(header.len())
}

// ─── run summary / history / logs ───────────────────────────────────────────

#[derive(Serialize)]
struct RunSummary<'a> {
    id: &'a str,
    job_id: &'a str,
    status: &'a str,
    trigger_type: &'a str,
    started_at: &'a str,
    finished_at: &'a str,
    exit_code: Option<i32>,
    attempts: u32,
}

#[derive(Serialize)]
struct RunReport<'a> {
    run: RunSummary<'a>,
}

#[derive(Serialize)]
struct RunsReport<'a> {
    runs: &'a [RunRow],
}

#[derive(Serialize)]
struct LogsReport<'a> {
    lines: &'a [LogRecord],
}

/// `jobs run --format json`: `{ "run": { … } }`.
pub(crate) fn render_run_json(out: &RunOutcome) -> String {
    let report = RunReport {
        run: RunSummary {
            id: &out.id,
            job_id: &out.job_id,
            status: out.status.as_str(),
            trigger_type: "manual",
            started_at: &out.started_at,
            finished_at: &out.finished_at,
            exit_code: out.exit_code,
            attempts: out.attempts,
        },
    };
    serde_json::to_string_pretty(&report).expect("run report serializes")
}

/// `jobs run` human one-liner. Glyph carries success/failure (NO_COLOR-safe).
pub(crate) fn render_run_human(out: &RunOutcome) -> String {
    let glyph = if out.status == RunStatus::Success {
        "✓"
    } else {
        "✗"
    };
    let code = out
        .exit_code
        .map(|c| c.to_string())
        .unwrap_or_else(|| "—".into());
    format!(
        "{glyph} {} {} (exit {code}, {} attempt(s))\n",
        out.job_id, out.status.as_str(), out.attempts
    )
}

/// `jobs history --format json`: `{ "runs": [ … ] }`.
pub(crate) fn render_runs_json(runs: &[RunRow]) -> String {
    serde_json::to_string_pretty(&RunsReport { runs }).expect("runs report serializes")
}

/// `jobs history` human table.
pub(crate) fn render_runs_human(runs: &[RunRow]) -> String {
    if runs.is_empty() {
        return "no runs yet\n".to_owned();
    }
    let mut s = String::from("RUN                       STATUS     EXIT  STARTED\n");
    for r in runs {
        let code = r
            .exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "—".into());
        let started = r.started_at.as_deref().unwrap_or("—");
        s.push_str(&format!(
            "{:<24}  {:<9}  {:>4}  {started}\n",
            r.id, r.status, code
        ));
    }
    s.push_str(&format!("\n{} run(s)\n", runs.len()));
    s
}

/// `logs --format json`: `{ "lines": [ … raw LogRecords … ] }`.
pub(crate) fn render_logs_json(lines: &[LogRecord]) -> String {
    serde_json::to_string_pretty(&LogsReport { lines }).expect("logs report serializes")
}

/// `logs` human render: one line per record, stderr marked with `!`.
pub(crate) fn render_logs_human(lines: &[LogRecord]) -> String {
    if lines.is_empty() {
        return "no output\n".to_owned();
    }
    let mut s = String::new();
    for l in lines {
        let tag = if l.stream == "stderr" { "!" } else { " " };
        s.push_str(&format!("{tag} {}\n", l.text));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostics::Diagnostic;

    #[test]
    fn run_json_wraps_in_run_key() {
        use crate::executor::{RunOutcome, RunStatus};
        let out = RunOutcome { id: "r1".into(), job_id: "cleanup".into(), status: RunStatus::Success,
            started_at: "2026-06-21T00:00:00+00:00".into(), finished_at: "2026-06-21T00:00:01+00:00".into(),
            exit_code: Some(0), attempts: 1 };
        let json = render_run_json(&out);
        assert!(json.contains("\"run\""));
        assert!(json.contains("\"status\": \"success\""));
        assert!(json.contains("\"exit_code\": 0"));
    }

    #[test]
    fn runs_json_wraps_in_runs_array() {
        use crate::state::RunRow;
        let rows = vec![RunRow { id: "r1".into(), status: "failed".into(), trigger_type: "manual".into(),
            started_at: Some("2026-06-21T00:00:00+00:00".into()),
            finished_at: Some("2026-06-21T00:00:01+00:00".into()), exit_code: Some(1), attempts: 2 }];
        let json = render_runs_json(&rows);
        assert!(json.contains("\"runs\""));
        assert!(json.contains("\"id\": \"r1\""));
    }

    #[test]
    fn logs_human_renders_stream_lines() {
        use crate::logs::LogRecord;
        let recs = vec![LogRecord { ts: "2026-06-21T00:00:00+00:00".into(), job_id: "c".into(),
            run_id: "r1".into(), attempt: 1, stream: "stdout".into(), text: "hello".into() }];
        assert!(render_logs_human(&recs).contains("hello"));
    }

    #[test]
    fn logs_human_handles_empty() {
        assert!(render_logs_human(&[]).contains("no output"));
    }

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

    fn row(id: &str, state: &str, kind: &str, next: Option<&str>, hash: &str) -> JobStateRow {
        JobStateRow {
            job_id: id.to_owned(),
            state: state.to_owned(),
            schedule_kind: kind.to_owned(),
            next_run_at: next.map(str::to_owned),
            config_hash: hash.to_owned(),
            updated_at: "2026-06-20T09:00:00+00:00".to_owned(),
        }
    }

    #[test]
    fn jobs_json_matches_golden() {
        let jobs = vec![
            row("alpha", "disabled", "calendar", None, "hash-a"),
            row(
                "beta",
                "active",
                "minute",
                Some("2026-06-20T09:15:00+00:00"),
                "hash-b",
            ),
        ];
        let json = render_jobs_json(&jobs);
        let expected = include_str!("../tests/golden/jobs_list_basic.json");
        assert_eq!(json.trim(), expected.trim());
    }

    #[test]
    fn job_json_wraps_in_job_key() {
        let json = render_job_json(&row("alpha", "active", "minute", None, "h"));
        assert!(json.contains("\"job\""), "got {json}");
        assert!(json.contains("\"id\": \"alpha\""), "got {json}");
    }

    #[test]
    fn jobs_human_lists_ids_and_count() {
        let jobs = vec![
            row("alpha", "disabled", "calendar", None, "h"),
            row(
                "beta",
                "active",
                "minute",
                Some("2026-06-20T09:15:00+00:00"),
                "h",
            ),
        ];
        let out = render_jobs_human(&jobs);
        assert!(out.contains("alpha"));
        assert!(out.contains("beta"));
        assert!(out.contains("2 job(s)"));
        assert!(out.contains("—"), "missing next_run renders as em dash");
    }

    #[test]
    fn jobs_human_handles_empty() {
        assert!(render_jobs_human(&[]).contains("no jobs configured"));
    }

    #[test]
    fn job_human_shows_detail_fields() {
        let out = render_job_human(&row("alpha", "disabled", "calendar", None, "abc"));
        assert!(out.contains("alpha"));
        assert!(out.contains("disabled"));
        assert!(out.contains("—"));
    }
}
