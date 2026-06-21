//! Desired-state configuration: YAML parsing, schema, normalization, defaults,
//! hashing, and migrations. Parsing is kept distinct from semantic validation.

use serde::Deserialize;

use crate::diagnostics::Diagnostic;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawConfig {
    pub(crate) version: u32,
    #[serde(default)]
    pub(crate) defaults: Option<RawDefaults>,
    pub(crate) jobs: Vec<RawJob>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawDefaults {
    pub(crate) timezone: Option<String>,
    pub(crate) timeout: Option<String>,
    pub(crate) overlap_policy: Option<String>,
    pub(crate) missed_run_policy: Option<String>,
    pub(crate) retry: Option<RawRetry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawJob {
    pub(crate) id: Option<String>,
    pub(crate) title: Option<String>,
    pub(crate) enabled: Option<bool>,
    pub(crate) schedule: RawSchedule,
    pub(crate) execution: RawExecution,
    pub(crate) timeout: Option<String>,
    pub(crate) missed_run_policy: Option<String>,
    pub(crate) overlap_policy: Option<String>,
    pub(crate) retry: Option<RawRetry>,
    #[serde(default)]
    pub(crate) tags: Vec<String>,
    #[serde(default)]
    #[allow(dead_code)] // consumed in phase 0.3 (metadata passthrough)
    pub(crate) metadata: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawSchedule {
    pub(crate) every: Option<Every>,
    pub(crate) at: Option<String>,
    pub(crate) on_day: Option<i64>,
    pub(crate) last_day: Option<bool>,
    pub(crate) cron: Option<String>,
    pub(crate) timezone: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub(crate) enum Every {
    One(String),
    Many(Vec<String>),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawExecution {
    pub(crate) command: Option<String>,
    #[serde(default)]
    pub(crate) args: Vec<String>,
    pub(crate) cwd: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RawRetry {
    pub(crate) max_retries: Option<i64>,
}

/// A parsed `every:` shorthand, enough to validate (not yet the normalized form).
pub(crate) enum Interval {
    Minutes(u32),
    Hours(u32),
    Named(String),
}

/// Parse one `every:` token. `Err(())` means the token is not a recognized
/// minute/hour interval or named keyword.
pub(crate) fn parse_every_interval(s: &str) -> Result<Interval, ()> {
    let s = s.trim();
    if let Some(n) = s.strip_suffix('m').and_then(|d| d.parse::<u32>().ok()) {
        return Ok(Interval::Minutes(n));
    }
    if let Some(n) = s.strip_suffix('h').and_then(|d| d.parse::<u32>().ok()) {
        return Ok(Interval::Hours(n));
    }
    const NAMED: &[&str] = &[
        "day",
        "weekday",
        "month",
        "monday",
        "tuesday",
        "wednesday",
        "thursday",
        "friday",
        "saturday",
        "sunday",
    ];
    if NAMED.contains(&s.to_lowercase().as_str()) {
        return Ok(Interval::Named(s.to_lowercase()));
    }
    Err(())
}

/// Parse a duration shorthand (`30s`, `15m`, `1h`, `2d`) into seconds.
pub(crate) fn parse_duration(s: &str) -> Result<u64, ()> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.find(|c: char| c.is_alphabetic()).ok_or(())?);
    let n: u64 = num.parse().map_err(|_| ())?;
    let mult = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => return Err(()),
    };
    Ok(n * mult)
}

// ─── Effective / normalized model ───────────────────────────────────────────

/// A schedule-complete, normalized schedule variant. Every schedule kind
/// supported by the YAML schema has exactly one representation here.
#[derive(Debug, PartialEq)]
#[allow(dead_code)]
pub(crate) enum NormalizedSchedule {
    /// `every: Nm` — fires every N minutes; N must be a divisor of 60.
    MinuteAligned { every_minutes: u32 },
    /// `every: Nh` — fires every N hours; N must be a divisor of 24.
    HourAligned { every_hours: u32 },
    /// Calendar-based schedule (daily, weekday, specific weekday(s), monthly).
    Calendar {
        /// Weekdays this schedule fires on, empty = every day.
        /// Elements: "day" | "weekday" | "monday" … "sunday".
        days: Vec<String>,
        /// Time-of-day as "HH:MM" (defaulted to "00:00" when absent).
        at: String,
        /// IANA timezone (defaulted to local when absent).
        timezone: Option<String>,
        /// Day-of-month for monthly schedules (None unless set explicitly).
        on_day: Option<i64>,
        /// True for "last day of month" monthly variant.
        last_day: bool,
    },
    /// Raw cron expression escape hatch.
    Cron {
        expression: String,
        timezone: Option<String>,
    },
}

/// Effective policy enum for overlap / missed-run handling.
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
pub(crate) enum OverlapPolicy {
    Skip,
    Queue,
    Kill,
}

#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)]
pub(crate) enum MissedRunPolicy {
    Skip,
    RunOnce,
    RunAll,
}

/// Durable, schedule-complete representation of a single job. All optional raw
/// fields have been resolved through the defaults-merge chain.
#[derive(Debug)]
#[allow(dead_code)]
pub(crate) struct EffectiveJob {
    /// Job id — may be None when the raw config omitted it.
    pub(crate) id: Option<String>,
    /// Human-readable title.
    pub(crate) title: Option<String>,
    /// Whether this job is active. Built-in default: true.
    pub(crate) enabled: bool,
    /// Normalized, schedule-complete schedule.
    pub(crate) schedule: NormalizedSchedule,
    /// Command to run.
    pub(crate) command: String,
    /// Command arguments.
    pub(crate) args: Vec<String>,
    /// Working directory for the command.
    pub(crate) cwd: Option<String>,
    /// Timeout in seconds after merging defaults. None = no limit.
    pub(crate) timeout_secs: Option<u64>,
    /// Effective timezone (IANA name or None = local).
    pub(crate) timezone: Option<String>,
    /// What to do when the previous run is still active.
    pub(crate) overlap_policy: OverlapPolicy,
    /// What to do when a scheduled run was missed.
    pub(crate) missed_run_policy: MissedRunPolicy,
    /// Max retry attempts after failure (0 = no retries).
    pub(crate) max_retries: u32,
    /// Tags for grouping / filtering.
    pub(crate) tags: Vec<String>,
}

/// The fully resolved config the scheduler consumes.
#[derive(Debug)]
#[allow(dead_code)]
pub(crate) struct EffectiveConfig {
    pub(crate) version: u32,
    pub(crate) jobs: Vec<EffectiveJob>,
}

// ─── normalize ───────────────────────────────────────────────────────────────

/// Build an [`EffectiveConfig`] from a validated [`RawConfig`].
///
/// Applies the merge order: **job field → top-level `defaults` → built-in default**.
/// Assumes the config has already been validated; unparseable values fall back
/// to built-in defaults rather than panicking (this is not a second validation pass).
#[allow(dead_code)]
pub(crate) fn normalize(raw: &RawConfig) -> EffectiveConfig {
    let defaults = raw.defaults.as_ref();

    let jobs = raw
        .jobs
        .iter()
        .map(|job| normalize_job(job, defaults))
        .collect();

    EffectiveConfig {
        version: raw.version,
        jobs,
    }
}

fn normalize_job(job: &RawJob, defaults: Option<&RawDefaults>) -> EffectiveJob {
    // ── enabled ──────────────────────────────────────────────────────────────
    let enabled = job.enabled.unwrap_or(true);

    // ── timeout ──────────────────────────────────────────────────────────────
    let timeout_secs = job
        .timeout
        .as_deref()
        .and_then(|s| parse_duration(s).ok())
        .or_else(|| {
            defaults
                .and_then(|d| d.timeout.as_deref())
                .and_then(|s| parse_duration(s).ok())
        });

    // ── timezone ─────────────────────────────────────────────────────────────
    let timezone = job
        .schedule
        .timezone
        .clone()
        .or_else(|| defaults.and_then(|d| d.timezone.clone()));

    // ── overlap_policy ───────────────────────────────────────────────────────
    let overlap_policy = parse_overlap_policy(
        job.overlap_policy
            .as_deref()
            .or_else(|| defaults.and_then(|d| d.overlap_policy.as_deref())),
    );

    // ── missed_run_policy ────────────────────────────────────────────────────
    let missed_run_policy = parse_missed_run_policy(
        job.missed_run_policy
            .as_deref()
            .or_else(|| defaults.and_then(|d| d.missed_run_policy.as_deref())),
    );

    // ── retry ────────────────────────────────────────────────────────────────
    let max_retries = job
        .retry
        .as_ref()
        .and_then(|r| r.max_retries)
        .or_else(|| defaults.and_then(|d| d.retry.as_ref()?.max_retries))
        .and_then(|n| u32::try_from(n).ok())
        .unwrap_or(0);

    // ── schedule ─────────────────────────────────────────────────────────────
    let schedule = normalize_schedule(&job.schedule, timezone.as_deref());

    // ── execution ────────────────────────────────────────────────────────────
    let command = job.execution.command.clone().unwrap_or_default();
    let args = job.execution.args.clone();
    let cwd = job.execution.cwd.clone();

    EffectiveJob {
        id: job.id.clone(),
        title: job.title.clone(),
        enabled,
        schedule,
        command,
        args,
        cwd,
        timeout_secs,
        timezone,
        overlap_policy,
        missed_run_policy,
        max_retries,
        tags: job.tags.clone(),
    }
}

fn normalize_schedule(raw: &RawSchedule, timezone: Option<&str>) -> NormalizedSchedule {
    // Cron takes priority when present.
    if let Some(expr) = &raw.cron {
        return NormalizedSchedule::Cron {
            expression: expr.clone(),
            timezone: raw.timezone.clone().or_else(|| timezone.map(str::to_owned)),
        };
    }

    if let Some(every) = &raw.every {
        let tokens: Vec<&str> = match every {
            Every::One(s) => vec![s.as_str()],
            Every::Many(v) => v.iter().map(String::as_str).collect(),
        };

        // Single token — could be minutes, hours, or a named day kind.
        if tokens.len() == 1 {
            match parse_every_interval(tokens[0]) {
                Ok(Interval::Minutes(n)) => {
                    return NormalizedSchedule::MinuteAligned { every_minutes: n };
                }
                Ok(Interval::Hours(n)) => {
                    return NormalizedSchedule::HourAligned { every_hours: n };
                }
                Ok(Interval::Named(name)) => {
                    return NormalizedSchedule::Calendar {
                        days: vec![name],
                        at: raw.at.clone().unwrap_or_else(|| "00:00".to_owned()),
                        timezone: raw.timezone.clone().or_else(|| timezone.map(str::to_owned)),
                        on_day: raw.on_day,
                        last_day: raw.last_day.unwrap_or(false),
                    };
                }
                Err(()) => {} // fall through to fallback
            }
        } else {
            // Multiple tokens — weekday list (e.g. ["monday", "wednesday"]).
            let days: Vec<String> = tokens
                .iter()
                .filter_map(|t| parse_every_interval(t).ok())
                .filter_map(|iv| match iv {
                    Interval::Named(name) => Some(name),
                    _ => None,
                })
                .collect();
            if !days.is_empty() {
                return NormalizedSchedule::Calendar {
                    days,
                    at: raw.at.clone().unwrap_or_else(|| "00:00".to_owned()),
                    timezone: raw.timezone.clone().or_else(|| timezone.map(str::to_owned)),
                    on_day: raw.on_day,
                    last_day: raw.last_day.unwrap_or(false),
                };
            }
        }
    }

    // Fallback — treat as daily calendar (validation is the real gate).
    NormalizedSchedule::Calendar {
        days: vec!["day".to_owned()],
        at: raw.at.clone().unwrap_or_else(|| "00:00".to_owned()),
        timezone: raw.timezone.clone().or_else(|| timezone.map(str::to_owned)),
        on_day: raw.on_day,
        last_day: raw.last_day.unwrap_or(false),
    }
}

fn parse_overlap_policy(s: Option<&str>) -> OverlapPolicy {
    match s {
        Some("queue") => OverlapPolicy::Queue,
        Some("kill") => OverlapPolicy::Kill,
        _ => OverlapPolicy::Skip, // built-in default
    }
}

fn parse_missed_run_policy(s: Option<&str>) -> MissedRunPolicy {
    match s {
        Some("run_once") => MissedRunPolicy::RunOnce,
        Some("run_all") => MissedRunPolicy::RunAll,
        _ => MissedRunPolicy::Skip, // built-in default
    }
}

// ─── config hashing ────────────────────────────────────────────────────────

/// SHA-256 (hex) over the canonical serialization of a job's runtime-affecting
/// effective config (`config-versioning-and-hashing` spec). Two jobs whose
/// runtime behavior is identical hash equally; purely presentational fields
/// (`title`, `tags`) and the job's identity (`id`) are excluded. This is the
/// per-job config identity used to correlate a run with the config that produced
/// it, and the projection identity stored in `jobs_state`.
pub(crate) fn job_config_hash(job: &EffectiveJob) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(canonical_job(job).as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// Render a job's runtime-affecting fields into a canonical, unambiguous string.
/// Field order is fixed and every value is delimited so distinct inputs cannot
/// collide (e.g. `args` are joined on the ASCII unit separator, never a space).
fn canonical_job(job: &EffectiveJob) -> String {
    use std::fmt::Write;
    let mut s = String::new();
    let _ = writeln!(s, "enabled={}", job.enabled);
    let _ = writeln!(s, "schedule={}", canonical_schedule(&job.schedule));
    let _ = writeln!(s, "command={}", job.command);
    let _ = writeln!(s, "args={}", job.args.join("\u{1f}"));
    let _ = writeln!(s, "cwd={}", job.cwd.as_deref().unwrap_or(""));
    let _ = writeln!(
        s,
        "timeout_secs={}",
        job.timeout_secs.map(|n| n.to_string()).unwrap_or_default()
    );
    let _ = writeln!(s, "timezone={}", job.timezone.as_deref().unwrap_or(""));
    let _ = writeln!(s, "overlap={:?}", job.overlap_policy);
    let _ = writeln!(s, "missed={:?}", job.missed_run_policy);
    let _ = writeln!(s, "max_retries={}", job.max_retries);
    s
}

/// Canonical, deterministic string form of a normalized schedule.
fn canonical_schedule(schedule: &NormalizedSchedule) -> String {
    match schedule {
        NormalizedSchedule::MinuteAligned { every_minutes } => format!("minute:{every_minutes}"),
        NormalizedSchedule::HourAligned { every_hours } => format!("hour:{every_hours}"),
        NormalizedSchedule::Calendar {
            days,
            at,
            timezone,
            on_day,
            last_day,
        } => format!(
            "calendar:days={}:at={at}:tz={}:on_day={}:last_day={last_day}",
            days.join(","),
            timezone.as_deref().unwrap_or(""),
            on_day.map(|d| d.to_string()).unwrap_or_default(),
        ),
        NormalizedSchedule::Cron {
            expression,
            timezone,
        } => format!("cron:{expression}:tz={}", timezone.as_deref().unwrap_or("")),
    }
}

// ─── parse ───────────────────────────────────────────────────────────────────

/// Parse raw YAML into the typed config. Structural problems (syntax, unknown /
/// missing / mistyped fields) surface as a single classified [`Diagnostic`];
/// semantic checks run later over the returned `RawConfig`.
#[allow(clippy::result_large_err)]
pub(crate) fn parse(yaml: &str) -> Result<RawConfig, Diagnostic> {
    serde_saphyr::from_str::<RawConfig>(yaml).map_err(classify_parse_error)
}

fn classify_parse_error(err: serde_saphyr::Error) -> Diagnostic {
    // Extract location before consuming the error (needed for WithSnippet unwrap).
    let loc = err
        .without_snippet()
        .location()
        .filter(|loc| *loc != serde_saphyr::Location::UNKNOWN)
        .map(|loc| (loc.line() as usize, loc.column() as usize));

    // Unwrap a WithSnippet wrapper to examine the structured inner variant.
    let inner = match err {
        serde_saphyr::Error::WithSnippet { error, .. } => *error,
        other => other,
    };

    let make = |code: &'static str, msg: String, path: String| -> Diagnostic {
        let d = Diagnostic::error(code, msg, path);
        match loc {
            Some((line, col)) => d.with_location(line, col),
            None => d,
        }
    };

    match inner {
        // Unknown field: use the field name as the path.
        serde_saphyr::Error::SerdeUnknownField { ref field, .. } => {
            make("field.unknown", inner.to_string(), field.clone())
        }
        // Missing required field: use the field name as the path.
        serde_saphyr::Error::SerdeMissingField { field, .. } => make(
            "field.missing",
            format!("missing field `{field}`"),
            field.to_string(),
        ),
        // Scan/syntax errors arrive as ExternalMessage (parser-level) or Message.
        serde_saphyr::Error::ExternalMessage { ref msg, .. }
        | serde_saphyr::Error::Message { ref msg, .. } => {
            make("yaml.syntax", msg.clone(), ".".to_string())
        }
        // Structural EOF / unexpected token errors are syntax errors.
        serde_saphyr::Error::Eof { .. }
        | serde_saphyr::Error::Unexpected { .. }
        | serde_saphyr::Error::UnexpectedSequenceEnd { .. }
        | serde_saphyr::Error::UnexpectedMappingEnd { .. } => {
            make("yaml.syntax", inner.to_string(), ".".to_string())
        }
        // Scalar parse failure: the field value has the right YAML kind (scalar) but
        // cannot be converted to the target Rust type (e.g. a string where u32 is
        // expected). serde_saphyr emits Error::InvalidScalar for these; neither this
        // variant nor SerdeInvalidType/SerdeInvalidValue carries a field name (the
        // serde de::Error::invalid_type() method has no field-name parameter), so
        // path falls back to the best-effort sentinel ".".
        serde_saphyr::Error::InvalidScalar { ref ty, .. } => make(
            "field.type",
            format!("invalid value: expected {ty}"),
            ".".to_string(),
        ),
        // Serde-level type/value mismatch (e.g. a sequence or mapping where a scalar
        // was expected, raised via the serde Visitor path).
        serde_saphyr::Error::SerdeInvalidType {
            ref unexpected,
            ref expected,
            ..
        }
        | serde_saphyr::Error::SerdeInvalidValue {
            ref unexpected,
            ref expected,
            ..
        } => make(
            "field.type",
            format!("invalid type: expected {expected}, got {unexpected}"),
            ".".to_string(),
        ),
        // Fallback: heuristic on the rendered message.
        _ => {
            let msg = inner.to_string();
            let lower = msg.to_lowercase();
            let code = if lower.contains("unknown field") {
                "field.unknown"
            } else if lower.contains("missing field") {
                "field.missing"
            } else if lower.contains("invalid type") || lower.contains("invalid value") {
                "field.type"
            } else {
                "yaml.syntax"
            };
            make(code, msg, ".".to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_minimal_config() {
        let raw = parse("version: 1\njobs:\n  - schedule:\n      every: 15m\n    execution:\n      command: ~/bin/x.sh\n").unwrap();
        assert_eq!(raw.version, 1);
        assert_eq!(raw.jobs.len(), 1);
    }

    #[test]
    fn malformed_yaml_is_syntax_error() {
        let d = parse("version: 1\njobs: [unclosed").unwrap_err();
        assert_eq!(d.code, "yaml.syntax");
    }

    #[test]
    fn unknown_field_rejected() {
        let d = parse("version: 1\nbogus: true\njobs: []\n").unwrap_err();
        assert_eq!(d.code, "field.unknown");
        assert!(d.path.contains("bogus"));
    }

    #[test]
    fn missing_required_field() {
        let d = parse("jobs: []\n").unwrap_err();
        assert_eq!(d.code, "field.missing");
        assert_eq!(d.path, "version");
    }

    #[test]
    fn type_mismatch_is_type_error() {
        // version expects u32 but receives a string that can't be parsed as one.
        // serde_saphyr emits Error::InvalidScalar for scalar parse failures; neither
        // that variant nor SerdeInvalidType carries a field name (the serde
        // de::Error::invalid_type() method has no field-name parameter), so path
        // stays at the best-effort sentinel ".".
        let d = parse("version: \"a string\"\njobs: []\n").unwrap_err();
        assert_eq!(d.code, "field.type");
        assert_eq!(d.path, ".");
    }
}

#[cfg(test)]
mod normalize_tests {
    use super::*;

    #[test]
    fn minute_schedule_normalizes_and_defaults_apply() {
        let raw = parse("version: 1\ndefaults: { timeout: 10m }\njobs:\n  - id: j\n    schedule: { every: 15m }\n    execution: { command: x }\n").unwrap();
        let eff = normalize(&raw);
        let job = &eff.jobs[0];
        assert!(matches!(
            job.schedule,
            NormalizedSchedule::MinuteAligned { every_minutes: 15 }
        ));
        assert_eq!(job.timeout_secs, Some(600)); // top-level default applied
        assert!(job.enabled); // built-in default
    }

    #[test]
    fn job_override_beats_top_level_default() {
        let raw = parse("version: 1\ndefaults: { timeout: 10m }\njobs:\n  - schedule: { every: day, at: \"09:00\" }\n    execution: { command: x }\n    timeout: 30m\n").unwrap();
        let eff = normalize(&raw);
        assert_eq!(eff.jobs[0].timeout_secs, Some(1800));
    }

    #[test]
    fn cron_schedule_normalizes() {
        let raw = parse("version: 1\njobs:\n  - schedule: { cron: \"0 9 * * 1-5\" }\n    execution: { command: x }\n").unwrap();
        let eff = normalize(&raw);
        assert!(matches!(
            &eff.jobs[0].schedule,
            NormalizedSchedule::Cron { expression, .. } if expression == "0 9 * * 1-5"
        ));
    }

    #[test]
    fn daily_calendar_schedule_normalizes() {
        let raw = parse("version: 1\njobs:\n  - schedule: { every: day, at: \"08:00\" }\n    execution: { command: x }\n").unwrap();
        let eff = normalize(&raw);
        assert!(matches!(
            &eff.jobs[0].schedule,
            NormalizedSchedule::Calendar { days, at, .. }
                if days == &["day"] && at == "08:00"
        ));
    }

    #[test]
    fn built_in_defaults_apply_when_no_top_level_defaults() {
        let raw = parse(
            "version: 1\njobs:\n  - schedule: { every: 30m }\n    execution: { command: x }\n",
        )
        .unwrap();
        let eff = normalize(&raw);
        let job = &eff.jobs[0];
        assert!(job.enabled);
        assert_eq!(job.timeout_secs, None);
        assert_eq!(job.overlap_policy, OverlapPolicy::Skip);
        assert_eq!(job.missed_run_policy, MissedRunPolicy::Skip);
        assert_eq!(job.max_retries, 0);
    }

    #[test]
    fn hour_aligned_schedule_normalizes() {
        let raw = parse(
            "version: 1\njobs:\n  - schedule: { every: 2h }\n    execution: { command: x }\n",
        )
        .unwrap();
        let eff = normalize(&raw);
        assert!(matches!(
            eff.jobs[0].schedule,
            NormalizedSchedule::HourAligned { every_hours: 2 }
        ));
    }

    #[test]
    fn monthly_schedule_normalizes_to_calendar_with_on_day() {
        let raw = parse(
            "version: 1\njobs:\n  - schedule: { every: month, on_day: 15, at: \"09:00\" }\n    execution: { command: x }\n",
        )
        .unwrap();
        let eff = normalize(&raw);
        assert!(matches!(
            &eff.jobs[0].schedule,
            NormalizedSchedule::Calendar { days, at, on_day, last_day, .. }
                if days == &["month"] && at == "09:00" && *on_day == Some(15) && !last_day
        ));
    }

    #[test]
    fn multiple_weekday_schedule_normalizes_to_calendar_with_all_days() {
        let raw = parse(
            "version: 1\njobs:\n  - schedule:\n      every: [monday, wednesday, friday]\n      at: \"09:00\"\n    execution: { command: x }\n",
        )
        .unwrap();
        let eff = normalize(&raw);
        assert!(matches!(
            &eff.jobs[0].schedule,
            NormalizedSchedule::Calendar { days, at, .. }
                if days == &["monday", "wednesday", "friday"] && at == "09:00"
        ));
    }
}

#[cfg(test)]
mod hash_tests {
    use super::*;

    /// A baseline effective job; tests mutate one field to isolate its effect.
    fn job() -> EffectiveJob {
        EffectiveJob {
            id: Some("job".to_owned()),
            title: Some("Title".to_owned()),
            enabled: true,
            schedule: NormalizedSchedule::MinuteAligned { every_minutes: 15 },
            command: "run.sh".to_owned(),
            args: vec!["--flag".to_owned()],
            cwd: Some("~/work".to_owned()),
            timeout_secs: Some(600),
            timezone: Some("UTC".to_owned()),
            overlap_policy: OverlapPolicy::Skip,
            missed_run_policy: MissedRunPolicy::Skip,
            max_retries: 0,
            tags: vec!["nightly".to_owned()],
        }
    }

    #[test]
    fn hash_is_deterministic() {
        assert_eq!(job_config_hash(&job()), job_config_hash(&job()));
    }

    #[test]
    fn hash_is_sha256_hex() {
        let h = job_config_hash(&job());
        assert_eq!(h.len(), 64, "expected 64 hex chars, got {h:?}");
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn hash_changes_when_schedule_changes() {
        let mut other = job();
        other.schedule = NormalizedSchedule::MinuteAligned { every_minutes: 30 };
        assert_ne!(job_config_hash(&job()), job_config_hash(&other));
    }

    #[test]
    fn hash_changes_when_command_changes() {
        let mut other = job();
        other.command = "other.sh".to_owned();
        assert_ne!(job_config_hash(&job()), job_config_hash(&other));
    }

    #[test]
    fn hash_changes_when_enabled_changes() {
        let mut other = job();
        other.enabled = false;
        assert_ne!(job_config_hash(&job()), job_config_hash(&other));
    }

    #[test]
    fn hash_ignores_presentational_fields() {
        let mut other = job();
        other.title = Some("A different title".to_owned());
        other.tags = vec!["different".to_owned(), "tags".to_owned()];
        other.id = Some("different-id".to_owned());
        assert_eq!(
            job_config_hash(&job()),
            job_config_hash(&other),
            "title/tags/id must not affect the config hash"
        );
    }

    #[test]
    fn hash_distinguishes_arg_boundaries() {
        let mut a = job();
        a.args = vec!["a".to_owned(), "b".to_owned()];
        let mut b = job();
        b.args = vec!["a b".to_owned()];
        assert_ne!(
            job_config_hash(&a),
            job_config_hash(&b),
            "['a','b'] must not collide with ['a b']"
        );
    }
}
