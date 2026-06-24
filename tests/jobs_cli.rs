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
fn jobs_add_rejects_id_yaml_injection() {
    let home = setup(CONFIG);
    let evil = "real\n    schedule: { every: 1h }\n    execution: { command: c }\n  - id: smuggled";
    periodic(
        &home,
        &[
            "jobs",
            "add",
            "--id",
            evil,
            "--every",
            "6h",
            "--command",
            "x",
        ],
    )
    .code(1);
    let cfg = read_config(&home);
    assert!(!cfg.contains("smuggled"), "no structure injection:\n{cfg}");
    assert_eq!(cfg, CONFIG, "config must be untouched");
}

#[test]
fn jobs_add_rejects_empty_and_null_id() {
    let home = setup(CONFIG);
    periodic(
        &home,
        &["jobs", "add", "--id", "", "--every", "6h", "--command", "x"],
    )
    .code(1);
    periodic(
        &home,
        &[
            "jobs",
            "add",
            "--id",
            "~",
            "--every",
            "6h",
            "--command",
            "x",
        ],
    )
    .code(1);
    periodic(
        &home,
        &[
            "jobs",
            "add",
            "--id",
            "Not Kebab",
            "--every",
            "6h",
            "--command",
            "x",
        ],
    )
    .code(1);
    assert_eq!(read_config(&home), CONFIG, "config must be untouched");
}

#[test]
fn jobs_add_empty_title_falls_back_to_command_basename() {
    let home = setup(CONFIG);
    periodic(
        &home,
        &[
            "jobs",
            "add",
            "--title",
            "",
            "--every",
            "6h",
            "--command",
            "/usr/bin/backup",
        ],
    )
    .success();
    assert!(
        read_config(&home).contains("id: backup"),
        "{}",
        read_config(&home)
    );
}

#[test]
fn jobs_list_with_invalid_config_fails() {
    let home = setup(
        "version: 1\njobs:\n  - id: bad\n    schedule: { every: 45m }\n    execution: { command: x }\n",
    );
    periodic(&home, &["jobs", "list"]).failure();
}

fn periodic_env(
    home: &tempfile::TempDir,
    envs: &[(&str, &str)],
    args: &[&str],
) -> assert_cmd::assert::Assert {
    let mut cmd = Command::cargo_bin("periodic").unwrap();
    cmd.env("HOME", home.path());
    // Clear inherited interactive editor vars so the test-supplied EDITOR wins.
    cmd.env_remove("VISUAL");
    for (k, v) in envs {
        cmd.env(k, v);
    }
    cmd.args(args).assert()
}

/// Write an executable shell script into `home` that, when run as `$EDITOR`
/// with the temp path as its argument, overwrites that file with `new_content`.
fn editor_script(home: &tempfile::TempDir, new_content: &str) -> std::path::PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = home.path().join("fake-editor.sh");
    // `$1` is the temp file path passed by periodic's `sh -c '<editor> "$1"'`.
    let script = format!("#!/bin/sh\ncat > \"$1\" <<'PERIODIC_EOF'\n{new_content}PERIODIC_EOF\n");
    fs::write(&path, script).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[test]
fn jobs_edit_no_change_is_noop() {
    let home = setup(CONFIG);
    periodic_env(&home, &[("EDITOR", "true")], &["jobs", "edit"])
        .success()
        .stdout(predicates::str::contains("No changes"));
    assert_eq!(read_config(&home), CONFIG); // untouched
}

#[test]
fn jobs_edit_applies_valid_change() {
    let home = setup(CONFIG);
    let new = "version: 1\njobs:\n  - id: only\n    schedule: { every: 1h }\n    execution: { command: z }\n";
    let editor = editor_script(&home, new);
    periodic_env(
        &home,
        &[("EDITOR", editor.to_str().unwrap())],
        &["jobs", "edit"],
    )
    .success()
    .stdout(predicates::str::contains("Config updated"));
    assert!(read_config(&home).contains("id: only"));
}

#[test]
fn jobs_edit_invalid_then_giveup_aborts() {
    let home = setup(CONFIG);
    // a job with no schedule/execution -> validation error; the static editor
    // re-writes the same invalid content each round -> give-up on round 2.
    let editor = editor_script(&home, "version: 1\njobs:\n  - id: broken\n");
    periodic_env(
        &home,
        &[("EDITOR", editor.to_str().unwrap())],
        &["jobs", "edit"],
    )
    .code(1);
    assert_eq!(read_config(&home), CONFIG); // untouched
}
