use assert_cmd::Command;

fn run(fixture: &str, args: &[&str]) -> assert_cmd::assert::Assert {
    let path = format!("{}/tests/fixtures/{fixture}", env!("CARGO_MANIFEST_DIR"));
    let mut cmd = Command::cargo_bin("periodic").unwrap();
    cmd.arg("validate").arg(&path).args(args).assert()
}

#[test]
fn valid_config_exits_zero() {
    run("valid.yaml", &[]).success();
}

#[test]
fn invalid_config_exits_one() {
    run("invalid.yaml", &[]).code(1);
}

#[test]
fn missing_file_exits_two() {
    run("does-not-exist.yaml", &[]).code(2);
}

#[test]
fn json_format_is_machine_readable() {
    run("invalid.yaml", &["--format", "json"])
        .code(1)
        .stdout(predicates::str::contains("\"ok\": false"));
}
