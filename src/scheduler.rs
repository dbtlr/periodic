//! Schedule computation and the scheduler loop: next-run calculation,
//! wall-clock alignment, occurrence identity, missed-run detection, and
//! clock-jump/DST handling. Emits run intents; never spawns processes.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::str::FromStr;

use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, TimeZone, Timelike, Utc, Weekday};
use chrono_tz::Tz;

use crate::config::{EffectiveConfig, EffectiveJob, NormalizedSchedule};

/// Resolve an optional IANA timezone name to a concrete [`Tz`].
///
/// `None` (or the validated `"local"` sentinel) means the job did not pin a
/// zone, so the system local zone is used. Names are already validated upstream
/// (decision 0001 / 0.2 validation), so an unparseable name or undetectable
/// local zone falls back to UTC purely defensively — the engine never panics on
/// a bad zone.
pub(crate) fn resolve_timezone(name: Option<&str>) -> Tz {
    match name {
        None | Some("local") => iana_time_zone::get_timezone()
            .ok()
            .and_then(|local| Tz::from_str(&local).ok())
            .unwrap_or(Tz::UTC),
        Some(tz) => Tz::from_str(tz).unwrap_or(Tz::UTC),
    }
}

/// The schedule family that produced an occurrence. Appears verbatim in the
/// [`Occurrence::key`], so the four computation paths share one set of labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // variants wired up by the per-kind computation tasks (PDC-42..44)
pub(crate) enum ScheduleKind {
    Minute,
    Hour,
    Calendar,
    Cron,
}

impl ScheduleKind {
    #[allow(dead_code)] // consumed via Occurrence::new by the computation tasks
    fn as_str(self) -> &'static str {
        match self {
            ScheduleKind::Minute => "minute",
            ScheduleKind::Hour => "hour",
            ScheduleKind::Calendar => "calendar",
            ScheduleKind::Cron => "cron",
        }
    }
}

/// A computed scheduled firing: the absolute instant plus its deterministic
/// occurrence key. The key embeds the offset-qualified RFC 3339 instant so DST
/// folds map to distinct keys (decision 0005).
#[derive(Debug, Clone, PartialEq)]
#[allow(dead_code)] // consumed by the engine API + state persistence (PDC-45 / 0.4)
pub(crate) struct Occurrence {
    pub(crate) scheduled_for: DateTime<Tz>,
    pub(crate) key: String,
}

impl Occurrence {
    #[allow(dead_code)] // consumed by the engine API + state persistence (PDC-45 / 0.4)
    pub(crate) fn new(job_id: &str, kind: ScheduleKind, scheduled_for: DateTime<Tz>) -> Self {
        let key = format!("{job_id}:{}:{}", kind.as_str(), scheduled_for.to_rfc3339());
        Occurrence { scheduled_for, key }
    }
}

/// Resolve a wall-clock local datetime to a concrete instant in `tz`, applying
/// the DST policy of decision 0005.
#[allow(dead_code)] // consumed by the calendar/aligned computation tasks (PDC-42/43)
pub(crate) fn resolve_wall_clock(naive: NaiveDateTime, tz: Tz) -> DateTime<Tz> {
    use chrono::offset::LocalResult;

    match tz.from_local_datetime(&naive) {
        LocalResult::Single(dt) => dt,
        // Spring-forward gap: the wall-clock time does not exist. Walk forward a
        // minute at a time to the first valid instant (the gap boundary).
        LocalResult::None => {
            let mut probe = naive;
            loop {
                probe += chrono::Duration::minutes(1);
                if let LocalResult::Single(dt) = tz.from_local_datetime(&probe) {
                    return dt;
                }
            }
        }
        // Fall-back fold: the wall-clock time occurs twice. Fire once, on the
        // earlier offset.
        LocalResult::Ambiguous(earliest, _latest) => earliest,
    }
}

/// Next minute-aligned firing strictly after `after`, in that instant's zone.
///
/// `every_minutes` divides 60 (enforced by 0.2 validation), so boundaries fall
/// at `:00, :every, :2·every, …` within each hour. The result is the first such
/// wall-clock boundary later than `after`, resolved through the DST policy.
#[allow(dead_code)] // consumed by the engine API dispatch (PDC-45)
pub(crate) fn next_minute_aligned(every_minutes: u32, after: DateTime<Tz>) -> DateTime<Tz> {
    let tz = after.timezone();
    let wall = after.naive_local();
    // Next boundary minute-of-hour strictly past the current minute. Since
    // `every` divides 60, this lands in (0, 60]; 60 means minute 0 of next hour.
    let next_min = (wall.minute() / every_minutes + 1) * every_minutes;
    let base = wall.date().and_hms_opt(wall.hour(), 0, 0).unwrap();
    let boundary = base + chrono::Duration::minutes(i64::from(next_min));
    resolve_wall_clock(boundary, tz)
}

/// Next hour-aligned firing strictly after `after`, in that instant's zone.
///
/// `every_hours` divides 24 (enforced by 0.2 validation), so boundaries fall at
/// `00:00, every:00, 2·every:00, …`. The result is the first such wall-clock
/// boundary later than `after`, resolved through the DST policy.
#[allow(dead_code)] // consumed by the engine API dispatch (PDC-45)
pub(crate) fn next_hour_aligned(every_hours: u32, after: DateTime<Tz>) -> DateTime<Tz> {
    let tz = after.timezone();
    let wall = after.naive_local();
    // Next boundary hour strictly past the current hour, in (0, 24]; 24 means
    // hour 0 of the next day.
    let next_hour = (wall.hour() / every_hours + 1) * every_hours;
    let base = wall.date().and_hms_opt(0, 0, 0).unwrap();
    let boundary = base + chrono::Duration::hours(i64::from(next_hour));
    resolve_wall_clock(boundary, tz)
}

/// Parse a validated `"HH:MM"` into `(hour, minute)`, defaulting to midnight if
/// it is somehow malformed (validation already rejects bad values; the engine
/// stays total regardless).
fn parse_hhmm(at: &str) -> (u32, u32) {
    at.split_once(':')
        .and_then(|(h, m)| Some((h.parse().ok()?, m.parse().ok()?)))
        .unwrap_or((0, 0))
}

/// Map a weekday name to [`Weekday`]. Returns `None` for non-weekday tokens
/// (`"day"`, `"weekday"`, `"month"`), which are handled by the caller.
fn weekday_from_name(name: &str) -> Option<Weekday> {
    Some(match name {
        "monday" => Weekday::Mon,
        "tuesday" => Weekday::Tue,
        "wednesday" => Weekday::Wed,
        "thursday" => Weekday::Thu,
        "friday" => Weekday::Fri,
        "saturday" => Weekday::Sat,
        "sunday" => Weekday::Sun,
        _ => return None,
    })
}

/// Whether a calendar `days` set fires on `weekday`. `"day"` means every day;
/// `"weekday"` means Monday–Friday; specific names match themselves.
fn calendar_fires_on(days: &[String], weekday: Weekday) -> bool {
    days.iter().any(|d| match d.as_str() {
        "day" => true,
        "weekday" => !matches!(weekday, Weekday::Sat | Weekday::Sun),
        name => weekday_from_name(name) == Some(weekday),
    })
}

/// Last calendar day (28–31) of the given month.
fn last_day_of_month(year: i32, month: u32) -> u32 {
    let (ny, nm) = if month == 12 {
        (year + 1, 1)
    } else {
        (year, month + 1)
    };
    // First of next month minus one day is the last day of this month.
    NaiveDate::from_ymd_opt(ny, nm, 1)
        .unwrap()
        .pred_opt()
        .unwrap()
        .day()
}

/// Next calendar firing strictly after `after`, applying decision 0001
/// (wall-clock in the schedule's `tz`) and decision 0005 (DST resolution).
///
/// A `days` set containing `"month"` is a monthly schedule (fires on `on_day`,
/// or the last day when `last_day`); otherwise `days` is a weekday set
/// (`"day"`, `"weekday"`, or named weekdays). The engine is total: every
/// schedule yields a next occurrence.
#[allow(dead_code)] // consumed by the engine API dispatch (PDC-45)
pub(crate) fn next_calendar(
    days: &[String],
    at: &str,
    tz: Tz,
    on_day: Option<i64>,
    last_day: bool,
    after: DateTime<Tz>,
) -> DateTime<Tz> {
    let (hour, minute) = parse_hhmm(at);
    let local = after.with_timezone(&tz);

    if days.iter().any(|d| d == "month") {
        // Monthly: walk forward month by month until the target day exists and
        // its instant is strictly after `after`.
        let (mut year, mut month) = (local.year(), local.month());
        loop {
            let day = if last_day {
                last_day_of_month(year, month)
            } else {
                on_day.filter(|d| (1..=31).contains(d)).unwrap_or(1) as u32
            };
            if let Some(date) = NaiveDate::from_ymd_opt(year, month, day) {
                let candidate = resolve_wall_clock(date.and_hms_opt(hour, minute, 0).unwrap(), tz);
                if candidate > after {
                    return candidate;
                }
            }
            (year, month) = if month == 12 {
                (year + 1, 1)
            } else {
                (year, month + 1)
            };
        }
    }

    // Weekday set: walk forward day by day to the next matching weekday whose
    // instant is strictly after `after`.
    let mut date = local.date_naive();
    loop {
        if calendar_fires_on(days, date.weekday()) {
            let candidate = resolve_wall_clock(date.and_hms_opt(hour, minute, 0).unwrap(), tz);
            if candidate > after {
                return candidate;
            }
        }
        date = date.succ_opt().unwrap();
    }
}

/// Next cron firing strictly after `after`, evaluated in `tz`.
///
/// Returns `None` if the expression cannot be parsed (defensive — validation
/// already rejects bad expressions) or has no future occurrence. croner owns
/// cron's own DST semantics, so the resolver above is not involved here.
#[allow(dead_code)] // consumed by the engine API dispatch (PDC-45)
pub(crate) fn next_cron(expression: &str, tz: Tz, after: DateTime<Tz>) -> Option<DateTime<Tz>> {
    let cron = croner::Cron::from_str(expression).ok()?;
    cron.find_next_occurrence(&after.with_timezone(&tz), false)
        .ok()
}

/// The schedule-computation engine: the next [`Occurrence`] for `schedule`
/// strictly after `after`, or `None` when none exists (only cron can be empty).
///
/// `default_tz` is the job's resolved timezone — it drives the aligned kinds
/// (which carry no zone of their own) and is the fallback for calendar/cron
/// schedules that don't pin one. This is the single entry point the scheduler
/// loop (phase 0.6) calls; everything above is an internal per-kind helper.
#[allow(dead_code)] // consumed by the scheduler loop (PDC-8 / phase 0.6)
pub(crate) fn next_occurrence(
    job_id: &str,
    schedule: &NormalizedSchedule,
    default_tz: Tz,
    after: DateTime<Tz>,
) -> Option<Occurrence> {
    let pinned_tz = |tz: &Option<String>| {
        tz.as_deref()
            .map_or(default_tz, |t| resolve_timezone(Some(t)))
    };

    let (instant, kind) = match schedule {
        NormalizedSchedule::MinuteAligned { every_minutes } => (
            next_minute_aligned(*every_minutes, after.with_timezone(&default_tz)),
            ScheduleKind::Minute,
        ),
        NormalizedSchedule::HourAligned { every_hours } => (
            next_hour_aligned(*every_hours, after.with_timezone(&default_tz)),
            ScheduleKind::Hour,
        ),
        NormalizedSchedule::Calendar {
            days,
            at,
            timezone,
            on_day,
            last_day,
        } => (
            next_calendar(days, at, pinned_tz(timezone), *on_day, *last_day, after),
            ScheduleKind::Calendar,
        ),
        NormalizedSchedule::Cron {
            expression,
            timezone,
        } => (
            next_cron(expression, pinned_tz(timezone), after)?,
            ScheduleKind::Cron,
        ),
    };
    // Occurrences identify a scheduled instant, never a sub-second moment. The
    // cron path derives its result from `after` and can inherit its sub-second
    // component; zero it so `occurrence_key` is deterministic regardless of when
    // the computation ran. The wall-clock kinds already land on whole seconds.
    let instant = instant.with_nanosecond(0).unwrap_or(instant);
    Some(Occurrence::new(job_id, kind, instant))
}

// ─── the scheduler loop core: in-memory schedule table ───────────────────────
// The daemon (PDC-74) drives this on its own thread: sleep until `next_wake`,
// then dispatch `pop_due` to the executor. The table is the "boring" scheduler —
// it computes occurrences and emits intents; it never spawns processes.

/// A due firing the daemon should dispatch to the executor. Owns a clone of the
/// job so it can be handed to a run thread; `occurrence_key` feeds `create_run`'s
/// dedupe; `scheduled_for` is the occurrence instant (UTC).
#[allow(dead_code)] // consumed by the daemon dispatch loop in PDC-74
pub(crate) struct DueRun {
    pub(crate) job: EffectiveJob,
    pub(crate) occurrence_key: String,
    pub(crate) scheduled_for: DateTime<Utc>,
}

/// One scheduled job and its upcoming firing, ordered by `scheduled_for` (then id
/// for determinism) so a min-heap surfaces the earliest-due job.
struct Entry {
    scheduled_for: DateTime<Tz>,
    key: String,
    job: EffectiveJob,
}

impl PartialEq for Entry {
    fn eq(&self, other: &Self) -> bool {
        self.scheduled_for == other.scheduled_for && self.key == other.key
    }
}
impl Eq for Entry {}
impl Ord for Entry {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.scheduled_for
            .cmp(&other.scheduled_for)
            .then_with(|| self.key.cmp(&other.key))
    }
}
impl PartialOrd for Entry {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// In-memory table of scheduled jobs, keyed by next firing. A `BinaryHeap` with
/// `Reverse` makes it a min-heap, so the earliest-due job is always at the top.
#[allow(dead_code)] // consumed by the daemon loop in PDC-74
pub(crate) struct ScheduleTable {
    heap: BinaryHeap<Reverse<Entry>>,
}

#[allow(dead_code)] // methods consumed by the daemon loop in PDC-74
impl ScheduleTable {
    /// Build the table from the effective config, computing each enabled, keyed
    /// job's first occurrence after `now`. Disabled and unkeyed jobs are skipped.
    pub(crate) fn build(config: &EffectiveConfig, now: DateTime<Utc>) -> Self {
        let mut heap = BinaryHeap::new();
        for job in &config.jobs {
            if !job.enabled {
                continue;
            }
            let Some(job_id) = job.id.as_deref() else {
                continue;
            };
            let tz = resolve_timezone(job.timezone.as_deref());
            if let Some(occ) = next_occurrence(job_id, &job.schedule, tz, now.with_timezone(&tz)) {
                heap.push(Reverse(Entry {
                    scheduled_for: occ.scheduled_for,
                    key: occ.key,
                    job: job.clone(),
                }));
            }
        }
        ScheduleTable { heap }
    }

    /// Number of scheduled jobs in the table.
    pub(crate) fn len(&self) -> usize {
        self.heap.len()
    }

    /// The earliest upcoming firing instant, or `None` when nothing is scheduled.
    /// The daemon sleeps until this (or a control event) before the next tick.
    pub(crate) fn next_wake(&self) -> Option<DateTime<Utc>> {
        self.heap
            .peek()
            .map(|Reverse(e)| e.scheduled_for.with_timezone(&Utc))
    }

    /// Pop every job whose firing is due at `now`, returning one `DueRun` each and
    /// rescheduling it to its next occurrence strictly after `now`. Intermediate
    /// missed occurrences collapse into the single returned run (no backlog); the
    /// missed-run *policy* (skip vs run-once) is applied by the caller in PDC-71.
    pub(crate) fn pop_due(&mut self, now: DateTime<Utc>) -> Vec<DueRun> {
        let mut due = Vec::new();
        loop {
            let is_due = match self.heap.peek() {
                Some(Reverse(e)) => e.scheduled_for.with_timezone(&Utc) <= now,
                None => false,
            };
            if !is_due {
                break;
            }
            let Reverse(entry) = self.heap.pop().expect("peeked a due entry");
            due.push(DueRun {
                job: entry.job.clone(),
                occurrence_key: entry.key.clone(),
                scheduled_for: entry.scheduled_for.with_timezone(&Utc),
            });
            let job_id = entry.job.id.as_deref().unwrap_or("");
            let tz = resolve_timezone(entry.job.timezone.as_deref());
            if let Some(occ) =
                next_occurrence(job_id, &entry.job.schedule, tz, now.with_timezone(&tz))
            {
                self.heap.push(Reverse(Entry {
                    scheduled_for: occ.scheduled_for,
                    key: occ.key,
                    job: entry.job,
                }));
            }
        }
        due
    }
}

#[cfg(test)]
mod schedule_table_tests {
    use super::*;
    use crate::config::{MissedRunPolicy, OverlapPolicy};
    use chrono::Utc;

    fn job(id: &str, every_minutes: u32, enabled: bool) -> EffectiveJob {
        EffectiveJob {
            id: Some(id.into()),
            title: None,
            enabled,
            schedule: NormalizedSchedule::MinuteAligned { every_minutes },
            command: "true".into(),
            args: vec![],
            cwd: None,
            timeout_secs: None,
            timezone: Some("UTC".into()),
            overlap_policy: OverlapPolicy::Skip,
            missed_run_policy: MissedRunPolicy::Skip,
            max_retries: 0,
            tags: vec![],
        }
    }

    fn at(h: u32, m: u32, s: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 21, h, m, s).unwrap()
    }

    fn cfg(jobs: Vec<EffectiveJob>) -> EffectiveConfig {
        EffectiveConfig { version: 1, jobs }
    }

    #[test]
    fn build_includes_only_enabled_keyed_jobs() {
        let mut unkeyed = job("x", 15, true);
        unkeyed.id = None;
        let table = ScheduleTable::build(
            &cfg(vec![job("a", 15, true), job("b", 15, false), unkeyed]),
            at(10, 7, 0),
        );
        assert_eq!(
            table.len(),
            1,
            "disabled and unkeyed jobs are not scheduled"
        );
    }

    #[test]
    fn next_wake_is_the_earliest_occurrence() {
        // a every 15m -> 10:15, b every 5m -> 10:10; at 10:07 the earliest is 10:10.
        let table = ScheduleTable::build(
            &cfg(vec![job("a", 15, true), job("b", 5, true)]),
            at(10, 7, 0),
        );
        assert_eq!(table.next_wake(), Some(at(10, 10, 0)));
    }

    #[test]
    fn pop_due_fires_due_job_and_reschedules() {
        let mut table = ScheduleTable::build(&cfg(vec![job("a", 15, true)]), at(10, 7, 0)); // next 10:15
        assert!(table.pop_due(at(10, 14, 59)).is_empty(), "not due yet");
        let due = table.pop_due(at(10, 15, 0));
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].scheduled_for, at(10, 15, 0));
        assert_eq!(due[0].job.id.as_deref(), Some("a"));
        assert_eq!(
            table.next_wake(),
            Some(at(10, 30, 0)),
            "rescheduled to the next boundary"
        );
    }

    #[test]
    fn missed_occurrences_collapse_to_one_run() {
        // Built at 10:00 -> next 10:15; clock jumps to 11:02 -> a single run, and the
        // job reschedules past now to 11:15 (no backlog of 10:15/10:30/10:45/11:00).
        let mut table = ScheduleTable::build(&cfg(vec![job("a", 15, true)]), at(10, 0, 0));
        let due = table.pop_due(at(11, 2, 0));
        assert_eq!(due.len(), 1, "missed occurrences collapse to a single run");
        assert_eq!(table.next_wake(), Some(at(11, 15, 0)));
    }

    #[test]
    fn due_run_carries_occurrence_key() {
        let mut table = ScheduleTable::build(&cfg(vec![job("a", 15, true)]), at(10, 7, 0));
        let due = table.pop_due(at(10, 15, 0));
        assert!(
            due[0].occurrence_key.starts_with("a:minute:"),
            "got {}",
            due[0].occurrence_key
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono_tz::Tz;

    fn naive(y: i32, m: u32, d: u32, h: u32, min: u32) -> chrono::NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(h, min, 0)
            .unwrap()
    }

    /// A concrete UTC instant at the given wall-clock time (seconds default 0).
    fn utc(y: i32, m: u32, d: u32, h: u32, min: u32) -> DateTime<Tz> {
        resolve_wall_clock(naive(y, m, d, h, min), Tz::UTC)
    }

    /// A UTC instant with explicit seconds, for strict-after edge cases.
    fn utc_s(y: i32, m: u32, d: u32, h: u32, min: u32, s: u32) -> DateTime<Tz> {
        let nd = NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(h, min, s)
            .unwrap();
        resolve_wall_clock(nd, Tz::UTC)
    }

    #[test]
    fn resolve_timezone_parses_a_named_zone() {
        assert_eq!(
            resolve_timezone(Some("America/New_York")),
            Tz::America__New_York
        );
    }

    #[test]
    fn resolve_timezone_treats_local_sentinel_as_system_zone() {
        // "local" is the validated sentinel for the system zone (same as None).
        assert_eq!(resolve_timezone(Some("local")), resolve_timezone(None));
    }

    #[test]
    fn resolve_wall_clock_returns_unambiguous_local_time() {
        // A plain winter morning in New York — EST, -05:00, no DST nearby.
        let dt = resolve_wall_clock(naive(2026, 1, 15, 9, 0), Tz::America__New_York);
        assert_eq!(dt.to_rfc3339(), "2026-01-15T09:00:00-05:00");
    }

    #[test]
    fn resolve_wall_clock_advances_through_a_spring_forward_gap() {
        // 2026-03-08: New York jumps 02:00 EST -> 03:00 EDT; 02:30 never exists.
        // Decision 0005: advance to the next valid instant — 03:00 EDT.
        let dt = resolve_wall_clock(naive(2026, 3, 8, 2, 30), Tz::America__New_York);
        assert_eq!(dt.to_rfc3339(), "2026-03-08T03:00:00-04:00");
    }

    #[test]
    fn resolve_wall_clock_picks_the_earlier_offset_on_a_fall_back_fold() {
        // 2026-11-01: New York falls back 02:00 EDT -> 01:00 EST; 01:30 occurs
        // twice. Decision 0005: fire once on the earlier offset — EDT (-04:00).
        let dt = resolve_wall_clock(naive(2026, 11, 1, 1, 30), Tz::America__New_York);
        assert_eq!(dt.to_rfc3339(), "2026-11-01T01:30:00-04:00");
    }

    #[test]
    fn occurrence_carries_the_instant_and_an_offset_qualified_key() {
        let dt = resolve_wall_clock(naive(2026, 1, 15, 9, 0), Tz::America__New_York);
        let occ = Occurrence::new("backup", ScheduleKind::Calendar, dt);
        assert_eq!(occ.scheduled_for, dt);
        // <job-id>:<kind>:<scheduled-for-rfc3339> — the offset makes DST folds distinct.
        assert_eq!(occ.key, "backup:calendar:2026-01-15T09:00:00-05:00");
    }

    // ── minute-aligned ──────────────────────────────────────────────────────

    #[test]
    fn minute_aligned_picks_the_next_boundary_within_the_hour() {
        assert_eq!(
            next_minute_aligned(15, utc(2026, 1, 15, 9, 7)),
            utc(2026, 1, 15, 9, 15)
        );
    }

    #[test]
    fn minute_aligned_is_strict_when_already_on_a_boundary() {
        assert_eq!(
            next_minute_aligned(15, utc(2026, 1, 15, 9, 15)),
            utc(2026, 1, 15, 9, 30)
        );
    }

    #[test]
    fn minute_aligned_advances_to_a_boundary_when_seconds_remain() {
        // 09:15:30 has passed the :15 boundary -> next is :30.
        assert_eq!(
            next_minute_aligned(15, utc_s(2026, 1, 15, 9, 15, 30)),
            utc(2026, 1, 15, 9, 30)
        );
    }

    #[test]
    fn minute_aligned_rolls_across_the_day() {
        assert_eq!(
            next_minute_aligned(30, utc(2026, 1, 15, 23, 45)),
            utc(2026, 1, 16, 0, 0)
        );
    }

    // ── hour-aligned ────────────────────────────────────────────────────────

    #[test]
    fn hour_aligned_picks_the_next_boundary() {
        assert_eq!(
            next_hour_aligned(6, utc(2026, 1, 15, 6, 30)),
            utc(2026, 1, 15, 12, 0)
        );
    }

    #[test]
    fn hour_aligned_is_strict_when_already_on_a_boundary() {
        assert_eq!(
            next_hour_aligned(6, utc(2026, 1, 15, 12, 0)),
            utc(2026, 1, 15, 18, 0)
        );
    }

    #[test]
    fn hour_aligned_rolls_across_the_day() {
        assert_eq!(
            next_hour_aligned(6, utc(2026, 1, 15, 18, 30)),
            utc(2026, 1, 16, 0, 0)
        );
    }

    // ── calendar ────────────────────────────────────────────────────────────

    fn cal(
        days: &[&str],
        at: &str,
        on_day: Option<i64>,
        last_day: bool,
        after: DateTime<Tz>,
    ) -> DateTime<Tz> {
        let days: Vec<String> = days.iter().map(|s| s.to_string()).collect();
        next_calendar(&days, at, Tz::UTC, on_day, last_day, after)
    }

    #[test]
    fn calendar_daily_fires_at_the_time_today_then_tomorrow() {
        assert_eq!(
            cal(&["day"], "09:00", None, false, utc(2026, 1, 15, 8, 0)),
            utc(2026, 1, 15, 9, 0)
        );
        // Strict: at the boundary it rolls to tomorrow.
        assert_eq!(
            cal(&["day"], "09:00", None, false, utc(2026, 1, 15, 9, 0)),
            utc(2026, 1, 16, 9, 0)
        );
    }

    #[test]
    fn calendar_single_weekday_jumps_to_that_weekday() {
        // Wed 2026-01-14 -> next Friday is 2026-01-16.
        assert_eq!(
            cal(&["friday"], "17:00", None, false, utc(2026, 1, 14, 12, 0)),
            utc(2026, 1, 16, 17, 0)
        );
    }

    #[test]
    fn calendar_multiple_weekdays_pick_the_nearest() {
        // Thu 2026-01-15 10:00, set {Mon,Wed,Fri} -> Fri 2026-01-16 09:00.
        assert_eq!(
            cal(
                &["monday", "wednesday", "friday"],
                "09:00",
                None,
                false,
                utc(2026, 1, 15, 10, 0)
            ),
            utc(2026, 1, 16, 9, 0)
        );
    }

    #[test]
    fn calendar_weekday_token_skips_the_weekend() {
        // Fri 2026-01-16 18:00 -> Mon 2026-01-19 09:00.
        assert_eq!(
            cal(&["weekday"], "09:00", None, false, utc(2026, 1, 16, 18, 0)),
            utc(2026, 1, 19, 9, 0)
        );
    }

    #[test]
    fn calendar_monthly_on_day_fires_next_month() {
        assert_eq!(
            cal(&["month"], "09:00", Some(1), false, utc(2026, 1, 15, 0, 0)),
            utc(2026, 2, 1, 9, 0)
        );
    }

    #[test]
    fn calendar_monthly_last_day_fires_end_of_month() {
        assert_eq!(
            cal(&["month"], "09:00", None, true, utc(2026, 1, 15, 0, 0)),
            utc(2026, 1, 31, 9, 0)
        );
    }

    #[test]
    fn calendar_monthly_on_day_31_skips_months_without_it() {
        // After 2026-01-31, on_day=31: Feb and Apr lack a 31st -> next is Mar 31.
        assert_eq!(
            cal(&["month"], "09:00", Some(31), false, utc(2026, 2, 1, 0, 0)),
            utc(2026, 3, 31, 9, 0)
        );
    }

    #[test]
    fn calendar_daily_advances_through_a_dst_gap_in_its_own_zone() {
        // Daily 02:30 in New York on the spring-forward day -> 03:00 EDT
        // (decision 0005), proving the schedule's tz drives wall-clock + DST.
        let days = vec!["day".to_string()];
        let after = resolve_wall_clock(naive(2026, 3, 8, 0, 0), Tz::America__New_York);
        let next = next_calendar(&days, "02:30", Tz::America__New_York, None, false, after);
        assert_eq!(next.to_rfc3339(), "2026-03-08T03:00:00-04:00");
    }

    // ── cron ────────────────────────────────────────────────────────────────

    #[test]
    fn cron_daily_fires_at_the_pattern_time() {
        assert_eq!(
            next_cron("0 9 * * *", Tz::UTC, utc(2026, 1, 15, 8, 0)),
            Some(utc(2026, 1, 15, 9, 0))
        );
    }

    #[test]
    fn cron_is_strict_after_a_matching_instant() {
        assert_eq!(
            next_cron("0 9 * * *", Tz::UTC, utc(2026, 1, 15, 9, 0)),
            Some(utc(2026, 1, 16, 9, 0))
        );
    }

    #[test]
    fn cron_honors_the_schedule_timezone() {
        // 08:00 EST -> next "0 9 * * *" fires at 09:00 EST (-05:00).
        let after = resolve_wall_clock(naive(2026, 1, 15, 8, 0), Tz::America__New_York);
        let next = next_cron("0 9 * * *", Tz::America__New_York, after).unwrap();
        assert_eq!(next.to_rfc3339(), "2026-01-15T09:00:00-05:00");
    }

    #[test]
    fn cron_weekday_range_skips_the_weekend() {
        // Fri 2026-01-16 18:00, "0 9 * * 1-5" -> Mon 2026-01-19 09:00.
        assert_eq!(
            next_cron("0 9 * * 1-5", Tz::UTC, utc(2026, 1, 16, 18, 0)),
            Some(utc(2026, 1, 19, 9, 0))
        );
    }

    #[test]
    fn cron_unparseable_expression_yields_none() {
        assert_eq!(
            next_cron("not a cron", Tz::UTC, utc(2026, 1, 15, 8, 0)),
            None
        );
    }

    // ── engine API: next_occurrence dispatch ──────────────────────────────────

    fn calendar_daily_at(at: &str) -> NormalizedSchedule {
        NormalizedSchedule::Calendar {
            days: vec!["day".to_string()],
            at: at.to_string(),
            timezone: None,
            on_day: None,
            last_day: false,
        }
    }

    /// A representative schedule of every kind, for property-style coverage.
    fn every_kind() -> Vec<NormalizedSchedule> {
        vec![
            NormalizedSchedule::MinuteAligned { every_minutes: 15 },
            NormalizedSchedule::HourAligned { every_hours: 6 },
            calendar_daily_at("09:00"),
            NormalizedSchedule::Calendar {
                days: vec!["month".to_string()],
                at: "09:00".to_string(),
                timezone: None,
                on_day: Some(1),
                last_day: false,
            },
            NormalizedSchedule::Cron {
                expression: "0 9 * * *".to_string(),
                timezone: None,
            },
        ]
    }

    /// The kind label embedded in an occurrence key (`job:KIND:instant`).
    fn key_kind(occ: &Occurrence) -> String {
        occ.key.split(':').nth(1).unwrap().to_string()
    }

    #[test]
    fn dispatch_builds_an_occurrence_with_the_kind_in_the_key() {
        let occ = next_occurrence(
            "backup",
            &calendar_daily_at("09:00"),
            Tz::UTC,
            utc(2026, 1, 15, 8, 0),
        )
        .unwrap();
        assert_eq!(occ.scheduled_for, utc(2026, 1, 15, 9, 0));
        assert_eq!(occ.key, "backup:calendar:2026-01-15T09:00:00+00:00");
    }

    #[test]
    fn dispatch_routes_each_kind_to_its_labelled_key() {
        let after = utc(2026, 1, 15, 8, 0);
        let kinds: Vec<String> = every_kind()
            .iter()
            .map(|s| key_kind(&next_occurrence("j", s, Tz::UTC, after).unwrap()))
            .collect();
        assert_eq!(kinds, ["minute", "hour", "calendar", "calendar", "cron"]);
    }

    #[test]
    fn dispatch_aligned_uses_the_default_timezone() {
        // Hour-aligned has no embedded zone; default_tz drives wall-clock.
        // 06:30 EST -> next 6h boundary 12:00 EST (-05:00).
        let after = resolve_wall_clock(naive(2026, 1, 15, 6, 30), Tz::America__New_York);
        let occ = next_occurrence(
            "j",
            &NormalizedSchedule::HourAligned { every_hours: 6 },
            Tz::America__New_York,
            after,
        )
        .unwrap();
        assert_eq!(occ.scheduled_for.to_rfc3339(), "2026-01-15T12:00:00-05:00");
    }

    #[test]
    fn property_every_kind_is_total_and_strictly_after() {
        let after = utc(2026, 1, 15, 8, 0);
        for schedule in every_kind() {
            let occ = next_occurrence("j", &schedule, Tz::UTC, after)
                .unwrap_or_else(|| panic!("no occurrence for {schedule:?}"));
            assert!(
                occ.scheduled_for > after,
                "not strictly after for {schedule:?}"
            );
        }
    }

    #[test]
    fn property_stepping_advances_monotonically() {
        // Feeding each occurrence back as `after` yields a strictly later one,
        // so the engine can drive an unbounded forward iteration.
        for schedule in every_kind() {
            let mut cursor = utc(2026, 1, 15, 8, 0);
            for _ in 0..5 {
                let occ = next_occurrence("j", &schedule, Tz::UTC, cursor)
                    .unwrap_or_else(|| panic!("no occurrence for {schedule:?}"));
                assert!(occ.scheduled_for > cursor, "stalled for {schedule:?}");
                cursor = occ.scheduled_for;
            }
        }
    }

    fn cron(expr: &str) -> NormalizedSchedule {
        NormalizedSchedule::Cron {
            expression: expr.to_owned(),
            timezone: None,
        }
    }

    /// An `after` whose seconds and sub-second components are non-zero, to prove
    /// the computed occurrence does not inherit them.
    fn after_with_subsecond() -> DateTime<Tz> {
        Tz::UTC
            .with_ymd_and_hms(2026, 6, 20, 0, 30, 17)
            .unwrap()
            .with_nanosecond(123_456_789)
            .unwrap()
    }

    #[test]
    fn cron_occurrence_has_no_sub_second_component() {
        let occ =
            next_occurrence("j", &cron("0 * * * *"), Tz::UTC, after_with_subsecond()).unwrap();
        assert_eq!(
            occ.scheduled_for.nanosecond(),
            0,
            "cron instant must be whole-second"
        );
        assert!(
            !occ.key.contains('.'),
            "occurrence_key must not carry sub-second precision: {}",
            occ.key
        );
    }

    #[test]
    fn cron_occurrence_key_is_stable_across_sub_second_of_after() {
        // The same logical fire computed at different microseconds of the same
        // second must yield the same occurrence_key, or dedupe breaks.
        let base = Tz::UTC.with_ymd_and_hms(2026, 6, 20, 0, 30, 0).unwrap();
        let early = base.with_nanosecond(1).unwrap();
        let late = base.with_nanosecond(999_000_000).unwrap();
        let a = next_occurrence("j", &cron("0 * * * *"), Tz::UTC, early).unwrap();
        let b = next_occurrence("j", &cron("0 * * * *"), Tz::UTC, late).unwrap();
        assert_eq!(a.key, b.key);
    }
}
