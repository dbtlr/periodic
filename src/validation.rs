//! Validation: schema, semantic, schedule, and execution checks. Distinct from
//! parsing — a config can parse yet still be semantically invalid.

use std::collections::HashSet;
use std::str::FromStr;

use crate::config::{Every, Interval, RawConfig, RawJob, parse_duration, parse_every_interval};
use crate::diagnostics::Diagnostic;

/// Run every semantic check over a parsed config, accumulating all diagnostics.
/// Pure: no I/O, no rendering. The single engine every mutation path reuses.
pub(crate) fn validate_config(config: &RawConfig) -> Vec<Diagnostic> {
    let mut out = Vec::new();
    if config.version != 1 {
        out.push(Diagnostic::error(
            "version.unsupported",
            format!("unsupported config version: {}", config.version),
            "version",
        ));
    }
    let mut seen: HashSet<&str> = HashSet::new();
    for (idx, job) in config.jobs.iter().enumerate() {
        check_schedule(idx, job, &mut out);
        check_job(idx, job, &mut out);
        if let Some(id) = &job.id
            && !seen.insert(id)
        {
            out.push(
                Diagnostic::error(
                    "job.id_duplicate",
                    format!("duplicate job id: {id:?}"),
                    format!("jobs[{idx}].id"),
                )
                .with_job(id.clone()),
            );
        }
    }
    out
}

const VERY_LONG_TIMEOUT_SECS: u64 = 24 * 3600;

fn check_job(idx: usize, job: &RawJob, out: &mut Vec<Diagnostic>) {
    let jid = job.id.clone();
    let push = |out: &mut Vec<Diagnostic>, d: Diagnostic| {
        out.push(match &jid {
            Some(id) => d.with_job(id.clone()),
            None => d,
        });
    };

    if let Some(id) = &job.id
        && !is_kebab(id)
    {
        push(
            out,
            Diagnostic::error(
                "job.id_invalid",
                format!("job id {id:?} must be kebab-case (a-z, 0-9, -)"),
                format!("jobs[{idx}].id"),
            ),
        );
    }

    match &job.execution.command {
        None => push(
            out,
            Diagnostic::error(
                "execution.command_missing",
                "execution.command is required",
                format!("jobs[{idx}].execution.command"),
            ),
        ),
        Some(cmd) if cmd.trim().is_empty() => push(
            out,
            Diagnostic::error(
                "execution.command_missing",
                "execution.command is empty",
                format!("jobs[{idx}].execution.command"),
            ),
        ),
        Some(cmd) => {
            if !command_resolves(cmd) {
                push(
                    out,
                    Diagnostic::warning(
                        "execution.not_found",
                        format!("command not found on this machine: {cmd:?}"),
                        format!("jobs[{idx}].execution.command"),
                    ),
                );
            }
        }
    }

    if let Some(t) = &job.timeout {
        match parse_duration(t) {
            Err(()) => push(
                out,
                Diagnostic::error(
                    "duration.invalid",
                    format!("invalid duration: {t:?}"),
                    format!("jobs[{idx}].timeout"),
                ),
            ),
            Ok(secs) if secs > VERY_LONG_TIMEOUT_SECS => push(
                out,
                Diagnostic::warning(
                    "timeout.very_long",
                    format!("timeout {t:?} exceeds 24h"),
                    format!("jobs[{idx}].timeout"),
                ),
            ),
            Ok(_) => {}
        }
    }

    if let Some(retry) = &job.retry
        && retry.max_retries.is_some_and(|n| n < 0)
    {
        push(
            out,
            Diagnostic::error(
                "field.type",
                "retry.max_retries must be >= 0",
                format!("jobs[{idx}].retry.max_retries"),
            ),
        );
    }
}

/// Whether `s` is a valid kebab-case job id (`a-z`, `0-9`, `-`; non-empty). The
/// canonical id-charset rule, reused by `jobs add` to reject ids before writing.
pub(crate) fn is_kebab(s: &str) -> bool {
    !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Best-effort resolution: an existing absolute/relative path, or a bare name
/// found on `PATH`. Only drives a warning, never an error (spec §6).
fn command_resolves(cmd: &str) -> bool {
    let first = cmd.split_whitespace().next().unwrap_or(cmd);
    let expanded = shellexpand_tilde(first);
    if expanded.contains('/') {
        return std::path::Path::new(expanded.as_ref()).exists();
    }
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|dir| dir.join(expanded.as_ref()).exists())
    })
}

fn shellexpand_tilde(s: &str) -> std::borrow::Cow<'_, str> {
    if let Some(rest) = s.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return std::borrow::Cow::Owned(format!("{}/{rest}", home.to_string_lossy()));
    }
    std::borrow::Cow::Borrowed(s)
}

fn check_schedule(idx: usize, job: &RawJob, out: &mut Vec<Diagnostic>) {
    let s = &job.schedule;
    let base = format!("jobs[{idx}].schedule");
    let jid = job.id.clone();

    let push = |out: &mut Vec<Diagnostic>, d: Diagnostic| {
        out.push(match &jid {
            Some(id) => d.with_job(id.clone()),
            None => d,
        });
    };

    if s.every.is_some() && s.cron.is_some() {
        push(
            out,
            Diagnostic::error(
                "schedule.invalid",
                "schedule sets both `every` and `cron`",
                base.clone(),
            ),
        );
    }
    if s.every.is_none() && s.cron.is_none() {
        push(
            out,
            Diagnostic::error(
                "schedule.invalid",
                "schedule sets neither `every` nor `cron`",
                base.clone(),
            ),
        );
    }
    if s.on_day.is_some() && s.last_day == Some(true) {
        push(
            out,
            Diagnostic::error(
                "schedule.invalid",
                "schedule sets both `on_day` and `last_day`",
                base.clone(),
            ),
        );
    }
    if let Some(day) = s.on_day
        && !(1..=31).contains(&day)
    {
        push(
            out,
            Diagnostic::error(
                "schedule.invalid",
                format!("on_day {day} is out of range 1..=31"),
                format!("{base}.on_day"),
            ),
        );
    }

    if let Some(every) = &s.every {
        let tokens: Vec<&String> = match every {
            Every::One(v) => vec![v],
            Every::Many(vs) => vs.iter().collect(),
        };
        for tok in tokens {
            match parse_every_interval(tok) {
                Ok(Interval::Minutes(n)) if n == 0 || 60 % n != 0 => {
                    push(
                        out,
                        Diagnostic::error(
                            "schedule.non_divisor",
                            format!("every: {tok} is not a divisor of 60"),
                            format!("{base}.every"),
                        ),
                    );
                }
                Ok(Interval::Hours(n)) if n == 0 || 24 % n != 0 => {
                    push(
                        out,
                        Diagnostic::error(
                            "schedule.non_divisor",
                            format!("every: {tok} is not a divisor of 24"),
                            format!("{base}.every"),
                        ),
                    );
                }
                Ok(_) => {}
                Err(()) => {
                    push(
                        out,
                        Diagnostic::error(
                            "schedule.invalid",
                            format!("every: {tok} is not a recognized interval or weekday"),
                            format!("{base}.every"),
                        ),
                    );
                }
            }
        }
    }

    if let Some(at) = &s.at
        && !is_valid_hhmm(at)
    {
        push(
            out,
            Diagnostic::error(
                "schedule.invalid",
                format!("at: {at:?} is not a HH:MM time"),
                format!("{base}.at"),
            ),
        );
    }

    if let Some(cron) = &s.cron
        && croner::Cron::from_str(cron).is_err()
    {
        push(
            out,
            Diagnostic::error(
                "schedule.cron_invalid",
                format!("invalid cron expression: {cron:?}"),
                format!("{base}.cron"),
            ),
        );
    }

    if let Some(tz) = &s.timezone
        && tz != "local"
        && chrono_tz::Tz::from_str(tz).is_err()
    {
        push(
            out,
            Diagnostic::error(
                "schedule.timezone_invalid",
                format!("unknown timezone: {tz:?}"),
                format!("{base}.timezone"),
            ),
        );
    }
}

fn is_valid_hhmm(s: &str) -> bool {
    let Some((h, m)) = s.split_once(':') else {
        return false;
    };
    !h.is_empty()
        && !m.is_empty()
        && matches!((h.parse::<u32>(), m.parse::<u32>()), (Ok(h), Ok(m)) if h < 24 && m < 60)
}

#[cfg(test)]
mod schedule_tests {
    use super::*;
    use crate::config::parse;

    fn codes(yaml: &str) -> Vec<&'static str> {
        validate_config(&parse(yaml).unwrap())
            .into_iter()
            .map(|d| d.code)
            .collect()
    }

    #[test]
    fn non_divisor_minute_is_error() {
        let c = codes(
            "version: 1\njobs:\n  - schedule: { every: 45m }\n    execution: { command: x }\n",
        );
        assert!(c.contains(&"schedule.non_divisor"));
    }

    #[test]
    fn divisor_minute_is_clean() {
        let c = codes(
            "version: 1\njobs:\n  - schedule: { every: 15m }\n    execution: { command: x }\n",
        );
        assert!(!c.contains(&"schedule.non_divisor"));
    }

    #[test]
    fn bad_cron_is_error() {
        let c = codes(
            "version: 1\njobs:\n  - schedule: { cron: \"not a cron\" }\n    execution: { command: x }\n",
        );
        assert!(c.contains(&"schedule.cron_invalid"));
    }

    #[test]
    fn unknown_timezone_is_error() {
        let c = codes(
            "version: 1\njobs:\n  - schedule: { every: day, at: \"09:00\", timezone: Mars/Phobos }\n    execution: { command: x }\n",
        );
        assert!(c.contains(&"schedule.timezone_invalid"));
    }

    #[test]
    fn every_and_cron_both_set_is_invalid() {
        let c = codes(
            "version: 1\njobs:\n  - schedule: { every: day, cron: \"0 9 * * *\" }\n    execution: { command: x }\n",
        );
        assert!(c.contains(&"schedule.invalid"));
    }
}

#[cfg(test)]
mod job_tests {
    use super::*;
    use crate::config::parse;

    fn codes(yaml: &str) -> Vec<&'static str> {
        validate_config(&parse(yaml).unwrap())
            .into_iter()
            .map(|d| d.code)
            .collect()
    }

    #[test]
    fn unsupported_version() {
        let c = codes("version: 2\njobs: []\n");
        assert!(c.contains(&"version.unsupported"));
    }

    #[test]
    fn duplicate_id() {
        let c = codes(
            "version: 1\njobs:\n  - id: a\n    schedule: { every: day, at: \"09:00\" }\n    execution: { command: x }\n  - id: a\n    schedule: { every: day, at: \"09:00\" }\n    execution: { command: y }\n",
        );
        assert!(c.contains(&"job.id_duplicate"));
    }

    #[test]
    fn invalid_id_charset() {
        let c = codes(
            "version: 1\njobs:\n  - id: \"Not Kebab\"\n    schedule: { every: day, at: \"09:00\" }\n    execution: { command: x }\n",
        );
        assert!(c.contains(&"job.id_invalid"));
    }

    #[test]
    fn missing_command() {
        let c = codes(
            "version: 1\njobs:\n  - schedule: { every: day, at: \"09:00\" }\n    execution: { args: [a] }\n",
        );
        assert!(c.contains(&"execution.command_missing"));
    }

    #[test]
    fn bad_duration() {
        let c = codes(
            "version: 1\njobs:\n  - schedule: { every: day, at: \"09:00\" }\n    execution: { command: x }\n    timeout: 30x\n",
        );
        assert!(c.contains(&"duration.invalid"));
    }

    #[test]
    fn accumulates_multiple_semantic_errors() {
        let c = codes(
            "version: 1\njobs:\n  - schedule: { every: 45m }\n    execution: { args: [] }\n    timeout: 9z\n",
        );
        assert!(c.contains(&"schedule.non_divisor"));
        assert!(c.contains(&"execution.command_missing"));
        assert!(c.contains(&"duration.invalid"));
    }

    #[test]
    fn empty_jobs_list_is_valid() {
        // A config with version: 1 and no jobs is intentionally valid — no diagnostics.
        let c = codes("version: 1\njobs: []\n");
        assert!(
            c.is_empty(),
            "expected zero diagnostics for empty jobs, got: {c:?}"
        );
    }
}
