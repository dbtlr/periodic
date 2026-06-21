use assert_cmd::Command;
use std::fs;

const CONFIG: &str = "version: 1\n\
jobs:\n\
\x20 - id: cleanup\n\
\x20   schedule: { every: 15m }\n\
\x20   execution: { command: x }\n";

fn home_with_config() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    let cfg_dir = home.path().join(".config/periodic");
    fs::create_dir_all(&cfg_dir).unwrap();
    fs::write(cfg_dir.join("periodic.config.yaml"), CONFIG).unwrap();
    home
}

fn periodic(home: &tempfile::TempDir, args: &[&str]) -> assert_cmd::assert::Assert {
    Command::cargo_bin("periodic")
        .unwrap()
        .env("HOME", home.path())
        .args(args)
        .assert()
}

#[test]
fn doctor_reports_db_not_yet_created() {
    let home = tempfile::tempdir().unwrap();
    periodic(&home, &["doctor"])
        .success()
        .stdout(predicates::str::contains("not yet created"));
}

#[test]
fn doctor_reports_healthy_after_db_is_created() {
    let home = home_with_config();
    // `jobs list` creates and migrates the state DB.
    periodic(&home, &["jobs", "list"]).success();
    periodic(&home, &["doctor"])
        .success()
        .stdout(predicates::str::contains("healthy"));
}
