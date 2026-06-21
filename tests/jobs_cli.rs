use assert_cmd::Command;
use std::fs;

const CONFIG: &str = "version: 1\n\
jobs:\n\
\x20 - id: cleanup\n\
\x20   schedule: { every: 15m }\n\
\x20   execution: { command: x }\n\
\x20 - id: report\n\
\x20   enabled: false\n\
\x20   schedule: { every: day, at: \"09:00\" }\n\
\x20   execution: { command: y }\n";

/// A temp HOME with a config file in place; both the config and the state DB
/// resolve under HOME, so the test never touches the real environment.
fn setup(config: &str) -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    let cfg_dir = home.path().join(".config/periodic");
    fs::create_dir_all(&cfg_dir).unwrap();
    fs::write(cfg_dir.join("periodic.config.yaml"), config).unwrap();
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
fn jobs_list_human_lists_jobs() {
    let home = setup(CONFIG);
    periodic(&home, &["jobs", "list"])
        .success()
        .stdout(predicates::str::contains("cleanup"))
        .stdout(predicates::str::contains("report"))
        .stdout(predicates::str::contains("2 job(s)"));
}

#[test]
fn jobs_list_json_is_machine_readable() {
    let home = setup(CONFIG);
    periodic(&home, &["jobs", "list", "--format", "json"])
        .success()
        .stdout(predicates::str::contains("\"jobs\""))
        .stdout(predicates::str::contains("\"id\": \"cleanup\""));
}

#[test]
fn jobs_status_shows_one_job() {
    let home = setup(CONFIG);
    periodic(&home, &["jobs", "status", "report"])
        .success()
        .stdout(predicates::str::contains("report"))
        .stdout(predicates::str::contains("disabled"));
}

#[test]
fn jobs_status_unknown_job_exits_one() {
    let home = setup(CONFIG);
    periodic(&home, &["jobs", "status", "ghost"]).code(1);
}

#[test]
fn jobs_list_with_invalid_config_fails() {
    let home = setup(
        "version: 1\njobs:\n  - id: bad\n    schedule: { every: 45m }\n    execution: { command: x }\n",
    );
    periodic(&home, &["jobs", "list"]).failure();
}
