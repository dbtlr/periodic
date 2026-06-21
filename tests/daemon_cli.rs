//! Daemon lifecycle integration: start (detached) → status → stop, over a real
//! detached process and Unix socket, in an isolated `HOME`/`XDG_RUNTIME_DIR`.
//!
//! This drives the orchestration surface deterministically. It does NOT assert a
//! scheduled job *fires* inside the loop: the smallest schedule (`every: 1m`)
//! aligns to wall-clock minute boundaries, so a fire is up to ~60s out — too slow
//! and timing-sensitive for a reliable test. That path is dogfooded against the
//! live daemon instead (the loop's dispatch/dedupe is unit-tested in `daemon.rs`).

use assert_cmd::Command;
use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

const CONFIG: &str = "version: 1\njobs:\n  - id: tick\n    schedule: { every: 1m }\n    execution: { command: \"true\" }\n";

struct Env {
    home: tempfile::TempDir,
    runtime: tempfile::TempDir,
}

fn setup() -> Env {
    let home = tempfile::tempdir().unwrap();
    let cfg = home.path().join(".config/periodic");
    fs::create_dir_all(&cfg).unwrap();
    fs::write(cfg.join("periodic.config.yaml"), CONFIG).unwrap();
    let runtime = tempfile::tempdir().unwrap();
    Env { home, runtime }
}

fn periodic(env: &Env, args: &[&str]) -> assert_cmd::assert::Assert {
    Command::cargo_bin("periodic")
        .unwrap()
        .env("HOME", env.home.path())
        .env("XDG_RUNTIME_DIR", env.runtime.path())
        .args(args)
        .assert()
}

/// Poll `daemon status --format json` until its stdout satisfies `pred`, or panic.
fn wait_for_status(env: &Env, pred: impl Fn(&str) -> bool) -> String {
    // Generous margin: graceful shutdown passes through a transient `stopping`
    // state and may drain in-flight runs before reaching `stopped`.
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let out = Command::cargo_bin("periodic")
            .unwrap()
            .env("HOME", env.home.path())
            .env("XDG_RUNTIME_DIR", env.runtime.path())
            .args(["daemon", "status", "--format", "json"])
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        if pred(&text) {
            return text;
        }
        assert!(
            Instant::now() < deadline,
            "status never matched; last was: {text}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn status_reports_not_running_before_start() {
    let env = setup();
    periodic(&env, &["daemon", "status"])
        .success()
        .stdout(predicates::str::contains("not running"));
}

#[test]
fn start_detached_then_status_then_stop() {
    let env = setup();

    periodic(&env, &["daemon", "start", "--detach"])
        .success()
        .stdout(predicates::str::contains("daemon started (pid"));

    let running = wait_for_status(&env, |s| s.contains("\"running\": true"));
    assert!(running.contains("\"state\": \"running\""), "got {running}");

    // A second start must refuse while the first is live.
    periodic(&env, &["daemon", "start", "--detach"])
        .code(1)
        .stderr(predicates::str::contains("already running"));

    periodic(&env, &["daemon", "stop"])
        .success()
        .stdout(predicates::str::contains("signalled to stop"));

    // Wait for the terminal `stopped` state specifically — `running: false` alone
    // also matches the transient `stopping` state during graceful shutdown.
    let stopped = wait_for_status(&env, |s| s.contains("\"state\": \"stopped\""));
    assert!(stopped.contains("\"running\": false"), "got {stopped}");

    // Stop is idempotent once the daemon is down.
    periodic(&env, &["daemon", "stop"])
        .success()
        .stdout(predicates::str::contains("not running"));

    // The socket is removed on clean shutdown.
    let sock = env.runtime.path().join("periodic/periodic.sock");
    assert!(
        !Path::new(&sock).exists(),
        "socket should be removed on shutdown"
    );
}

#[test]
fn stop_is_idempotent_when_never_started() {
    let env = setup();
    periodic(&env, &["daemon", "stop"])
        .success()
        .stdout(predicates::str::contains("not running"));
}
