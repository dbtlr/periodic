# Contributing to periodic

## Build & verify

```sh
mise install     # pin the Rust toolchain and just
just verify      # cargo fmt --check + clippy -D warnings + tests (all --locked)
```

`just verify` is the pre-commit gate and mirrors the CI quality gates.

## Release & versioning model

periodic uses a **progressive prerelease channel**. The version in `Cargo.toml`
is *always* a `-next` development version during normal work (e.g. `0.1.0-next`),
and every build-affecting merge to `main` auto-publishes an incrementing
`vX.Y.Z-next.N` prerelease (see `.github/workflows/prerelease.yml`). Install or
update to the latest prerelease with `periodic self-update --next`.

While periodic is pre-1.0, each phase of work is a minor version (`0.1`, `0.2`, …)
and minor versions may carry breaking changes.

### The cycle

1. **Develop** on `X.Y.Z-next`. Build-affecting merges to `main` publish
   `vX.Y.Z-next.1`, `.2`, … as GitHub prereleases.
2. **Cut a clean release** when the increment is done:
   - Promote `CHANGELOG.md`: rename `## Unreleased` to `## vX.Y.Z - YYYY-MM-DD`.
   - `just release X.Y.Z` — bumps to the clean version, refreshes the lockfile,
     commits, and creates the annotated `vX.Y.Z` tag.
   - `git push && git push --tags` — the tag triggers the dist release workflow,
     which builds every platform and publishes `vX.Y.Z` as a normal release.
3. **Reopen the next cycle immediately** so prereleases don't stall:
   - `just open-next X.<Y+1>.0` — sets the version to `X.<Y+1>.0-next` and commits.
   - The version guard (`.github/workflows/version-guard.yml`) fails any build
     that forgets this step.

### One-time repository setup

The prerelease tagger pushes tags with a PAT so the release workflow fires — a
`GITHUB_TOKEN`-pushed tag cannot trigger another workflow (GitHub anti-recursion).
Add a repository secret **`RELEASE_TAG_PAT`**: a fine-grained PAT (or classic
token) with `contents: write` on this repository. A deploy key works equally well.
