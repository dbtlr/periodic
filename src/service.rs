//! `periodic service …`: register the daemon with the per-user service manager so
//! it runs in the background and across logins.
//!
//! Two backends, split by `cfg`:
//! - **macOS**: a launchd LaunchAgent (`~/Library/LaunchAgents/<label>.plist`),
//!   driven with `launchctl`.
//! - **Linux**: a systemd `--user` unit (`~/.config/systemd/user/periodic.service`),
//!   driven with `systemctl --user`.
//!
//! The service runs `periodic daemon start --foreground`; the service manager owns
//! backgrounding and restart. The file-content generation and path resolution are
//! pure functions ([`plist_contents`], [`plist_path`], [`unit_contents`],
//! [`unit_path`]) so they can be unit-tested without touching the real system.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::cli::ServiceCommand;

/// The launchd label / reverse-DNS identifier for the daemon agent (macOS).
pub(crate) const LAUNCHD_LABEL: &str = "com.dbtlr.periodic.daemon";

/// The systemd `--user` unit name (without the `.service` suffix) (Linux).
/// Exercised by the cross-platform unit-content tests; the runtime backend that
/// consumes it is `cfg(target_os = "linux")` only.
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
pub(crate) const SYSTEMD_UNIT: &str = "periodic";

/// Route `periodic service …` to its handler.
pub(crate) fn run(cmd: ServiceCommand) -> anyhow::Result<ExitCode> {
    #[cfg(target_os = "macos")]
    {
        macos::run(cmd)
    }
    #[cfg(target_os = "linux")]
    {
        linux::run(cmd)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = cmd;
        anyhow::bail!(
            "`periodic service` is not supported on this platform \
             (only macOS launchd and Linux systemd --user)"
        )
    }
}

/// Resolve `$HOME` as a path, erroring clearly when it is unset.
fn home_dir() -> anyhow::Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or_else(|| anyhow::anyhow!("HOME is not set; cannot locate the service directory"))
}

// ─── macOS: launchd ──────────────────────────────────────────────────────────

/// Path to the LaunchAgent plist under `<home>/Library/LaunchAgents/`.
fn plist_path(home: &Path) -> PathBuf {
    home.join("Library/LaunchAgents")
        .join(format!("{LAUNCHD_LABEL}.plist"))
}

/// Generate the launchd plist for the daemon agent. `exe` is the periodic binary,
/// `log_dir` the directory for stdout/stderr capture. Pure string generation.
fn plist_contents(exe: &Path, log_dir: &Path) -> String {
    let exe = exe.display();
    let stdout = log_dir.join("daemon.out.log");
    let stderr = log_dir.join("daemon.err.log");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>{LAUNCHD_LABEL}</string>
    <key>ProgramArguments</key>
    <array>
        <string>{exe}</string>
        <string>daemon</string>
        <string>start</string>
        <string>--foreground</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>{stdout}</string>
    <key>StandardErrorPath</key>
    <string>{stderr}</string>
</dict>
</plist>
"#,
        stdout = stdout.display(),
        stderr = stderr.display(),
    )
}

#[cfg(target_os = "macos")]
mod macos {
    use super::*;
    use std::process::Command;

    pub(super) fn run(cmd: ServiceCommand) -> anyhow::Result<ExitCode> {
        match cmd {
            ServiceCommand::Install => install(),
            ServiceCommand::Uninstall => uninstall(),
            ServiceCommand::Start => start(),
            ServiceCommand::Stop => stop(),
            ServiceCommand::Status => status(),
        }
    }

    fn log_dir(home: &Path) -> PathBuf {
        home.join(".local/state/periodic")
    }

    fn install() -> anyhow::Result<ExitCode> {
        let home = home_dir()?;
        let exe = std::env::current_exe()?;
        let plist = plist_path(&home);
        let logs = log_dir(&home);

        std::fs::create_dir_all(&logs)
            .map_err(|e| anyhow::anyhow!("cannot create {}: {e}", logs.display()))?;
        if let Some(parent) = plist.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| anyhow::anyhow!("cannot create {}: {e}", parent.display()))?;
        }
        std::fs::write(&plist, plist_contents(&exe, &logs))
            .map_err(|e| anyhow::anyhow!("cannot write {}: {e}", plist.display()))?;

        launchctl(&["load", "-w", &plist.display().to_string()])?;
        println!("service installed and loaded ({LAUNCHD_LABEL})");
        Ok(ExitCode::SUCCESS)
    }

    fn uninstall() -> anyhow::Result<ExitCode> {
        let home = home_dir()?;
        let plist = plist_path(&home);
        // Unload first; ignore failure (it may already be unloaded).
        let _ = launchctl(&["unload", "-w", &plist.display().to_string()]);
        match std::fs::remove_file(&plist) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => anyhow::bail!("cannot remove {}: {e}", plist.display()),
        }
        println!("service uninstalled ({LAUNCHD_LABEL})");
        Ok(ExitCode::SUCCESS)
    }

    fn start() -> anyhow::Result<ExitCode> {
        launchctl(&["start", LAUNCHD_LABEL])?;
        println!("service started ({LAUNCHD_LABEL})");
        Ok(ExitCode::SUCCESS)
    }

    fn stop() -> anyhow::Result<ExitCode> {
        launchctl(&["stop", LAUNCHD_LABEL])?;
        println!("service stopped ({LAUNCHD_LABEL})");
        Ok(ExitCode::SUCCESS)
    }

    fn status() -> anyhow::Result<ExitCode> {
        let output = Command::new("launchctl")
            .args(["list", LAUNCHD_LABEL])
            .output()
            .map_err(|e| anyhow::anyhow!("failed to run launchctl: {e}"))?;
        if !output.status.success() {
            println!("service not loaded ({LAUNCHD_LABEL})");
            return Ok(ExitCode::from(1));
        }
        let text = String::from_utf8_lossy(&output.stdout);
        let view = LaunchctlStatus::parse(&text);
        print!("{}", view.render(LAUNCHD_LABEL));
        Ok(ExitCode::SUCCESS)
    }

    /// Run `launchctl <args>`, surfacing a non-zero exit or spawn failure.
    fn launchctl(args: &[&str]) -> anyhow::Result<()> {
        let status = Command::new("launchctl")
            .args(args)
            .status()
            .map_err(|e| anyhow::anyhow!("failed to run launchctl: {e}"))?;
        if !status.success() {
            anyhow::bail!("launchctl {} failed ({status})", args.join(" "));
        }
        Ok(())
    }
}

/// A parsed view of `launchctl list <label>` output: loaded, and the running pid
/// if any. Pure so it is unit-testable from a sample blob.
#[cfg(target_os = "macos")]
struct LaunchctlStatus {
    pid: Option<i64>,
}

#[cfg(target_os = "macos")]
impl LaunchctlStatus {
    /// Parse the `"PID" = NNNN;` line from `launchctl list <label>`. A missing or
    /// `-` pid means loaded-but-not-running.
    fn parse(text: &str) -> Self {
        let pid = text.lines().find_map(|line| {
            let line = line.trim();
            let rest = line.strip_prefix("\"PID\"")?;
            let value = rest
                .trim_start_matches([' ', '='])
                .trim_end_matches(';')
                .trim();
            value.parse::<i64>().ok()
        });
        LaunchctlStatus { pid }
    }

    fn render(&self, label: &str) -> String {
        match self.pid {
            Some(pid) => format!("service: running (pid {pid}, {label})\n"),
            None => format!("service: loaded, not running ({label})\n"),
        }
    }
}

// ─── Linux: systemd --user ───────────────────────────────────────────────────

/// Path to the systemd `--user` unit under `<home>/.config/systemd/user/`.
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
fn unit_path(home: &Path) -> PathBuf {
    home.join(".config/systemd/user")
        .join(format!("{SYSTEMD_UNIT}.service"))
}

/// Generate the systemd `--user` unit for the daemon. `exe` is the periodic
/// binary. Pure string generation.
#[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
fn unit_contents(exe: &Path) -> String {
    let exe = exe.display();
    format!(
        "[Unit]\n\
         Description=periodic user-space job scheduler daemon\n\
         \n\
         [Service]\n\
         ExecStart={exe} daemon start --foreground\n\
         Restart=on-failure\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::process::Command;

    pub(super) fn run(cmd: ServiceCommand) -> anyhow::Result<ExitCode> {
        match cmd {
            ServiceCommand::Install => install(),
            ServiceCommand::Uninstall => uninstall(),
            ServiceCommand::Start => simple("start", "started"),
            ServiceCommand::Stop => simple("stop", "stopped"),
            ServiceCommand::Status => status(),
        }
    }

    fn install() -> anyhow::Result<ExitCode> {
        let home = home_dir()?;
        let exe = std::env::current_exe()?;
        let unit = unit_path(&home);
        if let Some(parent) = unit.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| anyhow::anyhow!("cannot create {}: {e}", parent.display()))?;
        }
        std::fs::write(&unit, unit_contents(&exe))
            .map_err(|e| anyhow::anyhow!("cannot write {}: {e}", unit.display()))?;

        systemctl(&["daemon-reload"])?;
        systemctl(&["enable", "--now", SYSTEMD_UNIT])?;
        println!("service installed and enabled ({SYSTEMD_UNIT})");
        Ok(ExitCode::SUCCESS)
    }

    fn uninstall() -> anyhow::Result<ExitCode> {
        let home = home_dir()?;
        let unit = unit_path(&home);
        let _ = systemctl(&["disable", "--now", SYSTEMD_UNIT]);
        match std::fs::remove_file(&unit) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => anyhow::bail!("cannot remove {}: {e}", unit.display()),
        }
        let _ = systemctl(&["daemon-reload"]);
        println!("service uninstalled ({SYSTEMD_UNIT})");
        Ok(ExitCode::SUCCESS)
    }

    fn simple(action: &str, past: &str) -> anyhow::Result<ExitCode> {
        systemctl(&[action, SYSTEMD_UNIT])?;
        println!("service {past} ({SYSTEMD_UNIT})");
        Ok(ExitCode::SUCCESS)
    }

    fn status() -> anyhow::Result<ExitCode> {
        // `systemctl --user status` exits non-zero when the unit is inactive; pass
        // its output and exit code through rather than treating that as an error.
        let status = Command::new("systemctl")
            .args(["--user", "status", SYSTEMD_UNIT])
            .status()
            .map_err(|e| anyhow::anyhow!("failed to run systemctl: {e}"))?;
        Ok(match status.code() {
            Some(0) => ExitCode::SUCCESS,
            Some(c) => ExitCode::from(c as u8),
            None => ExitCode::from(1),
        })
    }

    /// Run `systemctl --user <args>`, surfacing a non-zero exit or spawn failure.
    fn systemctl(args: &[&str]) -> anyhow::Result<()> {
        let mut full = vec!["--user"];
        full.extend_from_slice(args);
        let status = Command::new("systemctl")
            .args(&full)
            .status()
            .map_err(|e| anyhow::anyhow!("failed to run systemctl: {e}"))?;
        if !status.success() {
            anyhow::bail!("systemctl {} failed ({status})", full.join(" "));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── path resolution (cross-platform: pure path joins) ─────────────────────

    #[test]
    fn plist_path_is_under_launch_agents() {
        let home = Path::new("/Users/alice");
        let p = plist_path(home);
        assert_eq!(
            p,
            Path::new("/Users/alice/Library/LaunchAgents/com.dbtlr.periodic.daemon.plist")
        );
    }

    #[test]
    fn unit_path_is_under_config_systemd_user() {
        let home = Path::new("/home/bob");
        let p = unit_path(home);
        assert_eq!(
            p,
            Path::new("/home/bob/.config/systemd/user/periodic.service")
        );
    }

    // ── macOS plist content (pure string-gen) ─────────────────────────────────

    #[test]
    fn plist_contains_label_exe_and_daemon_args() {
        let plist = plist_contents(
            Path::new("/usr/local/bin/periodic"),
            Path::new("/Users/alice/.local/state/periodic"),
        );
        assert!(plist.contains("com.dbtlr.periodic.daemon"));
        assert!(plist.contains("/usr/local/bin/periodic"));
        assert!(plist.contains("<string>daemon</string>"));
        assert!(plist.contains("<string>start</string>"));
        assert!(plist.contains("<string>--foreground</string>"));
    }

    #[test]
    fn plist_sets_run_at_load_and_keep_alive() {
        let plist = plist_contents(Path::new("/bin/periodic"), Path::new("/tmp/logs"));
        assert!(plist.contains("<key>RunAtLoad</key>"));
        assert!(plist.contains("<key>KeepAlive</key>"));
        assert!(plist.contains("/tmp/logs/daemon.out.log"));
        assert!(plist.contains("/tmp/logs/daemon.err.log"));
    }

    // ── Linux systemd unit content (pure string-gen) ──────────────────────────

    #[test]
    fn unit_contains_execstart_args_restart_and_wantedby() {
        let unit = unit_contents(Path::new("/usr/bin/periodic"));
        assert!(unit.contains("ExecStart=/usr/bin/periodic daemon start --foreground"));
        assert!(unit.contains("Restart=on-failure"));
        assert!(unit.contains("WantedBy=default.target"));
    }

    // ── launchctl status parsing (macOS only) ─────────────────────────────────

    #[cfg(target_os = "macos")]
    #[test]
    fn launchctl_status_parses_running_pid() {
        let blob = "{\n\t\"PID\" = 4242;\n\t\"Label\" = \"com.dbtlr.periodic.daemon\";\n}";
        let s = LaunchctlStatus::parse(blob);
        assert_eq!(s.pid, Some(4242));
        assert!(s.render(LAUNCHD_LABEL).contains("running (pid 4242"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn launchctl_status_without_pid_is_loaded_not_running() {
        let blob = "{\n\t\"Label\" = \"com.dbtlr.periodic.daemon\";\n}";
        let s = LaunchctlStatus::parse(blob);
        assert_eq!(s.pid, None);
        assert!(s.render(LAUNCHD_LABEL).contains("loaded, not running"));
    }
}
