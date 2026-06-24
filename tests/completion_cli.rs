use assert_cmd::Command;

fn periodic(args: &[&str]) -> assert_cmd::assert::Assert {
    Command::cargo_bin("periodic").unwrap().args(args).assert()
}

#[test]
fn completion_zsh_emits_script() {
    periodic(&["completion", "zsh"])
        .success()
        .stdout(predicates::str::contains("#compdef periodic"));
}

#[test]
fn completion_bash_emits_script() {
    periodic(&["completion", "bash"])
        .success()
        .stdout(predicates::str::contains("_periodic"));
}

#[test]
fn completion_fish_emits_script() {
    periodic(&["completion", "fish"])
        .success()
        .stdout(predicates::str::contains("complete -c periodic"));
}

#[test]
fn completion_requires_a_shell() {
    // No shell argument -> clap usage error (exit 2).
    periodic(&["completion"]).code(2);
}

#[test]
fn completion_rejects_unknown_shell() {
    // Unknown shell -> clap value-parse error (exit 2).
    periodic(&["completion", "tcsh"]).code(2);
}
