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

fn read_config(home: &tempfile::TempDir) -> String {
    fs::read_to_string(home.path().join(".config/periodic/periodic.config.yaml")).unwrap()
}

#[test]
fn jobs_pause_disables_on_disk() {
    let home = setup(CONFIG);
    periodic(&home, &["jobs", "pause", "cleanup"])
        .success()
        .stdout(predicates::str::contains("Paused"))
        .stdout(predicates::str::contains("cleanup"));
    assert!(
        read_config(&home).contains("enabled: false"),
        "cleanup should be paused on disk:\n{}",
        read_config(&home)
    );
}

#[test]
fn jobs_resume_enables_on_disk() {
    let home = setup(CONFIG);
    // `report` ships with enabled: false.
    periodic(&home, &["jobs", "resume", "report"])
        .success()
        .stdout(predicates::str::contains("Resumed"))
        .stdout(predicates::str::contains("report"));
    let cfg = read_config(&home);
    assert!(
        cfg.contains("enabled: true"),
        "report should be resumed:\n{cfg}"
    );
    assert!(
        !cfg.contains("enabled: false"),
        "no job should remain disabled:\n{cfg}"
    );
}

#[test]
fn jobs_pause_unknown_job_exits_one_without_writing() {
    let home = setup(CONFIG);
    periodic(&home, &["jobs", "pause", "ghost"]).code(1);
    assert_eq!(read_config(&home), CONFIG, "config must be untouched");
}

#[test]
fn jobs_pause_json_reports_state() {
    let home = setup(CONFIG);
    periodic(&home, &["jobs", "pause", "cleanup", "--format", "json"])
        .success()
        .stdout(predicates::str::contains("\"id\": \"cleanup\""))
        .stdout(predicates::str::contains("\"state\": \"paused\""));
}

#[test]
fn jobs_remove_deletes_job_and_keeps_siblings() {
    let home = setup(CONFIG);
    periodic(&home, &["jobs", "remove", "cleanup"])
        .success()
        .stdout(predicates::str::contains("Removed"))
        .stdout(predicates::str::contains("cleanup"));
    let cfg = read_config(&home);
    assert!(
        !cfg.contains("id: cleanup"),
        "cleanup should be gone:\n{cfg}"
    );
    assert!(cfg.contains("id: report"), "report should remain:\n{cfg}");
}

#[test]
fn jobs_remove_unknown_job_exits_one_without_writing() {
    let home = setup(CONFIG);
    periodic(&home, &["jobs", "remove", "ghost"]).code(1);
    assert_eq!(read_config(&home), CONFIG, "config must be untouched");
}

#[test]
fn jobs_remove_json_reports_removed() {
    let home = setup(CONFIG);
    periodic(&home, &["jobs", "remove", "cleanup", "--format", "json"])
        .success()
        .stdout(predicates::str::contains("\"id\": \"cleanup\""))
        .stdout(predicates::str::contains("\"removed\": true"));
}

#[test]
fn jobs_list_with_invalid_config_fails() {
    let home = setup(
        "version: 1\njobs:\n  - id: bad\n    schedule: { every: 45m }\n    execution: { command: x }\n",
    );
    periodic(&home, &["jobs", "list"]).failure();
}
