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
        // Frozen vocabulary (decision 0002): paused == "disabled", same as list/status.
        .stdout(predicates::str::contains("\"state\": \"disabled\""));
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
fn jobs_pause_missing_config_exits_two_not_one() {
    // No config file at all: a system error (exit 2), distinct from a domain
    // refusal like an unknown job (exit 1).
    let home = tempfile::tempdir().unwrap();
    Command::cargo_bin("periodic")
        .unwrap()
        .env("HOME", home.path())
        .args(["jobs", "pause", "cleanup"])
        .assert()
        .code(2);
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
fn jobs_add_appends_a_valid_job_preserving_existing() {
    let home = setup(CONFIG);
    periodic(
        &home,
        &[
            "jobs",
            "add",
            "--id",
            "backup",
            "--every",
            "6h",
            "--command",
            "/usr/bin/backup",
        ],
    )
    .success()
    .stdout(predicates::str::contains("Added"))
    .stdout(predicates::str::contains("backup"));
    let cfg = read_config(&home);
    assert!(cfg.contains("id: backup"), "new job present:\n{cfg}");
    assert!(cfg.contains("/usr/bin/backup"));
    assert!(
        cfg.contains("id: cleanup") && cfg.contains("id: report"),
        "existing kept:\n{cfg}"
    );
    // The result is valid: list parses and sees three jobs.
    periodic(&home, &["jobs", "list"])
        .success()
        .stdout(predicates::str::contains("3 job(s)"));
}

#[test]
fn jobs_add_derives_kebab_id_from_command_basename() {
    let home = setup(CONFIG);
    periodic(
        &home,
        &[
            "jobs",
            "add",
            "--every",
            "6h",
            "--command",
            "/usr/local/bin/sync",
        ],
    )
    .success();
    assert!(
        read_config(&home).contains("id: sync"),
        "{}",
        read_config(&home)
    );
}

#[test]
fn jobs_add_cron_and_calendar_schedules() {
    let home = setup(CONFIG);
    periodic(
        &home,
        &[
            "jobs",
            "add",
            "--id",
            "nightly",
            "--cron",
            "0 3 * * *",
            "--command",
            "x",
        ],
    )
    .success();
    periodic(
        &home,
        &[
            "jobs",
            "add",
            "--id",
            "morning",
            "--every",
            "day",
            "--at",
            "09:00",
            "--command",
            "x",
        ],
    )
    .success();
    let cfg = read_config(&home);
    assert!(cfg.contains("cron:"), "cron job written:\n{cfg}");
    assert!(
        cfg.contains("at: \"09:00\""),
        "time should be quoted:\n{cfg}"
    );
}

#[test]
fn jobs_add_duplicate_id_is_refused_without_writing() {
    let home = setup(CONFIG);
    periodic(
        &home,
        &[
            "jobs",
            "add",
            "--id",
            "cleanup",
            "--every",
            "6h",
            "--command",
            "x",
        ],
    )
    .code(1);
    assert_eq!(
        read_config(&home).matches("id: cleanup").count(),
        1,
        "a colliding add must not duplicate the job"
    );
}

#[test]
fn jobs_add_invalid_schedule_is_refused_without_writing() {
    let home = setup(CONFIG);
    periodic(
        &home,
        &[
            "jobs",
            "add",
            "--id",
            "bad",
            "--every",
            "45m",
            "--command",
            "x",
        ],
    )
    .code(1);
    assert!(
        !read_config(&home).contains("id: bad"),
        "invalid job must not be written"
    );
}

#[test]
fn jobs_add_every_and_cron_conflict_is_a_usage_error() {
    let home = setup(CONFIG);
    periodic(
        &home,
        &[
            "jobs",
            "add",
            "--id",
            "x",
            "--every",
            "6h",
            "--cron",
            "* * * * *",
            "--command",
            "y",
        ],
    )
    .code(2);
}

#[test]
fn jobs_add_requires_command_and_schedule() {
    let home = setup(CONFIG);
    periodic(&home, &["jobs", "add", "--id", "x", "--every", "6h"]).code(1); // no command
    periodic(&home, &["jobs", "add", "--id", "x", "--command", "y"]).code(1); // no schedule
}

#[test]
fn jobs_add_json_reports_added() {
    let home = setup(CONFIG);
    periodic(
        &home,
        &[
            "jobs",
            "add",
            "--id",
            "backup",
            "--every",
            "6h",
            "--command",
            "x",
            "--format",
            "json",
        ],
    )
    .success()
    .stdout(predicates::str::contains("\"id\": \"backup\""))
    .stdout(predicates::str::contains("\"added\": true"));
}

#[test]
fn jobs_list_with_invalid_config_fails() {
    let home = setup(
        "version: 1\njobs:\n  - id: bad\n    schedule: { every: 45m }\n    execution: { command: x }\n",
    );
    periodic(&home, &["jobs", "list"]).failure();
}
