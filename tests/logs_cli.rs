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
// echo writes to stdout; capture must round-trip to `logs`.
const ECHOER: &str = "version: 1\njobs:\n  - id: echoer\n    schedule: { every: 15m }\n    execution: { command: echo, args: [hello-logs] }\n";

#[test]
fn logs_show_captured_stdout() {
    let home = setup(ECHOER);
    periodic(&home, &["jobs", "run", "echoer"]).success();
    periodic(&home, &["logs", "echoer"])
        .success()
        .stdout(predicates::str::contains("hello-logs"));
}

#[test]
fn logs_empty_for_unrun_job() {
    let home = setup(ECHOER);
    periodic(&home, &["logs", "echoer"])
        .success()
        .stdout(predicates::str::contains("no output"));
}
