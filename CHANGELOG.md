# Changelog

All notable changes to periodic are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims to
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html). While periodic
is pre-1.0, minor versions (`0.x`) may carry breaking changes.

## [Unreleased]

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
