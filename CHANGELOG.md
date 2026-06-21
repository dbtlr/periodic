# Changelog

All notable changes to periodic are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims to
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html). While periodic
is pre-1.0, minor versions (`0.x`) may carry breaking changes.

## [Unreleased]

### Changed

- `periodic doctor` now also reports daemon liveness — `not running`, `running
  (pid N)`, `stopped`, or `not responding (stale heartbeat; possible crash)` — and
  exits non-zero when a daemon that claims to be running has a stale heartbeat.

### Fixed

- `periodic logs <id>` now exits `1` with `error: no such job` when the job is not
  in the config, instead of silently printing "no output" and exiting `0`. A known
  job with no captured output still reports "no output" and exits `0`.

## v0.5.0 - 2026-06-21

The `0.5` increment — execution. periodic now runs jobs: `periodic jobs run`
executes a job in the foreground with full process-lifecycle management (its own
process group, timeout, retries), records each run to the state database, and
surfaces run history and captured output via `jobs history` and `logs`.

### Added

- `periodic jobs run <id>` — execute a job now, in the foreground, recording the
  run. Exit `0` success · `1` failed/timeout/cancelled · `2` usage/invalid. The job
  runs in its own process group; Ctrl-C and timeouts terminate the whole tree
  (SIGTERM, then SIGKILL after a grace period). `--format json` emits
  `{ "run": { … } }`. Disabled jobs run on explicit manual trigger; invalid jobs
  are refused.
- `periodic jobs history <id> [--limit N] [--format json]` — list a job's recorded
  runs, most recent first. JSON: `{ "runs": [ … ] }`.
- `periodic logs <id> [--run <id>] [--format json]` — show captured stdout/stderr
  for a job (or a single run), read from per-day JSONL files under
  `~/.local/state/periodic/logs/`.
- Executor: honors `timeout` (terminal) and `retry.max_retries` (immediate retry on
  non-zero exit); writes `runs` / `run_attempts` / lifecycle `events`.

### Changed

- State schema migration `0002` drops the unused per-attempt `stdout_path` /
  `stderr_path` columns; run output now lives in per-day JSONL log files.

## v0.4.0 - 2026-06-21

The `0.4` increment — runtime state. periodic now persists observed state in a
SQLite database and surfaces each job's computed next run. This is also the first
stable cut to expose the schedule-computation engine (built internally in `0.3`)
to users, via the next-run times shown by `jobs list`/`status`.

### Added

- `periodic jobs list [--format human|json]` — list configured jobs with their
  state (`active`/`disabled`), schedule kind, and next run time, computed live
  from the schedule engine. The JSON form is the stable agent contract (decision
  0002): `{ "jobs": [ { "id", "state", "schedule_kind", "next_run_at",
  "config_hash", "updated_at" } ] }`.
- `periodic jobs status <id> [--format human|json]` — show one job's projection
  (`{ "job": { … } }` in JSON); exit `1` when the id is unknown.
- Observed runtime state is now persisted in a SQLite database at
  `~/.local/state/periodic/periodic.db`, created on first use (bundled SQLite, no
  system dependency). This is the first command to surface jobs' next-run times.
- `periodic doctor` — read-only health check of the state database, reporting its
  path and schema version (not-yet-created, healthy, pending upgrade, or newer
  than this build). Daemon and crash-recovery checks arrive with the daemon.

## v0.2.0 - 2026-06-18

The `0.2` increment — config and validation. periodic's first user-facing
surface: it now parses and strictly validates the YAML desired-state config and
opens the frozen `--format json` agent contract (decision 0002).

### Added

- `periodic validate [PATH] [--format human|json]` — parse and strictly validate
  the YAML config at `~/.config/periodic/periodic.config.yaml` (or an explicit
  PATH), reporting all diagnostics in a single pass. Exit codes: `0` valid
  (warnings do not fail), `1` validation errors, `2` config unreadable/missing.
- `--format json` emits the stable, additive-only agent contract (decision 0002):
  fields `ok`, `config_path`, `summary`, and `diagnostics` with `severity`,
  `code`, `message`, and optional `job`/`path`/`line`/`col`.
- Strict validation rules: unknown-field rejection, wall-clock divisor
  enforcement (decision 0001), cron expression / timezone / duration validation,
  and job ID uniqueness and naming checks.

## v0.1.0 - 2026-06-17

The first build of periodic — the `0.1` foundation. Most of this phase is build
and release infrastructure (not user-facing); the user-visible surface is the
installable binary and its update path.

### Added

- The `periodic` binary, installable via the dist shell installer, with shell
  completions (bash/zsh/fish) and a man page.
- `periodic self-update [--next] [--tag]` — update in place from the GitHub
  release channel: latest stable, the `-next` prerelease channel, or a specific tag.
