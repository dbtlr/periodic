# Changelog

All notable changes to periodic are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims to
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html). While periodic
is pre-1.0, minor versions (`0.x`) may carry breaking changes.

## [Unreleased]

The first build of periodic — the `0.1` foundation. Most of this phase is build
and release infrastructure (not user-facing); the user-visible surface is the
installable binary and its update path.

### Added

- The `periodic` binary, installable via the dist shell installer, with shell
  completions (bash/zsh/fish) and a man page.
- `periodic self-update [--next] [--tag]` — update in place from the GitHub
  release channel: latest stable, the `-next` prerelease channel, or a specific tag.
