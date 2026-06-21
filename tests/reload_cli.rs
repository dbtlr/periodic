use assert_cmd::Command;
use std::fs;

fn setup(config: &str) -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    let cfg = home.path().join(".config/periodic");
    fs::create_dir_all(&cfg).unwrap();
    fs::write(cfg.join("periodic.config.yaml"), config).unwrap();
    home
}
fn periodic(home: &tempfile::TempDir, args: &[&str]) -> assert_cmd::assert::Assert {
    Command::cargo_bin("periodic")
        .unwrap()
        .env("HOME", home.path())
        .args(args)
        .assert()
}

const OK: &str = "version: 1\njobs:\n  - id: ok\n    schedule: { every: 15m }\n    execution: { command: true }\n";
// `every: 7m` is invalid — 7 does not divide 60 (decision 0001).
const BAD: &str = "version: 1\njobs:\n  - id: ok\n    schedule: { every: 7m }\n    execution: { command: true }\n";

#[test]
fn reload_valid_config_without_daemon_reports_not_running() {
    // With no daemon running, reload validates the on-disk config and reports.
    let home = setup(OK);
    periodic(&home, &["reload"])
        .success()
        .stdout(predicates::str::contains("daemon not running"));
}

#[test]
fn reload_invalid_config_exits_one() {
    // A config error is caught before any IPC — never reload an invalid config.
    let home = setup(BAD);
    periodic(&home, &["reload"]).code(1);
}
