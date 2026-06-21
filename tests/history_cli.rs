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
    Command::cargo_bin("periodic").unwrap().env("HOME", home.path()).args(args).assert()
}
const OK: &str = "version: 1\njobs:\n  - id: ok\n    schedule: { every: 15m }\n    execution: { command: true }\n";

#[test]
fn history_after_run_lists_the_run() {
    let home = setup(OK);
    periodic(&home, &["jobs", "run", "ok"]).success();
    periodic(&home, &["jobs", "history", "ok"]).success()
        .stdout(predicates::str::contains("run(s)"));
}

#[test]
fn history_json_has_runs_array() {
    let home = setup(OK);
    periodic(&home, &["jobs", "run", "ok"]).success();
    periodic(&home, &["jobs", "history", "ok", "--format", "json"]).success()
        .stdout(predicates::str::contains("\"runs\""));
}

#[test]
fn history_unknown_job_exits_one() {
    let home = setup(OK);
    periodic(&home, &["jobs", "history", "ghost"]).code(1);
}
