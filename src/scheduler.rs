//! Schedule computation and the scheduler loop: next-run calculation,
//! wall-clock alignment, occurrence identity, missed-run detection, and
//! clock-jump/DST handling. Emits run intents; never spawns processes.

use std::str::FromStr;

use chrono::{DateTime, NaiveDateTime, TimeZone};
use chrono_tz::Tz;

/// Resolve an optional IANA timezone name to a concrete [`Tz`].
///
/// `None` means the job did not specify a zone, so the system local zone is
/// used. Names are already validated upstream (decision 0001 / 0.2 validation),
/// so an unparseable name or undetectable local zone falls back to UTC purely
/// defensively — the engine never panics on a bad zone.
#[allow(dead_code)] // consumed by the per-kind computation tasks (PDC-42..44)
pub(crate) fn resolve_timezone(name: Option<&str>) -> Tz {
    match name {
        Some(tz) => Tz::from_str(tz).unwrap_or(Tz::UTC),
        None => iana_time_zone::get_timezone()
            .ok()
            .and_then(|local| Tz::from_str(&local).ok())
            .unwrap_or(Tz::UTC),
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

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use chrono_tz::Tz;

    fn naive(y: i32, m: u32, d: u32, h: u32, min: u32) -> chrono::NaiveDateTime {
        NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(h, min, 0)
            .unwrap()
    }

    #[test]
    fn resolve_timezone_parses_a_named_zone() {
        assert_eq!(
            resolve_timezone(Some("America/New_York")),
            Tz::America__New_York
        );
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
}
