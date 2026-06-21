//! Observed runtime state: SQLite schema, migrations, and repositories for
//! jobs, runs, attempts, events, and daemon state.

use std::path::{Path, PathBuf};

use rusqlite::Connection;

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
];

/// Default on-disk location of the runtime state database.
///
/// Mirrors [`crate::cli::default_config_path`]'s `HOME`-relative resolution:
/// observed state lives under the XDG state dir, separate from user config.
#[allow(dead_code)] // consumed by the reconcile + read-surface commands (PDC-48/49/50)
pub(crate) fn default_db_path() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_default();
    home.join(".local/state/periodic/periodic.db")
}

/// Open (creating if absent) the state database at `path`, applying connection
/// pragmas and running any pending migrations. The parent directory is created
/// if it does not yet exist.
#[allow(dead_code)] // consumed by the reconcile + read-surface commands (PDC-48/49/50)
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

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    fn temp_db() -> (tempfile::TempDir, Connection) {
        let dir = tempfile::tempdir().unwrap();
        let conn = open(&dir.path().join("periodic.db")).unwrap();
        (dir, conn)
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
}
