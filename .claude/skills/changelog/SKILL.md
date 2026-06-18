---
name: changelog
description: Hard-and-fast rule and conventions for maintaining CHANGELOG.md in periodic. Every user-visible change lands in `## [Unreleased]` BEFORE it ships to main; promoted to a versioned section when a clean release is cut. Use when staging a commit, opening a PR, merging to main, or cutting a release.
---

# CHANGELOG discipline for periodic

## The rule (non-negotiable)

**Every user-visible change must appear in `CHANGELOG.md` under `## [Unreleased]` BEFORE the change lands on `main`.** No exceptions. A commit that adds/changes/removes user-visible behavior without a CHANGELOG entry is incomplete — add the entry before merging.

The rule is enforced at *change time*, not *release time*. periodic publishes a `vX.Y.Z-next.N` prerelease on every build-affecting merge to `main`, so a user-visible change reaches operators (via `periodic self-update --next`) the moment it merges — long before a clean `0.x.0` is cut. By the time you cut a release, `## [Unreleased]` should already be complete; promoting it is a rename, not an authoring pass.

This applies whether the change ships via a direct commit to `main`, a PR merge, or a squash-merge from a feature branch.

## What counts as "user-visible"

**Guiding principle:** *Does this change land in the compiled binary the user runs, or in what the installer puts on their machine?* If yes, it belongs in the CHANGELOG. If no — it lives only in the repo (CI config, contributor docs, dev-only tooling) — it does not.

This is a binary-effect test, not a "did the user request it" test. A runtime dependency bump that ships in the linked binary is user-visible even though the user didn't ask for it; a `deny.toml` tweak that only changes what `cargo-deny` accepts in CI is not, because the user's binary is bit-for-bit identical either way.

**Requires a CHANGELOG entry:**

- New commands, subcommands, flags, or options (`periodic jobs pause`, `--overlap`, `self-update --tag`)
- New or changed YAML config keys (`schedule.every`, `execution.command`, `retry`, `overlap`)
- Changed command behavior, default values, or human output format
- **`--format json` contract changes** — the JSON output is the frozen, semver-versioned agent surface (decision 0002). Any change to it is significant and, if not backward-compatible, **breaking** (see below).
- SQLite state-schema or migration changes that affect persisted runtime state
- Schedule-semantics changes (wall-clock alignment, divisor rules, missed-run/overlap behavior — decisions 0001 and related)
- Removed or renamed surface (command names, flags, config keys, status strings)
- New error variants or actionable error messages users will see
- **Runtime cargo dependency *bumps* that land in the compiled binary** — version changes to existing `[dependencies]` (`Cargo.toml` or transitive `Cargo.lock`), or feature-flag changes that alter what's linked in.
- **New dependencies that affect installation** — e.g. a different TLS backend that pulls in system libraries. *Adding* a runtime dep usually rides along with a feature that already has its own entry; that entry implicitly covers it. Only call the dep out separately when it ships without a corresponding feature change.
- File-location changes (config path `~/.config/periodic/`, the SQLite state path, the IPC socket path, log paths)
- Installer or `self-update` behavior changes

**Does NOT require a CHANGELOG entry:**

- Internal refactors with no observable user/agent difference
- Test additions or test infrastructure
- **Dev-/build-dependency-only changes** (`[dev-dependencies]`, `[build-dependencies]` that don't link into the shipped binary)
- **CI/CD configuration** — workflow files (`ci.yml`, `release.yml`, `prerelease.yml`, `version-guard.yml`), `cargo-dist` regeneration, GitHub Actions version bumps
- **Linter / scanner config that doesn't affect the binary** — `deny.toml`, `rustfmt.toml`, `clippy.toml`
- Code style fixes (rustfmt, clippy compliance)
- Repo-only docs that don't change a documented user/agent contract (README typos, `CONTRIBUTING.md` edits, internal notes)

**The compiled-binary test in practice:**

| Change | Lands in binary / install? | CHANGELOG? |
|---|---|---|
| Add `axoupdater` to `[dependencies]` **as part of the `self-update` command** | Yes (linked) | No separate entry — covered by the feature's own `### Added` line |
| Add a runtime dep with no corresponding feature change (infrastructure prep) | Yes (linked) | Yes — one line under `### Changed` |
| Bump `clap` 4.6.1 → 4.7.0 (no API change, just different bytes) | Yes (different bytes) | Yes — one line under `### Changed` |
| Switch the TLS backend (e.g. native-tls → rustls) | Yes (affects install/runtime) | Yes — note the backend change |
| Add a `[dev-dependencies]` test helper | No (test-only) | No |
| Widen `deny.toml` allow-list for a new transitive license | No (lints CI, not the binary) | No |
| Bump `actions/checkout` v5 → v6 in a workflow | No (CI tool) | No |
| `cargo fmt` sweep | Identical compiled output | No |
| Regenerate `release.yml` via `cargo-dist` | Only if the installer scripts change | Judgment call; lean "yes" when the installer changes |

When in doubt: add an entry. Operators would rather skip a sentence than miss a real change.

## Section structure

The file follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/):

```markdown
# Changelog

All notable changes to periodic are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims to
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html). While periodic
is pre-1.0, minor versions (`0.x`) may carry breaking changes.

## [Unreleased]

### Breaking changes
### Added
### Changed
### Fixed
### Known limitations

## v0.X.0 - YYYY-MM-DD
...
```

Subsection order is fixed (Breaking changes → Added → Changed → Fixed → Known limitations). Omit a subsection entirely if it would be empty. Always keep a `## [Unreleased]` heading at the top, even when empty, so the next author has somewhere to write.

## Subsection guidelines

### Breaking changes

Loud, explicit, named. Each names: **what broke** (the surface + the exact error users see), **the migration path** (even if "no migration shim" — the pre-1.0 default), and **the blast radius**.

Good:
> **`--format json` job records add a required `next_run_at` field (contract v2).** Agents pinned to v1 that reject unknown-shaped records must update. No back-compat shim pre-1.0.

Bad (too vague):
> Breaking change: JSON output changed.

### Added

Lead with what the user/agent sees, not the implementation. Include the command, flag, or config key so it's grep-able.

Good:
> `periodic jobs pause <id>` / `resume <id>` — toggle a job's `enabled` flag in the YAML and trigger a validated reload.

Bad (implementer-narrative):
> Added a JobsCommand::Pause variant in src/cli.rs.

### Changed

Behavior or default that existed but now works differently, without breaking callers.

### Fixed

Bug fixes shipping to `main`. Name the symptom operators observed, not the internal cause.

Good:
> Wall-clock schedules no longer drift after a system sleep; `every: 15m` resumes on the correct `:00/:15/:30/:45` boundary instead of the daemon-relative offset.

### Known limitations

Intentional v1 trade-offs worth documenting. Name the symptom, the workaround, and what triggers the eventual fix.

## Cutting a release

periodic develops on a `X.Y.Z-next` version and publishes prereleases on every build-affecting merge. To cut a clean release:

1. **Confirm `## [Unreleased]` is complete** — walk the commit log since the previous release; every user-visible commit should be represented.
2. **Promote** `## [Unreleased]` to `## vX.Y.Z - YYYY-MM-DD` (today's date), optionally with a one-line release-theme paragraph after the header.
3. **Add a fresh `## [Unreleased]` above it** with the standard intro.
4. **`just release X.Y.Z`** — bumps the version, refreshes the lockfile, commits (including the CHANGELOG promotion), and cuts the annotated tag the dist workflow builds from.
5. **`git push && git push --tags`**, then **`just open-next X.<Y+1>.0`** to reopen the next development cycle. The version guard fails any build that skips the reopen. (Full ceremony: `CONTRIBUTING.md`.)

## Three-layer durability

The CHANGELOG is one of three layers; each answers a different question:

1. **`CHANGELOG.md` `## [Unreleased]`** — human-curated release notes; the primary surface for operators. "What shipped in this release?"
2. **`git log` / squash commit bodies** — the implementation history with SHAs. "What's the history of this code?"
3. **Atlas vault `Workspaces/periodic/`** — `decisions/` (ADRs), `notes/` (specs), and Saga session logs: rationale, alternatives, risks. "Why was it built this way?"

Don't duplicate effort across layers. Specs/plans are transient (reviewed, then deleted on merge) — they are not a durable layer.

## Anti-patterns

- **Catching up at release time.** The rule is at-change-time; reconstructing from `git log` misses things and produces vague summaries.
- **"Various improvements" / "Bug fixes".** Name them.
- **Pasting commit messages verbatim.** Commits talk to engineers ("refactor dispatch out of cli.rs"); CHANGELOG talks to operators and agents ("`periodic self-update` now supports the `-next` channel"). Translate.
- **Leaving no `## [Unreleased]` heading between releases.** Always keep one at the top.
- **Hiding breaking changes under "Changed".** If a caller (or an agent on the JSON contract) must update, it's breaking — its own loud heading.
- **Promoting `## [Unreleased]` partially.** Promote the whole section at release time.

## Quick reference

| Situation | Action |
|---|---|
| New flag, command, config key, or behavior | Bullet under `### Added` in `## [Unreleased]` |
| Changed default or existing behavior (non-breaking) | `### Changed` |
| Removed/renamed surface, or incompatible `--format json` change | `### Breaking changes` with migration path |
| Bug operators have hit | `### Fixed` describing the user-visible symptom |
| Runtime dep added AS PART OF a feature | No separate entry — the feature's `### Added` covers it |
| Runtime dep added/dropped/bumped WITHOUT a feature change | One-liner under `### Changed` |
| Dev/build dep, or `deny.toml`/`*.toml`/workflow tweak | No CHANGELOG entry |
| Internal refactor, no observable change | No CHANGELOG entry |
| Cutting a tagged release | Promote `## [Unreleased]` → `## vX.Y.Z - YYYY-MM-DD`; add fresh `## [Unreleased]`; `just release` then `just open-next` |

## Related

- `CHANGELOG.md` — the file itself
- `CONTRIBUTING.md` — the develop/release/reopen ceremony and the `-next` channel
- decision `0002` (JSON contract) and `0004` (release discipline) in the atlas vault `Workspaces/periodic/decisions/`
