# Changelog

All notable changes to periodic are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims to
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html). While periodic
is pre-1.0, minor versions (`0.x`) may carry breaking changes.

## [Unreleased]

### Added

- `periodic jobs edit` — open the whole config in `$EDITOR` (`$VISUAL` → `$EDITOR`
  → `vi`) to hand-edit your desired state, then validate and apply it. On save the
  config is parsed and validated; if it has errors the editor reopens with the
  errors shown as a comment header and your text preserved (save it unchanged to
  abort). When there is no config yet, a starter scaffold is seeded — this is the
  way to create your first job. A valid result is applied atomically through the
  same dual-mode path as the other mutations (live IPC reload when the daemon runs,
  direct write when stopped). If the config changed on disk while you were editing,
  the edit is refused rather than clobbering the concurrent change. Saving with no
  changes is a clean no-op. The command is interactive-only and is not part of the
  `--format json` contract.

- `periodic jobs add` — create a job from flags. The schedule comes from
  `--every <15m|6h|day|weekday|friday|monday,wednesday|month>` (with `--at`,
  `--on-day`, `--last-day`) or the `--cron` escape hatch (the two are mutually
  exclusive); the command and options from `--command`, `--cwd`, `--timeout`,
  `--overlap`, `--retry`, `--title`, `--disabled`. The job id is `--id`, else a
  kebab-case slug of `--title`, else the command's basename. A generated block is
  appended to `jobs:` surgically (the rest of the file is untouched), then
  validated before it is written, so an invalid schedule or a colliding id is
  refused (exit `1`) without changing the config. `--format json` reports
  `{ "id", "added": true }`. Adding the first job to an empty config is not yet
  supported — use `jobs edit`.

- `periodic jobs pause <id>` / `periodic jobs resume <id>` — disable or re-enable a
  job by toggling its `enabled` flag in the config. Edits are **surgical**: the
  job's `enabled:` line is flipped (or inserted) in place, leaving every comment,
  key order, and formatting in the rest of the file untouched. The change is
  validated before it is written and applied atomically, so an invalid result never
  replaces a good config. When the daemon is running the change is applied over IPC
  and the schedule reloads live; when it is stopped the CLI writes the file
  directly. `--format json` reports `{ "id", "state" }`. An unknown job id exits `1`
  without touching the file.
- `periodic jobs remove <id>` — delete a job from the config. The job's block is
  excised surgically (siblings, comments, and formatting preserved), validated, and
  written atomically; the same dual-mode applies (live IPC reload when the daemon
  runs, direct write when stopped). Invoking the command is the confirmation — there
  is no interactive prompt. Run history is unaffected. `--format json` reports
  `{ "id", "removed": true }`; an unknown job id exits `1` without touching the file.

## v0.6.0 - 2026-06-21

The `0.6` increment — the daemon. periodic now runs as a long-lived scheduler: a
`periodic daemon` process fires jobs on their wall-clock schedules in their own
process groups, recovers cleanly from crashes, applies overlap and missed-run
policies, reloads config live, and can be supervised by launchd / `systemd --user`.

### Added

- `periodic daemon start [--foreground] [--detach]` — run the scheduler daemon. By
  default (and with `--foreground`) it runs the loop in the foreground until
  signalled; `--detach` re-spawns it as a detached background process and prints the
  child pid. On startup it validates the config, reconciles state, recovers runs
  interrupted by a prior crash, and then dispatches due jobs to their own process
  groups. Refuses to start (exit `1`, `daemon already running (pid N)`) when a live
  daemon already holds the heartbeat. SIGTERM/SIGINT trigger a graceful shutdown
  that drains in-flight runs (up to a 10s grace) before exiting.
- `periodic daemon stop [--force]` — stop the running daemon by sending SIGTERM
  (or SIGKILL with `--force`) to the recorded pid. Idempotent: a missing or
  already-stopped daemon prints `daemon not running` and exits `0`.
- `periodic daemon status [--format json]` — report daemon liveness from the
  recorded heartbeat: `running (pid N)`, `stopped`, `not responding` (stale
  heartbeat), or `not running`. JSON: `{ "daemon": { "state", "pid", "running" } }`.
- Overlap policy `skip` (the v1 default): when a scheduled occurrence fires while a
  prior run of the same job is still in flight, the daemon records it as a
  `skipped_overlap` run (visible in `jobs history`) instead of starting a second run.
- Missed-run handling on startup, honoring each job's `missed_run_policy` for
  occurrences that elapsed while the daemon was down (bounded to a 1-day lookback):
  `skip` (default) records one collapsed `skipped` run so the miss is visible in
  history; `run_once` runs the most recent missed occurrence; `run_all` runs each.
  Occurrence-key dedupe means a run that already completed is never repeated.
- `periodic service install | uninstall | start | stop | status` — run the daemon
  under the per-user service manager (launchd on macOS, `systemd --user` on Linux)
  so it starts at login and restarts on failure. `install` registers a unit that
  runs `periodic daemon start --foreground`; the other subcommands drive it.
- `periodic reload` — validate the config, then apply it dual-mode: a running
  daemon is asked over IPC to atomically swap its in-memory schedule (keeping the
  last-known-good schedule if the new config is invalid); when the daemon is
  stopped, the on-disk config is validated and applied on the next start. A config
  error exits `1` without touching a running daemon.

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
