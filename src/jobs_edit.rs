//! The `jobs edit` edit/validate loop: a pure driver over an injected editor
//! runner so the looping, header injection/stripping, and no-change/abort logic
//! are exhaustively unit-testable without spawning a real editor. The real
//! `$EDITOR`-spawning runner and on-disk orchestration live in `main.rs`.
#![allow(dead_code)] // wired up by `run_jobs_edit` in Task 2

use crate::{config, validation};

const HEADER_START: &str = "# \u{250c}\u{2500} periodic:";
const HEADER_END: &str = "# \u{2514}\u{2500}";

/// Outcome of the editor loop, before any on-disk dispatch.
#[derive(Debug)]
pub(crate) enum EditResult {
    /// A valid config that differs from the seed — carry it to persist.
    Edited(String),
    /// The user saved the seed unchanged on the first round — clean no-op.
    NoChange,
    /// Empty buffer, gave up on an invalid buffer, or editor exited non-zero.
    Aborted,
}

/// Drive the edit/validate loop. `seed` is the initial buffer; `run_editor` is
/// the editor seam (see module/interface docs for its contract).
pub(crate) fn run_edit_loop(
    seed: &str,
    mut run_editor: impl FnMut(&str) -> std::io::Result<Option<String>>,
) -> std::io::Result<EditResult> {
    // `current` is the last body (header-free) we handed to the editor.
    let mut current = seed.to_string();
    let mut header: Option<String> = None;
    let mut first = true;

    loop {
        let buffer_in = match &header {
            Some(h) => format!("{h}{current}"),
            None => current.clone(),
        };
        let raw = match run_editor(&buffer_in)? {
            Some(s) => s,
            None => return Ok(EditResult::Aborted), // editor exited non-zero
        };
        let body = strip_header(&raw);

        if body.trim().is_empty() {
            return Ok(EditResult::Aborted);
        }
        if body == current {
            // No edit since we last handed them `current`.
            return Ok(if first && header.is_none() {
                EditResult::NoChange // saved the seed untouched on round 1
            } else {
                EditResult::Aborted // gave up on an invalid buffer
            });
        }

        first = false;
        current = body.clone();
        match validate_yaml(&body) {
            Ok(()) => return Ok(EditResult::Edited(body)),
            Err(diags) => header = Some(render_header(&diags)),
        }
    }
}

/// Parse + validate; return the blocking error diagnostics (empty Ok = clean).
fn validate_yaml(yaml: &str) -> Result<(), Vec<crate::diagnostics::Diagnostic>> {
    match config::parse(yaml) {
        Err(d) => Err(vec![d]),
        Ok(raw) => {
            let errs: Vec<_> = validation::validate_config(&raw)
                .into_iter()
                .filter(|d| d.is_error())
                .collect();
            if errs.is_empty() { Ok(()) } else { Err(errs) }
        }
    }
}

/// Render the inert comment-fence header shown above the buffer on a re-loop.
fn render_header(diags: &[crate::diagnostics::Diagnostic]) -> String {
    let mut s = format!(
        "# \u{250c}\u{2500} periodic: {} error(s) \u{2014} fix and save, or save unchanged to abort\n",
        diags.len()
    );
    for d in diags {
        s.push_str(&format!("# \u{2502} {}: {}\n", d.path, d.message));
    }
    s.push_str("# \u{2514}\u{2500}\n");
    s
}

/// Remove a leading contiguous header fence (if present) so it never reaches
/// validation or disk. Best-effort: only strips a fence anchored at line 0.
fn strip_header(buf: &str) -> String {
    if !buf.starts_with(HEADER_START) {
        return buf.to_string();
    }
    let mut lines = buf.lines();
    // consume through the first end-marker line
    for line in lines.by_ref() {
        if line.starts_with(HEADER_END) {
            break;
        }
    }
    let rest = lines.collect::<Vec<_>>().join("\n");
    // preserve a trailing newline shape similar to typical YAML files
    if rest.is_empty() {
        rest
    } else {
        format!("{rest}\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID: &str = "version: 1\njobs: []\n";
    // missing schedule/execution -> a validation error (not just a parse error)
    const INVALID: &str = "version: 1\njobs:\n  - id: x\n";

    #[test]
    fn valid_first_save_returns_edited() {
        let mut seen = Vec::new();
        let r = run_edit_loop("seed: 1\n", |buf| {
            seen.push(buf.to_string());
            Ok(Some(VALID.to_string()))
        })
        .unwrap();
        assert!(matches!(r, EditResult::Edited(c) if c == VALID));
        // first buffer handed to the editor is exactly the seed (no header)
        assert_eq!(seen[0], "seed: 1\n");
    }

    #[test]
    fn no_change_first_save_is_noop() {
        let r = run_edit_loop(VALID, |buf| Ok(Some(buf.to_string()))).unwrap();
        assert!(matches!(r, EditResult::NoChange));
    }

    #[test]
    fn empty_buffer_aborts() {
        let r = run_edit_loop(VALID, |_| Ok(Some("   \n".to_string()))).unwrap();
        assert!(matches!(r, EditResult::Aborted));
    }

    #[test]
    fn editor_nonzero_exit_aborts() {
        let r = run_edit_loop(VALID, |_| Ok(None)).unwrap();
        assert!(matches!(r, EditResult::Aborted));
    }

    #[test]
    fn invalid_then_fixed_loops_with_header_then_succeeds() {
        let mut round = 0;
        let mut second_buf = String::new();
        let r = run_edit_loop("version: 1\n", |buf| {
            round += 1;
            if round == 1 {
                Ok(Some(INVALID.to_string())) // first save: invalid
            } else {
                second_buf = buf.to_string(); // capture what round 2 was handed
                Ok(Some(VALID.to_string())) // user fixes it
            }
        })
        .unwrap();
        assert!(matches!(r, EditResult::Edited(c) if c == VALID));
        assert_eq!(round, 2);
        // round 2's buffer carries the injected error header above the invalid content
        assert!(
            second_buf.starts_with("# "),
            "header should be injected: {second_buf:?}"
        );
        assert!(second_buf.contains("error"));
        assert!(second_buf.contains("version: 1")); // their content preserved below it
    }

    #[test]
    fn invalid_then_saved_unchanged_aborts() {
        let r = run_edit_loop("version: 1\n", |_| Ok(Some(INVALID.to_string()))).unwrap();
        // round 1 invalid -> reopen with header -> round 2 returns same content
        // (stripped == previous) -> give-up -> abort
        assert!(matches!(r, EditResult::Aborted));
    }

    #[test]
    fn injected_header_is_stripped_from_persisted_content() {
        // Simulate an editor that leaves the injected header in place but fixes
        // the body: the returned Edited content must NOT contain the header.
        let mut round = 0;
        let r = run_edit_loop("version: 1\n", |buf| {
            round += 1;
            if round == 1 {
                Ok(Some(INVALID.to_string()))
            } else {
                // Simulate an editor that leaves the injected header comment fence
                // intact and writes valid YAML content below it.  The loop must
                // strip the fence so it never reaches validation or disk.
                // (serde-saphyr uses first-wins for duplicate keys, so we cannot
                // simply concatenate the old invalid body before the new valid
                // body — instead we replace the body with only the valid content.)
                let header_fence: String = buf
                    .lines()
                    .take_while(|l| !l.starts_with(HEADER_END))
                    .flat_map(|l| [l, "\n"])
                    .collect::<String>()
                    + &format!("{HEADER_END}\n");
                Ok(Some(format!("{header_fence}{VALID}")))
            }
        })
        .unwrap();
        match r {
            EditResult::Edited(c) => {
                assert!(!c.contains("# "), "header must be stripped: {c:?}");
                assert!(c.contains("jobs: []"));
            }
            other => panic!("expected Edited, got {other:?}"),
        }
    }
}
