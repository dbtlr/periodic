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
const BAD: &str = "version: 1\njobs:\n  - id: nope\n    schedule: { every: 15m }\n    execution: { command: false }\n";
const DISABLED: &str = "version: 1\njobs:\n  - id: off\n    enabled: false\n    schedule: { every: 15m }\n    execution: { command: true }\n";

#[test]
fn run_success_exits_zero() {
    let home = setup(OK);
    periodic(&home, &["jobs", "run", "ok"])
        .success()
        .stdout(predicates::str::contains("success"));
}

#[test]
fn run_failure_exits_one() {
    let home = setup(BAD);
    periodic(&home, &["jobs", "run", "nope"]).code(1);
}

#[test]
fn run_unknown_job_exits_two() {
    let home = setup(OK);
    periodic(&home, &["jobs", "run", "ghost"]).code(2);
}

#[test]
fn disabled_job_runs_on_manual_trigger() {
    let home = setup(DISABLED);
    periodic(&home, &["jobs", "run", "off"]).success();
}

#[test]
fn run_json_has_run_envelope() {
    let home = setup(OK);
    periodic(&home, &["jobs", "run", "ok", "--format", "json"])
        .success()
        .stdout(predicates::str::contains("\"run\""))
        .stdout(predicates::str::contains("\"status\": \"success\""));
}
