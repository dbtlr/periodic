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
