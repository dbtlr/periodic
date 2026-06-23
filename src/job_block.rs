//! Generate a block-style YAML job entry from `jobs add` flags (PDC-82).
//!
//! periodic has no YAML serializer (serde-saphyr is deserialize-only), so the
//! block is hand-built with conservative quoting. The generated block is always
//! run through validate-before-persist, so a quoting miss surfaces as a refusal,
//! never as a corrupted config.

use crate::cli::JobsAddArgs;

/// The job id for an add: explicit `--id` verbatim (validation checks it), else a
/// kebab-case slug of `--title`, else the command's basename. `None` if none of
/// those yields a non-empty id.
pub(crate) fn derive_id(args: &JobsAddArgs) -> Option<String> {
    if let Some(id) = &args.id {
        return Some(id.clone());
    }
    let from_title = args.title.as_deref().map(kebab).filter(|s| !s.is_empty());
    from_title.or_else(|| {
        args.command
            .as_deref()
            .map(|c| kebab(basename(c)))
            .filter(|s| !s.is_empty())
    })
}

/// The last path segment of a command, ignoring any arguments after a space.
fn basename(cmd: &str) -> &str {
    let first = cmd.split_whitespace().next().unwrap_or(cmd);
    first.rsplit('/').next().unwrap_or(first)
}

/// Coerce arbitrary text into a kebab-case id (`[a-z0-9-]`, no leading/trailing or
/// doubled dashes) — the charset validation enforces.
fn kebab(s: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_end_matches('-').to_string()
}

/// Render a scalar for the generated block, quoting conservatively. Anything that
/// could be misparsed (colons, comments, indicators, bool/number/null lookalikes,
/// surrounding space) is double-quoted; clean tokens like `15m` or `/usr/bin/x`
/// stay plain for readability.
fn scalar(s: &str) -> String {
    let lower = s.to_ascii_lowercase();
    let needs = s.is_empty()
        || s.contains([
            ':', '#', '\n', '\t', '"', '\'', '{', '}', '[', ']', ',', '&', '*', '!', '|', '>', '%',
            '@', '`',
        ])
        || s.starts_with(['-', '?', ' '])
        || s.ends_with(' ')
        || matches!(
            lower.as_str(),
            "true" | "false" | "null" | "yes" | "no" | "~"
        )
        || s.parse::<f64>().is_ok();
    if needs {
        format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
    } else {
        s.to_owned()
    }
}

/// Render `--every` as a YAML value: a comma-separated list becomes a flow
/// sequence (`[a, b]`), a single token stays a scalar.
fn every_value(s: &str) -> String {
    if s.contains(',') {
        let items: Vec<String> = s.split(',').map(|x| scalar(x.trim())).collect();
        format!("[{}]", items.join(", "))
    } else {
        scalar(s)
    }
}

/// Build the block-style `jobs:` list item for a new job. Only the fields the
/// user set are emitted; the result is one list item indented two spaces.
pub(crate) fn build_block(id: &str, args: &JobsAddArgs) -> String {
    let mut b = String::new();
    // Defense in depth: the caller already validates the id is kebab-case (which
    // never needs quoting), but quote it anyway so a raw id can never inject YAML
    // structure (e.g. a newline smuggling extra keys/jobs).
    b.push_str(&format!("  - id: {}\n", scalar(id)));
    if let Some(t) = &args.title {
        b.push_str(&format!("    title: {}\n", scalar(t)));
    }
    if args.disabled {
        b.push_str("    enabled: false\n");
    }

    b.push_str("    schedule:\n");
    if let Some(cron) = &args.cron {
        b.push_str(&format!("      cron: {}\n", scalar(cron)));
    } else if let Some(every) = &args.every {
        b.push_str(&format!("      every: {}\n", every_value(every)));
        if let Some(at) = &args.at {
            b.push_str(&format!("      at: {}\n", scalar(at)));
        }
        if let Some(day) = args.on_day {
            b.push_str(&format!("      on_day: {day}\n"));
        }
        if args.last_day {
            b.push_str("      last_day: true\n");
        }
    }

    b.push_str("    execution:\n");
    b.push_str(&format!(
        "      command: {}\n",
        scalar(args.command.as_deref().unwrap_or(""))
    ));
    if let Some(cwd) = &args.cwd {
        b.push_str(&format!("      cwd: {}\n", scalar(cwd)));
    }

    if let Some(t) = &args.timeout {
        b.push_str(&format!("    timeout: {}\n", scalar(t)));
    }
    if let Some(o) = &args.overlap {
        b.push_str(&format!("    overlap_policy: {}\n", scalar(o)));
    }
    if let Some(r) = args.retry {
        b.push_str(&format!("    retry:\n      max_retries: {r}\n"));
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::OutputFormat;

    fn args() -> JobsAddArgs {
        JobsAddArgs {
            every: None,
            at: None,
            on_day: None,
            last_day: false,
            cron: None,
            command: None,
            cwd: None,
            timeout: None,
            overlap: None,
            retry: None,
            id: None,
            title: None,
            disabled: false,
            format: OutputFormat::Human,
        }
    }

    #[test]
    fn derive_id_prefers_explicit_then_title_then_command() {
        let mut a = args();
        a.command = Some("/usr/bin/backup".into());
        assert_eq!(derive_id(&a).as_deref(), Some("backup"));

        a.title = Some("Daily Cleanup".into());
        assert_eq!(derive_id(&a).as_deref(), Some("daily-cleanup"));

        a.id = Some("explicit-id".into());
        assert_eq!(derive_id(&a).as_deref(), Some("explicit-id"));
    }

    #[test]
    fn derived_ids_are_kebab_case() {
        let mut a = args();
        a.command = Some("/opt/My_Tool.sh --flag".into());
        // basename "My_Tool.sh" -> kebab
        assert_eq!(derive_id(&a).as_deref(), Some("my-tool-sh"));
    }

    #[test]
    fn scalar_quotes_risky_values_and_leaves_clean_ones_plain() {
        assert_eq!(scalar("15m"), "15m");
        assert_eq!(scalar("/usr/bin/backup"), "/usr/bin/backup");
        assert_eq!(scalar("09:00"), "\"09:00\""); // colon
        assert_eq!(scalar("echo hi"), "echo hi"); // internal space is fine plain
        assert_eq!(scalar("true"), "\"true\""); // bool lookalike
        assert_eq!(scalar("42"), "\"42\""); // number lookalike
    }

    #[test]
    fn every_value_renders_lists_and_scalars() {
        assert_eq!(every_value("15m"), "15m");
        assert_eq!(
            every_value("monday,wednesday,friday"),
            "[monday, wednesday, friday]"
        );
    }
}
