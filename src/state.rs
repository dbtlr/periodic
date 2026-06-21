//! Observed runtime state: SQLite schema, migrations, and repositories for
//! jobs, runs, attempts, events, and daemon state.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension};
use serde::Serialize;

use crate::config::{EffectiveConfig, NormalizedSchedule};
use crate::error::{Error, Result};

/// Ordered schema migrations. Each entry is applied once, in order; the array
/// index + 1 is the `PRAGMA user_version` it advances the database to. Append a
/// new entry to evolve the schema — never edit an applied one.
const MIGRATIONS: &[&str] = &[
    // 0001 — full runtime-state schema. Only `jobs_state` has a writer in phase
    // 0.4; `runs`/`run_attempts`/`events`/`daemon_state` are populated by the
    // executor (0.5) and daemon (0.6). Landing the whole schema in one migration
    // avoids re-migrating as each writer arrives (see ADR 0006).
    "
    create table jobs_state (
      job_id text primary key,
      state text not null,
      config_hash text not null,
      schedule_kind text not null,
      next_run_at text,
      last_run_id text,
      last_success_at text,
      last_failure_at text,
      consecutive_failures integer not null default 0,
      validation_error text,
      updated_at text not null
    );

    create table runs (
      id text primary key,
      job_id text not null,
      config_hash text not null,
      trigger_type text not null,
      status text not null,
      scheduled_for text,
      occurrence_key text,
      started_at text,
      finished_at text,
      exit_code integer,
      error text,
      created_at text not null,
      updated_at text not null
    );

    create table run_attempts (
      id text primary key,
      run_id text not null references runs(id) on delete cascade,
      attempt_number integer not null,
      status text not null,
      pid integer,
      started_at text,
      finished_at text,
      exit_code integer,
      signal integer,
      stdout_path text,
      stderr_path text,
      error text,
      created_at text not null,
      updated_at text not null,
      unique(run_id, attempt_number)
    );

    create table events (
      id text primary key,
      job_id text,
      run_id text,
      attempt_id text,
      level text not null,
      event_type text not null,
      message text not null,
      data_json text,
      created_at text not null
    );

    create table daemon_state (
      key text primary key,
      value text not null,
      updated_at text not null
    );

    create index idx_runs_job_started on runs(job_id, started_at desc);
    create index idx_runs_status on runs(status);
    create unique index idx_runs_occurrence_key on runs(occurrence_key) where occurrence_key is not null;
    create index idx_attempts_run on run_attempts(run_id, attempt_number);
    create index idx_events_job_created on events(job_id, created_at desc);
    create index idx_events_run_created on events(run_id, created_at desc);
    ",
    // 0002 — phase 0.5 executor. Output is a daily JSONL keyed by line fields
    // (job_id/run_id), not a path per attempt, so the per-attempt path columns
    // from 0001 are wrong-granularity. `run_attempts` has no writer before this
    // phase, so dropping them loses no data. New migration (not a 0001 edit):
    // released 0.4 DBs are already at user_version = 1.
    "
    alter table run_attempts drop column stdout_path;
    alter table run_attempts drop column stderr_path;
    ",
];

/// Default on-disk location of the runtime state database.
///
/// Mirrors [`crate::cli::default_config_path`]'s `HOME`-relative resolution:
/// observed state lives under the XDG state dir, separate from user config.
pub(crate) fn default_db_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(".local/state/periodic/periodic.db")
}

/// Default on-disk location of per-day run output logs.
pub(crate) fn default_logs_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(".local/state/periodic/logs")
}

/// Open (creating if absent) the state database at `path`, applying connection
/// pragmas and running any pending migrations. The parent directory is created
/// if it does not yet exist.
pub(crate) fn open(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|source| Error::StateDir {
            path: parent.display().to_string(),
            source,
        })?;
    }
    let conn = Connection::open(path)?;
    apply_pragmas(&conn)?;
    migrate(&conn)?;
    Ok(conn)
}

/// Apply the per-connection pragmas. WAL improves local concurrent reads from
/// the CLI/TUI while the daemon writes; foreign keys and the busy timeout are
/// connection-scoped and must be re-applied on every open.
fn apply_pragmas(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "pragma journal_mode = WAL;
         pragma foreign_keys = ON;
         pragma busy_timeout = 5000;",
    )?;
    Ok(())
}

/// Apply any migrations newer than the database's current `user_version`,
/// advancing it to the latest as it goes. Idempotent: a fully-migrated database
/// applies nothing.
fn migrate(conn: &Connection) -> Result<()> {
    let current: i64 = conn.query_row("pragma user_version", [], |r| r.get(0))?;
    for (idx, sql) in MIGRATIONS.iter().enumerate() {
        let version = idx as i64 + 1;
        if current < version {
            conn.execute_batch(sql)?;
            // `user_version` cannot be bound as a parameter; `version` is a
            // crate-controlled integer, never user input.
            conn.execute_batch(&format!("pragma user_version = {version};"))?;
        }
    }
    Ok(())
}

// ─── jobs_state projection ───────────────────────────────────────────────────

/// Project the effective config into `jobs_state`: one row per job, carrying the
/// current `state`, `config_hash`, `schedule_kind`, and computed `next_run_at`.
///
/// This is a *projection*, not a history write — it upserts only the
/// config-derived columns and deliberately preserves the run-outcome columns
/// (`last_run_id`, `last_success_at`, `last_failure_at`, `consecutive_failures`,
/// `validation_error`) owned by the executor (0.5) and daemon (0.6). Jobs
/// without an id are skipped. Returns the number of rows projected.
pub(crate) fn reconcile(
    conn: &Connection,
    config: &EffectiveConfig,
    now: DateTime<Utc>,
) -> Result<usize> {
    let updated_at = now.to_rfc3339();
    let mut count = 0;
    for job in &config.jobs {
        let Some(job_id) = job.id.as_deref() else {
            continue; // unkeyed jobs can't be projected (validation flags these)
        };
        let state = if job.enabled { "active" } else { "disabled" };
        let config_hash = crate::config::job_config_hash(job);
        let kind = schedule_kind(&job.schedule);
        let next_run_at: Option<String> = if job.enabled {
            let tz = crate::scheduler::resolve_timezone(job.timezone.as_deref());
            crate::scheduler::next_occurrence(job_id, &job.schedule, tz, now.with_timezone(&tz))
                .map(|occ| occ.scheduled_for.to_rfc3339())
        } else {
            None
        };
        conn.execute(
            "insert into jobs_state
                 (job_id, state, config_hash, schedule_kind, next_run_at, updated_at)
             values (?1, ?2, ?3, ?4, ?5, ?6)
             on conflict(job_id) do update set
                 state = excluded.state,
                 config_hash = excluded.config_hash,
                 schedule_kind = excluded.schedule_kind,
                 next_run_at = excluded.next_run_at,
                 updated_at = excluded.updated_at",
            rusqlite::params![job_id, state, config_hash, kind, next_run_at, updated_at],
        )?;
        count += 1;
    }
    Ok(count)
}

/// The stable `schedule_kind` tag persisted for a normalized schedule, matching
/// the kind segment of `occurrence_key`.
fn schedule_kind(schedule: &NormalizedSchedule) -> &'static str {
    match schedule {
        NormalizedSchedule::MinuteAligned { .. } => "minute",
        NormalizedSchedule::HourAligned { .. } => "hour",
        NormalizedSchedule::Calendar { .. } => "calendar",
        NormalizedSchedule::Cron { .. } => "cron",
    }
}

// ─── run / attempt writers (executor, 0.5) ───────────────────────────────────

/// Insert a `pending` run row. Manual runs pass `trigger_type = "manual"` and a
/// null `occurrence_key` (no scheduled-occurrence dedupe for manual triggers).
pub(crate) fn create_run(
    conn: &Connection,
    id: &str,
    job_id: &str,
    config_hash: &str,
    trigger_type: &str,
    now: DateTime<Utc>,
) -> Result<()> {
    let ts = now.to_rfc3339();
    conn.execute(
        "insert into runs (id, job_id, config_hash, trigger_type, status, created_at, updated_at)
         values (?1, ?2, ?3, ?4, 'pending', ?5, ?5)",
        rusqlite::params![id, job_id, config_hash, trigger_type, ts],
    )?;
    Ok(())
}

/// Transition a run to `running`, stamping `started_at`.
pub(crate) fn mark_run_running(conn: &Connection, id: &str, now: DateTime<Utc>) -> Result<()> {
    let ts = now.to_rfc3339();
    let n = conn.execute(
        "update runs set status='running', started_at=?2, updated_at=?2 where id=?1",
        rusqlite::params![id, ts],
    )?;
    if n == 0 {
        return Err(Error::NoRowUpdated(format!("run {id}")));
    }
    Ok(())
}

/// Terminal run update: status + optional exit code / error message + `finished_at`.
pub(crate) fn finish_run(
    conn: &Connection,
    id: &str,
    status: &str,
    exit_code: Option<i32>,
    error: Option<&str>,
    now: DateTime<Utc>,
) -> Result<()> {
    let ts = now.to_rfc3339();
    let n = conn.execute(
        "update runs set status=?2, exit_code=?3, error=?4, finished_at=?5, updated_at=?5 where id=?1",
        rusqlite::params![id, status, exit_code.map(i64::from), error, ts],
    )?;
    if n == 0 {
        return Err(Error::NoRowUpdated(format!("run {id}")));
    }
    Ok(())
}

/// Insert a `running` attempt row.
pub(crate) fn start_attempt(
    conn: &Connection,
    id: &str,
    run_id: &str,
    attempt_number: i64,
    pid: Option<i32>,
    now: DateTime<Utc>,
) -> Result<()> {
    let ts = now.to_rfc3339();
    conn.execute(
        "insert into run_attempts (id, run_id, attempt_number, status, pid, started_at, created_at, updated_at)
         values (?1, ?2, ?3, 'running', ?4, ?5, ?5, ?5)",
        rusqlite::params![id, run_id, attempt_number, pid.map(i64::from), ts],
    )?;
    Ok(())
}

/// Record the OS pid on an already-started attempt (known only post-spawn).
pub(crate) fn start_attempt_pid(conn: &Connection, id: &str, pid: i32) -> Result<()> {
    let n = conn.execute(
        "update run_attempts set pid=?2 where id=?1",
        rusqlite::params![id, i64::from(pid)],
    )?;
    if n == 0 {
        return Err(Error::NoRowUpdated(format!("attempt {id}")));
    }
    Ok(())
}

/// Terminal attempt update.
pub(crate) fn finish_attempt(
    conn: &Connection,
    id: &str,
    status: &str,
    exit_code: Option<i32>,
    signal: Option<i32>,
    error: Option<&str>,
    now: DateTime<Utc>,
) -> Result<()> {
    let ts = now.to_rfc3339();
    let n = conn.execute(
        "update run_attempts set status=?2, exit_code=?3, signal=?4, error=?5, finished_at=?6, updated_at=?6
         where id=?1",
        rusqlite::params![id, status, exit_code.map(i64::from), signal.map(i64::from), error, ts],
    )?;
    if n == 0 {
        return Err(Error::NoRowUpdated(format!("attempt {id}")));
    }
    Ok(())
}

/// Update the executor-owned outcome columns on `jobs_state` after a run finishes.
/// Success sets `last_success_at` and zeroes `consecutive_failures`; failure sets
/// `last_failure_at` and increments the counter. Always records `last_run_id`.
pub(crate) fn update_job_outcome(
    conn: &Connection,
    job_id: &str,
    run_id: &str,
    succeeded: bool,
    now: DateTime<Utc>,
) -> Result<()> {
    let ts = now.to_rfc3339();
    if succeeded {
        conn.execute(
            "update jobs_state set last_run_id=?2, last_success_at=?3,
                 consecutive_failures=0, updated_at=?3 where job_id=?1",
            rusqlite::params![job_id, run_id, ts],
        )?;
    } else {
        conn.execute(
            "update jobs_state set last_run_id=?2, last_failure_at=?3,
                 consecutive_failures=consecutive_failures+1, updated_at=?3 where job_id=?1",
            rusqlite::params![job_id, run_id, ts],
        )?;
    }
    Ok(())
}

/// Whether a job id is present in `jobs_state` (used by `jobs run`/`history`).
pub(crate) fn job_exists(conn: &Connection, job_id: &str) -> Result<bool> {
    let n: i64 = conn.query_row(
        "select count(*) from jobs_state where job_id=?1",
        [job_id],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

// ─── run history reads ───────────────────────────────────────────────────────

/// A `runs` row as surfaced by `jobs history`. `attempts` is the count of
/// `run_attempts` for the run. Serializes to the frozen JSON run shape (0002).
#[derive(Debug, Serialize, PartialEq)]
pub(crate) struct RunRow {
    pub(crate) id: String,
    pub(crate) status: String,
    pub(crate) trigger_type: String,
    pub(crate) started_at: Option<String>,
    pub(crate) finished_at: Option<String>,
    pub(crate) exit_code: Option<i64>,
    pub(crate) attempts: i64,
}

/// Runs for a job, most recent first (by `created_at`), capped at `limit`.
pub(crate) fn list_runs(conn: &Connection, job_id: &str, limit: i64) -> Result<Vec<RunRow>> {
    let mut stmt = conn.prepare(
        "select r.id, r.status, r.trigger_type, r.started_at, r.finished_at, r.exit_code,
                (select count(*) from run_attempts a where a.run_id = r.id) as attempts
         from runs r where r.job_id = ?1
         order by r.created_at desc, r.id desc limit ?2",
    )?;
    let rows = stmt
        .query_map(rusqlite::params![job_id, limit], |row| {
            Ok(RunRow {
                id: row.get(0)?,
                status: row.get(1)?,
                trigger_type: row.get(2)?,
                started_at: row.get(3)?,
                finished_at: row.get(4)?,
                exit_code: row.get(5)?,
                attempts: row.get(6)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

// ─── health inspection (read-only) ───────────────────────────────────────────

/// A read-only snapshot of the state database's health, for `periodic doctor`.
/// `schema_version` is `None` when the database has not been created yet.
#[derive(Debug, PartialEq)]
pub(crate) struct DbHealth {
    pub(crate) path: String,
    pub(crate) schema_version: Option<i64>,
    pub(crate) expected_version: i64,
}

/// Inspect the state database without mutating it. Unlike [`open`], this never
/// creates the file or runs migrations — a missing database reports
/// `schema_version: None` rather than being initialized as a side effect.
pub(crate) fn inspect(path: &Path) -> Result<DbHealth> {
    let expected_version = MIGRATIONS.len() as i64;
    let path_display = path.display().to_string();
    if !path.exists() {
        return Ok(DbHealth {
            path: path_display,
            schema_version: None,
            expected_version,
        });
    }
    let conn = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let schema_version: i64 = conn.query_row("pragma user_version", [], |r| r.get(0))?;
    Ok(DbHealth {
        path: path_display,
        schema_version: Some(schema_version),
        expected_version,
    })
}

// ─── jobs_state reads ────────────────────────────────────────────────────────

/// A `jobs_state` row as surfaced by the read commands. Serializes to the frozen
/// `--format json` job shape (decision 0002); `job_id` is exposed as `id`.
#[derive(Debug, Serialize, PartialEq)]
pub(crate) struct JobStateRow {
    #[serde(rename = "id")]
    pub(crate) job_id: String,
    pub(crate) state: String,
    pub(crate) schedule_kind: String,
    pub(crate) next_run_at: Option<String>,
    pub(crate) config_hash: String,
    pub(crate) updated_at: String,
}

const JOB_STATE_COLUMNS: &str =
    "job_id, state, schedule_kind, next_run_at, config_hash, updated_at";

fn row_to_job_state(row: &rusqlite::Row) -> rusqlite::Result<JobStateRow> {
    Ok(JobStateRow {
        job_id: row.get(0)?,
        state: row.get(1)?,
        schedule_kind: row.get(2)?,
        next_run_at: row.get(3)?,
        config_hash: row.get(4)?,
        updated_at: row.get(5)?,
    })
}

/// All job projections, ordered by id.
pub(crate) fn list_job_states(conn: &Connection) -> Result<Vec<JobStateRow>> {
    let sql = format!("select {JOB_STATE_COLUMNS} from jobs_state order by job_id");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map([], row_to_job_state)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// One job's projection, or `None` if the id is not present.
pub(crate) fn get_job_state(conn: &Connection, job_id: &str) -> Result<Option<JobStateRow>> {
    let sql = format!("select {JOB_STATE_COLUMNS} from jobs_state where job_id = ?1");
    let row = conn
        .query_row(&sql, [job_id], row_to_job_state)
        .optional()?;
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    fn temp_db() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let conn = open(&dir.path().join("periodic.db")).unwrap();
        (dir, conn)
    }

    #[test]
    fn terminal_writers_error_on_missing_row() {
        // Once writes come from separate daemon code paths, an UPDATE that matches no
        // row must surface as an error, not a silent no-op.
        use chrono::TimeZone;
        let (_d, conn) = temp_db();
        let now = Utc.timestamp_opt(1000, 0).unwrap();
        assert!(
            mark_run_running(&conn, "ghost", now).is_err(),
            "mark_run_running on missing run must error"
        );
        assert!(
            finish_run(&conn, "ghost", "success", Some(0), None, now).is_err(),
            "finish_run on missing run must error"
        );
        assert!(
            start_attempt_pid(&conn, "ghost", 42).is_err(),
            "start_attempt_pid on missing attempt must error"
        );
        assert!(
            finish_attempt(&conn, "ghost", "success", Some(0), None, None, now).is_err(),
            "finish_attempt on missing attempt must error"
        );
    }

    fn table_names(conn: &Connection) -> Vec<String> {
        let mut stmt = conn
            .prepare("select name from sqlite_master where type = 'table' order by name")
            .unwrap();
        stmt.query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    }

    fn insert_run(
        conn: &Connection,
        id: &str,
        occurrence_key: Option<&str>,
    ) -> rusqlite::Result<usize> {
        conn.execute(
            "insert into runs (id, job_id, config_hash, trigger_type, status, occurrence_key, created_at, updated_at)
             values (?, 'job', 'hash', 'scheduled', 'pending', ?, '2026-06-20T00:00:00Z', '2026-06-20T00:00:00Z')",
            params![id, occurrence_key],
        )
    }

    #[test]
    fn open_creates_all_schema_tables() {
        let (_dir, conn) = temp_db();
        let tables = table_names(&conn);
        for expected in [
            "jobs_state",
            "runs",
            "run_attempts",
            "events",
            "daemon_state",
        ] {
            assert!(
                tables.contains(&expected.to_string()),
                "missing table {expected}; have {tables:?}"
            );
        }
    }

    #[test]
    fn open_enables_foreign_keys() {
        let (_dir, conn) = temp_db();
        let on: i64 = conn
            .query_row("pragma foreign_keys", [], |r| r.get(0))
            .unwrap();
        assert_eq!(on, 1);
    }

    #[test]
    fn open_uses_wal_journal_mode() {
        let (_dir, conn) = temp_db();
        let mode: String = conn
            .query_row("pragma journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode, "wal");
    }

    #[test]
    fn open_sets_user_version_to_latest_migration() {
        let (_dir, conn) = temp_db();
        let version: i64 = conn
            .query_row("pragma user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, MIGRATIONS.len() as i64);
    }

    #[test]
    fn open_creates_missing_parent_directory() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/state/periodic.db");
        let _conn = open(&path).unwrap();
        assert!(path.exists(), "db file not created at {}", path.display());
    }

    #[test]
    fn open_is_idempotent_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("periodic.db");
        let first = open(&path).unwrap();
        let v1: i64 = first
            .query_row("pragma user_version", [], |r| r.get(0))
            .unwrap();
        drop(first);
        let second = open(&path).unwrap();
        let v2: i64 = second
            .query_row("pragma user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v1, v2);
        // Schema still intact, no duplicate-table error on reopen.
        assert!(table_names(&second).contains(&"runs".to_string()));
    }

    #[test]
    fn occurrence_key_unique_index_rejects_duplicates() {
        let (_dir, conn) = temp_db();
        insert_run(&conn, "run-1", Some("job:minute:2026-06-20T09:15:00Z")).unwrap();
        let dup = insert_run(&conn, "run-2", Some("job:minute:2026-06-20T09:15:00Z"));
        assert!(
            dup.is_err(),
            "duplicate occurrence_key should be rejected by the unique index"
        );
    }

    #[test]
    fn null_occurrence_keys_are_not_deduplicated() {
        let (_dir, conn) = temp_db();
        insert_run(&conn, "run-1", None).unwrap();
        // A second manual run (null occurrence_key) must be allowed — the unique
        // index is partial (only non-null keys).
        insert_run(&conn, "run-2", None).unwrap();
    }

    #[test]
    fn deleting_a_run_cascades_to_its_attempts() {
        let (_dir, conn) = temp_db();
        insert_run(&conn, "run-1", None).unwrap();
        conn.execute(
            "insert into run_attempts (id, run_id, attempt_number, status, created_at, updated_at)
             values ('att-1', 'run-1', 1, 'running', '2026-06-20T00:00:00Z', '2026-06-20T00:00:00Z')",
            [],
        )
        .unwrap();
        conn.execute("delete from runs where id = 'run-1'", [])
            .unwrap();
        let remaining: i64 = conn
            .query_row("select count(*) from run_attempts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            remaining, 0,
            "attempts should cascade-delete with their run"
        );
    }

    #[test]
    fn migration_0002_drops_attempt_path_columns() {
        let (_dir, conn) = temp_db();
        let cols: Vec<String> = conn
            .prepare("select name from pragma_table_info('run_attempts')")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        assert!(
            !cols.contains(&"stdout_path".to_string()),
            "stdout_path should be dropped"
        );
        assert!(
            !cols.contains(&"stderr_path".to_string()),
            "stderr_path should be dropped"
        );
    }

    #[test]
    fn user_version_is_two_after_migrations() {
        let (_dir, conn) = temp_db();
        let v: i64 = conn
            .query_row("pragma user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, 2);
    }
}

#[cfg(test)]
mod reconcile_tests {
    use super::*;
    use chrono::TimeZone;

    fn temp_db() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let conn = open(&dir.path().join("periodic.db")).unwrap();
        (dir, conn)
    }

    fn config(yaml: &str) -> EffectiveConfig {
        crate::config::normalize(&crate::config::parse(yaml).unwrap())
    }

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 6, 20, 9, 0, 0).unwrap()
    }

    #[test]
    fn inserts_one_row_per_job() {
        let (_dir, conn) = temp_db();
        let cfg = config(
            "version: 1\njobs:\n  - id: a\n    schedule: { every: 15m }\n    execution: { command: x }\n  - id: b\n    schedule: { every: day, at: \"09:00\" }\n    execution: { command: y }\n",
        );
        let n = reconcile(&conn, &cfg, now()).unwrap();
        assert_eq!(n, 2);
        let count: i64 = conn
            .query_row("select count(*) from jobs_state", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn computes_future_next_run_for_enabled_job() {
        let (_dir, conn) = temp_db();
        let cfg = config(
            "version: 1\njobs:\n  - id: a\n    schedule: { every: 15m }\n    execution: { command: x }\n",
        );
        reconcile(&conn, &cfg, now()).unwrap();
        let next: Option<String> = conn
            .query_row(
                "select next_run_at from jobs_state where job_id = 'a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let next = next.expect("enabled job should have a next_run_at");
        let parsed = DateTime::parse_from_rfc3339(&next).unwrap();
        assert!(parsed > now(), "next_run_at {next} should be after now");
    }

    #[test]
    fn disabled_job_is_disabled_with_no_next_run() {
        let (_dir, conn) = temp_db();
        let cfg = config(
            "version: 1\njobs:\n  - id: a\n    enabled: false\n    schedule: { every: 15m }\n    execution: { command: x }\n",
        );
        reconcile(&conn, &cfg, now()).unwrap();
        let (state, next): (String, Option<String>) = conn
            .query_row(
                "select state, next_run_at from jobs_state where job_id = 'a'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, "disabled");
        assert!(next.is_none(), "disabled job must not have a next_run_at");
    }

    #[test]
    fn stores_config_hash_and_schedule_kind() {
        let (_dir, conn) = temp_db();
        let cfg = config(
            "version: 1\njobs:\n  - id: a\n    schedule: { every: 15m }\n    execution: { command: x }\n",
        );
        reconcile(&conn, &cfg, now()).unwrap();
        let (hash, kind): (String, String) = conn
            .query_row(
                "select config_hash, schedule_kind from jobs_state where job_id = 'a'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(kind, "minute");
        assert_eq!(hash, crate::config::job_config_hash(&cfg.jobs[0]));
    }

    #[test]
    fn is_idempotent_across_reruns() {
        let (_dir, conn) = temp_db();
        let cfg = config(
            "version: 1\njobs:\n  - id: a\n    schedule: { every: 15m }\n    execution: { command: x }\n",
        );
        reconcile(&conn, &cfg, now()).unwrap();
        reconcile(&conn, &cfg, now()).unwrap();
        let count: i64 = conn
            .query_row("select count(*) from jobs_state", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn preserves_run_outcome_columns_on_rerun() {
        let (_dir, conn) = temp_db();
        let cfg = config(
            "version: 1\njobs:\n  - id: a\n    schedule: { every: 15m }\n    execution: { command: x }\n",
        );
        reconcile(&conn, &cfg, now()).unwrap();
        // Simulate the executor having written run outcomes onto the projection.
        conn.execute(
            "update jobs_state set consecutive_failures = 5, last_success_at = '2026-06-19T00:00:00Z' where job_id = 'a'",
            [],
        )
        .unwrap();
        reconcile(&conn, &cfg, now()).unwrap();
        let (failures, success): (i64, Option<String>) = conn
            .query_row(
                "select consecutive_failures, last_success_at from jobs_state where job_id = 'a'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(failures, 5, "reconcile must not reset run-outcome columns");
        assert_eq!(success.as_deref(), Some("2026-06-19T00:00:00Z"));
    }

    #[test]
    fn updates_projection_when_config_changes() {
        let (_dir, conn) = temp_db();
        let before = config(
            "version: 1\njobs:\n  - id: a\n    schedule: { every: 15m }\n    execution: { command: x }\n",
        );
        reconcile(&conn, &before, now()).unwrap();
        let h1: String = conn
            .query_row(
                "select config_hash from jobs_state where job_id = 'a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let after = config(
            "version: 1\njobs:\n  - id: a\n    schedule: { every: 30m }\n    execution: { command: x }\n",
        );
        reconcile(&conn, &after, now()).unwrap();
        let h2: String = conn
            .query_row(
                "select config_hash from jobs_state where job_id = 'a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_ne!(
            h1, h2,
            "config_hash should change when the schedule changes"
        );
    }

    #[test]
    fn skips_jobs_without_an_id() {
        let (_dir, conn) = temp_db();
        let cfg = config(
            "version: 1\njobs:\n  - schedule: { every: 15m }\n    execution: { command: x }\n",
        );
        let n = reconcile(&conn, &cfg, now()).unwrap();
        assert_eq!(n, 0);
        let count: i64 = conn
            .query_row("select count(*) from jobs_state", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }
}

#[cfg(test)]
mod read_tests {
    use super::*;
    use chrono::TimeZone;

    fn temp_db() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let conn = open(&dir.path().join("periodic.db")).unwrap();
        (dir, conn)
    }

    fn seeded() -> (tempfile::TempDir, Connection) {
        let (dir, conn) = temp_db();
        let cfg = crate::config::normalize(
            &crate::config::parse(
                "version: 1\njobs:\n  - id: beta\n    schedule: { every: 15m }\n    execution: { command: x }\n  - id: alpha\n    enabled: false\n    schedule: { every: day, at: \"09:00\" }\n    execution: { command: y }\n",
            )
            .unwrap(),
        );
        let now = Utc.with_ymd_and_hms(2026, 6, 20, 9, 0, 0).unwrap();
        reconcile(&conn, &cfg, now).unwrap();
        (dir, conn)
    }

    #[test]
    fn list_returns_rows_ordered_by_id() {
        let (_dir, conn) = seeded();
        let rows = list_job_states(&conn).unwrap();
        let ids: Vec<&str> = rows.iter().map(|r| r.job_id.as_str()).collect();
        assert_eq!(ids, ["alpha", "beta"]);
    }

    #[test]
    fn list_carries_projected_fields() {
        let (_dir, conn) = seeded();
        let rows = list_job_states(&conn).unwrap();
        let beta = rows.iter().find(|r| r.job_id == "beta").unwrap();
        assert_eq!(beta.state, "active");
        assert_eq!(beta.schedule_kind, "minute");
        assert!(beta.next_run_at.is_some());
    }

    #[test]
    fn get_returns_the_requested_job() {
        let (_dir, conn) = seeded();
        let row = get_job_state(&conn, "alpha").unwrap().unwrap();
        assert_eq!(row.job_id, "alpha");
        assert_eq!(row.state, "disabled");
        assert!(row.next_run_at.is_none());
    }

    #[test]
    fn get_returns_none_for_unknown_job() {
        let (_dir, conn) = seeded();
        assert!(get_job_state(&conn, "nope").unwrap().is_none());
    }

    #[test]
    fn inspect_reports_not_created_for_missing_db() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent.db");
        let health = inspect(&path).unwrap();
        assert_eq!(health.schema_version, None);
        assert!(health.expected_version >= 1);
    }

    #[test]
    fn inspect_does_not_create_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent.db");
        inspect(&path).unwrap();
        assert!(
            !path.exists(),
            "inspect must be read-only and not create the db"
        );
    }

    #[test]
    fn inspect_reports_current_schema_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("periodic.db");
        drop(open(&path).unwrap()); // create + migrate
        let health = inspect(&path).unwrap();
        assert_eq!(health.schema_version, Some(health.expected_version));
    }

    #[test]
    fn job_state_serializes_id_field() {
        let row = JobStateRow {
            job_id: "cleanup".to_owned(),
            state: "active".to_owned(),
            schedule_kind: "calendar".to_owned(),
            next_run_at: Some("2026-06-21T09:00:00-04:00".to_owned()),
            config_hash: "abc".to_owned(),
            updated_at: "2026-06-20T09:00:00+00:00".to_owned(),
        };
        let json = serde_json::to_string(&row).unwrap();
        assert!(json.contains("\"id\":\"cleanup\""), "got {json}");
        assert!(!json.contains("job_id"), "job_id should serialize as id");
    }
}

#[cfg(test)]
mod run_writer_tests {
    use super::*;
    use chrono::TimeZone;

    fn temp_db() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let conn = open(&dir.path().join("periodic.db")).unwrap();
        (dir, conn)
    }
    fn at(s: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(s, 0).unwrap()
    }

    fn seed_job(conn: &Connection) {
        conn.execute(
            "insert into jobs_state (job_id, state, config_hash, schedule_kind, updated_at)
             values ('cleanup', 'active', 'h', 'minute', '2026-06-20T00:00:00Z')",
            [],
        )
        .unwrap();
    }

    #[test]
    fn create_run_inserts_pending_manual_run() {
        let (_d, conn) = temp_db();
        create_run(&conn, "r1", "cleanup", "h", "manual", at(1000)).unwrap();
        let (status, trig): (String, String) = conn
            .query_row(
                "select status, trigger_type from runs where id='r1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((status.as_str(), trig.as_str()), ("pending", "manual"));
    }

    #[test]
    fn finish_run_records_status_and_exit_code() {
        let (_d, conn) = temp_db();
        create_run(&conn, "r1", "cleanup", "h", "manual", at(1000)).unwrap();
        mark_run_running(&conn, "r1", at(1001)).unwrap();
        finish_run(&conn, "r1", "failed", Some(2), None, at(1005)).unwrap();
        let (status, code, started, finished): (
            String,
            Option<i64>,
            Option<String>,
            Option<String>,
        ) = conn
            .query_row(
                "select status, exit_code, started_at, finished_at from runs where id='r1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(status, "failed");
        assert_eq!(code, Some(2));
        assert!(started.is_some() && finished.is_some());
    }

    #[test]
    fn attempts_record_number_and_status() {
        let (_d, conn) = temp_db();
        create_run(&conn, "r1", "cleanup", "h", "manual", at(1000)).unwrap();
        start_attempt(&conn, "a1", "r1", 1, Some(4242), at(1001)).unwrap();
        finish_attempt(&conn, "a1", "failed", Some(1), None, None, at(1002)).unwrap();
        start_attempt(&conn, "a2", "r1", 2, Some(4243), at(1003)).unwrap();
        finish_attempt(&conn, "a2", "success", Some(0), None, None, at(1004)).unwrap();
        let n: i64 = conn
            .query_row(
                "select count(*) from run_attempts where run_id='r1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 2);

        // Verify a1 column values
        let (status, num, exit, finished): (String, i64, Option<i64>, Option<String>) = conn.query_row(
            "select status, attempt_number, exit_code, finished_at from run_attempts where id='a1'",
            [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        ).unwrap();
        assert_eq!(status, "failed");
        assert_eq!(num, 1);
        assert_eq!(exit, Some(1));
        assert!(finished.is_some());

        // Verify a2 column values
        let (status, num, exit, finished): (String, i64, Option<i64>, Option<String>) = conn.query_row(
            "select status, attempt_number, exit_code, finished_at from run_attempts where id='a2'",
            [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        ).unwrap();
        assert_eq!(status, "success");
        assert_eq!(num, 2);
        assert_eq!(exit, Some(0));
        assert!(finished.is_some());
    }

    #[test]
    fn update_job_outcome_on_success_sets_last_success_and_clears_failures() {
        let (_d, conn) = temp_db();
        seed_job(&conn);
        conn.execute(
            "update jobs_state set consecutive_failures = 3 where job_id='cleanup'",
            [],
        )
        .unwrap();
        update_job_outcome(&conn, "cleanup", "r1", true, at(2000)).unwrap();
        let (last_run, success_at, fails): (Option<String>, Option<String>, i64) = conn.query_row(
            "select last_run_id, last_success_at, consecutive_failures from jobs_state where job_id='cleanup'",
            [], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?))).unwrap();
        assert_eq!(last_run.as_deref(), Some("r1"));
        assert!(success_at.is_some());
        assert_eq!(fails, 0);
    }

    #[test]
    fn update_job_outcome_on_failure_increments_failures() {
        let (_d, conn) = temp_db();
        seed_job(&conn);
        update_job_outcome(&conn, "cleanup", "r1", false, at(2000)).unwrap();
        update_job_outcome(&conn, "cleanup", "r2", false, at(2001)).unwrap();
        let (fail_at, fails): (Option<String>, i64) = conn.query_row(
            "select last_failure_at, consecutive_failures from jobs_state where job_id='cleanup'",
            [], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert!(fail_at.is_some());
        assert_eq!(fails, 2);
    }

    #[test]
    fn job_exists_reflects_jobs_state() {
        let (_d, conn) = temp_db();
        seed_job(&conn);
        assert!(job_exists(&conn, "cleanup").unwrap());
        assert!(!job_exists(&conn, "ghost").unwrap());
    }

    #[test]
    fn start_attempt_pid_updates_pid_on_started_attempt() {
        let (_d, conn) = temp_db();
        create_run(&conn, "r1", "cleanup", "h", "manual", at(1000)).unwrap();
        start_attempt(&conn, "a1", "r1", 1, None, at(1001)).unwrap();
        start_attempt_pid(&conn, "a1", 4242).unwrap();
        let pid: Option<i64> = conn
            .query_row("select pid from run_attempts where id='a1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(pid, Some(4242));
    }

    #[test]
    fn list_runs_returns_recent_first_with_attempt_count() {
        let (_d, conn) = temp_db();
        create_run(&conn, "r1", "cleanup", "h", "manual", at(1000)).unwrap();
        mark_run_running(&conn, "r1", at(1000)).unwrap();
        start_attempt(&conn, "a1", "r1", 1, None, at(1000)).unwrap();
        finish_attempt(&conn, "a1", "success", Some(0), None, None, at(1001)).unwrap();
        finish_run(&conn, "r1", "success", Some(0), None, at(1001)).unwrap();
        create_run(&conn, "r2", "cleanup", "h", "manual", at(2000)).unwrap();
        mark_run_running(&conn, "r2", at(2000)).unwrap();
        finish_run(&conn, "r2", "failed", Some(1), None, at(2001)).unwrap();

        let rows = list_runs(&conn, "cleanup", 10).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].id, "r2", "most recent first");
        assert_eq!(rows[0].status, "failed");
        assert_eq!(rows[1].attempts, 1);
    }

    #[test]
    fn list_runs_honors_limit() {
        let (_d, conn) = temp_db();
        for i in 0..5 {
            let id = format!("r{i}");
            create_run(&conn, &id, "cleanup", "h", "manual", at(1000 + i)).unwrap();
            mark_run_running(&conn, &id, at(1000 + i)).unwrap();
        }
        assert_eq!(list_runs(&conn, "cleanup", 3).unwrap().len(), 3);
    }
}
