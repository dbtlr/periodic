set shell := ["bash", "-cu"]

# List available recipes.
default:
    @just --list

# Format the source tree.
fmt:
    cargo fmt

# Check formatting without writing.
fmt-check:
    cargo fmt --check

# Lint with warnings denied.
lint:
    cargo clippy --all-targets --locked -- -D warnings

# Type-check against the committed lockfile.
check:
    cargo check --locked

# Run the test suite.
test:
    cargo test --locked

# Build the debug binary.
build:
    cargo build --locked

# Build the release binary.
build-release:
    cargo build --release --locked

# Install periodic from source.
install:
    cargo install --path . --locked

# The pre-commit gate: formatting, lints, and tests.
verify: fmt-check lint test

# Run periodic with arbitrary args.
run *args:
    cargo run -q -- {{args}}

# Cut a clean release. Promote CHANGELOG.md (## Unreleased -> ## v{{version}})
# by hand first; this then bumps the version, refreshes the lockfile, commits,
# and creates the annotated tag the dist release workflow builds from. Reopen
# the next -next cycle afterwards (see CONTRIBUTING.md).
release version:
    sed -i.bak 's/^version = ".*"/version = "{{version}}"/' Cargo.toml && rm -f Cargo.toml.bak
    cargo check
    git add Cargo.toml Cargo.lock CHANGELOG.md
    git commit -m "Release v{{version}}"
    git tag -a v{{version}} -m "periodic v{{version}}"

# Open the next development cycle after a release: set the version to
# {{version}}-next and commit, so the prerelease channel resumes. The version
# guard fails any build that skips this step. See CONTRIBUTING.md.
open-next version:
    sed -i.bak 's/^version = ".*"/version = "{{version}}-next"/' Cargo.toml && rm -f Cargo.toml.bak
    cargo check
    git add Cargo.toml Cargo.lock
    git commit -m "Open {{version}}-next development cycle"
