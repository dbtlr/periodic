//! Surgical, format-preserving edits to the desired-state YAML (ADR 0009).
//!
//! Every mutation operates on the *source text* — locating the affected span via
//! the span-aware `granit-parser` event stream and splicing bytes — never by
//! re-serializing the in-memory model. The user's comments, key ordering, and
//! formatting survive every edit. Cases the surgical writer cannot edit safely
//! (flow-style `jobs`, anchors/merge keys spanning the target, unparseable input)
//! are refused with an error pointing at `jobs edit`, never silently rewritten.

use std::ops::Range;

use granit_parser::{Event, Parser, Span};

/// Toggle a job's `enabled:` field in place, preserving all surrounding text.
///
/// Returns the edited YAML source. Errors if the job id is not found or the
/// surrounding YAML cannot be edited safely.
pub(crate) fn set_enabled(source: &str, job_id: &str, enabled: bool) -> anyhow::Result<String> {
    let events = collect_events(source)?;
    let (range, text) = plan_set_enabled(source, &events, job_id, enabled)
        .ok_or_else(|| anyhow::anyhow!("job '{job_id}' not found (use `jobs edit`)"))?;

    let mut out = String::with_capacity(source.len() + text.len());
    out.push_str(&source[..range.start]);
    out.push_str(&text);
    out.push_str(&source[range.end..]);
    Ok(out)
}

/// Parse the source into the span-aware event stream. Borrows `source`.
fn collect_events(source: &str) -> anyhow::Result<Vec<(Event<'_>, Span)>> {
    let mut parser = Parser::new_from_str(source);
    let mut events = Vec::new();
    while let Some(result) = parser.next_event() {
        let (event, span) = result.map_err(|e| anyhow::anyhow!("invalid YAML: {e}"))?;
        events.push((event, span));
    }
    Ok(events)
}

/// Index of the next event carrying data, skipping presentation-only comments.
fn skip_comments(events: &[(Event, Span)], mut i: usize) -> usize {
    while matches!(events.get(i), Some((Event::Comment(..), _))) {
        i += 1;
    }
    i
}

/// Given `start` at a value node, return the index just past that whole node
/// (a scalar is one event; a mapping/sequence spans to its matching end).
fn end_of_node(events: &[(Event, Span)], start: usize) -> usize {
    match events.get(start).map(|(e, _)| e) {
        Some(Event::MappingStart(..) | Event::SequenceStart(..)) => {
            let mut depth = 0usize;
            let mut i = start;
            while i < events.len() {
                match &events[i].0 {
                    Event::MappingStart(..) | Event::SequenceStart(..) => depth += 1,
                    Event::MappingEnd | Event::SequenceEnd => {
                        depth -= 1;
                        if depth == 0 {
                            return i + 1;
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            i
        }
        _ => start + 1,
    }
}

/// The (key_idx, value_idx) pairs of a block mapping starting at `map_start`.
fn map_entries(events: &[(Event, Span)], map_start: usize) -> Vec<(usize, usize)> {
    let mut pairs = Vec::new();
    let mut i = skip_comments(events, map_start + 1);
    while !matches!(events.get(i).map(|(e, _)| e), Some(Event::MappingEnd) | None) {
        let key_idx = i;
        let val_idx = skip_comments(events, key_idx + 1);
        pairs.push((key_idx, val_idx));
        i = skip_comments(events, end_of_node(events, val_idx));
    }
    pairs
}

/// Scalar text at `idx`, if the event there is a scalar.
fn scalar_at<'a>(events: &'a [(Event, Span)], idx: usize) -> Option<&'a str> {
    match events.get(idx) {
        Some((Event::Scalar(value, ..), _)) => Some(value.as_ref()),
        _ => None,
    }
}

/// Leading whitespace of the source line containing byte offset `at`.
fn line_indent_of(source: &str, at: usize) -> &str {
    let line_start = source[..at].rfind('\n').map_or(0, |n| n + 1);
    let line = &source[line_start..];
    let ws_len = line.len() - line.trim_start().len();
    &line[..ws_len]
}

/// Byte range to splice and the replacement text to toggle/insert a job's
/// `enabled:` field. A non-empty range replaces an existing value; an empty
/// range inserts a new `enabled:` line indented to match the job's other keys.
fn plan_set_enabled(
    source: &str,
    events: &[(Event, Span)],
    job_id: &str,
    enabled: bool,
) -> Option<(Range<usize>, String)> {
    let literal = if enabled { "true" } else { "false" };

    let root = events
        .iter()
        .position(|(e, _)| matches!(e, Event::MappingStart(..)))?;

    // Find the `jobs:` sequence at the document root.
    let jobs_seq = map_entries(events, root).into_iter().find_map(|(k, v)| {
        (scalar_at(events, k) == Some("jobs")
            && matches!(events.get(v).map(|(e, _)| e), Some(Event::SequenceStart(..))))
        .then_some(v)
    })?;

    // Walk each job mapping in the sequence; match by `id`.
    let mut i = skip_comments(events, jobs_seq + 1);
    while !matches!(events.get(i).map(|(e, _)| e), Some(Event::SequenceEnd) | None) {
        let item = i;
        if matches!(events[item].0, Event::MappingStart(..)) {
            let entries = map_entries(events, item);
            let is_target = entries
                .iter()
                .any(|&(k, v)| scalar_at(events, k) == Some("id") && scalar_at(events, v) == Some(job_id));
            if is_target {
                // Existing `enabled:` → replace its value span.
                if let Some(range) = entries.iter().find_map(|&(k, v)| {
                    (scalar_at(events, k) == Some("enabled")).then(|| events[v].1.byte_range())?
                }) {
                    return Some((range, literal.to_owned()));
                }
                // Absent → insert a new line after the first entry, indented to
                // match a clean sibling key (the first key shares the `- ` dash line).
                let (first_k, first_v) = *entries.first()?;
                let indent_key = entries.get(1).map_or(first_k, |&(k, _)| k);
                let indent_byte = events[indent_key].1.byte_range().map_or(0, |r| r.start);
                let indent = line_indent_of(source, indent_byte);
                let after_first = events[end_of_node(events, first_v) - 1].1.byte_range()?.end;
                let at = source[after_first..]
                    .find('\n')
                    .map_or(source.len(), |n| after_first + n);
                return Some((at..at, format!("\n{indent}enabled: {literal}")));
            }
        }
        i = skip_comments(events, end_of_node(events, item));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMENTED: &str = "\
version: 1
jobs:
  # keep the box tidy
  - id: cleanup           # nightly sweep
    enabled: true
    schedule:
      every: 15m
    execution:
      command: /usr/bin/true
";

    const TWO_JOBS_NO_ENABLED: &str = "\
version: 1
jobs:
  - id: cleanup           # nightly sweep
    schedule:
      every: 15m
    execution:
      command: /usr/bin/true
  - id: backup
    enabled: true
    schedule:
      every: 6h
    execution:
      command: /usr/bin/backup
";

    #[test]
    fn set_enabled_false_toggles_field_and_preserves_comments() {
        let out = set_enabled(COMMENTED, "cleanup", false).expect("edit should succeed");

        assert!(out.contains("enabled: false"), "field should be toggled:\n{out}");
        assert!(!out.contains("enabled: true"), "old value should be gone:\n{out}");
        // Comments and untouched lines survive verbatim (the whole point of ADR 0009).
        assert!(out.contains("# keep the box tidy"), "standalone comment lost:\n{out}");
        assert!(out.contains("# nightly sweep"), "inline comment lost:\n{out}");
        assert!(out.contains("command: /usr/bin/true"), "unrelated line lost:\n{out}");
    }

    #[test]
    fn set_enabled_true_resumes_a_paused_job() {
        let paused = COMMENTED.replace("enabled: true", "enabled: false");
        let out = set_enabled(&paused, "cleanup", true).expect("edit should succeed");
        assert!(out.contains("enabled: true"), "should resume:\n{out}");
        assert!(!out.contains("enabled: false"));
    }

    #[test]
    fn set_enabled_errors_on_unknown_job() {
        let err = set_enabled(COMMENTED, "nope", false).unwrap_err();
        assert!(err.to_string().contains("nope"), "error should name the job: {err}");
    }

    #[test]
    fn set_enabled_inserts_field_when_absent_targeting_only_the_named_job() {
        // `cleanup` relies on the default (no `enabled:` line); pausing must insert it.
        let out = set_enabled(TWO_JOBS_NO_ENABLED, "cleanup", false).expect("edit should succeed");
        assert!(out.contains("enabled: false"), "field should be inserted:\n{out}");
        // The insertion must land inside `cleanup`, not touch `backup`.
        assert!(out.contains("enabled: true"), "backup's field must be untouched:\n{out}");
        // Round-trips: the result still parses and reconciles to the intended state.
        let cleanup_block = out.split("- id: backup").next().unwrap();
        assert!(cleanup_block.contains("enabled: false"), "insertion landed in the wrong job:\n{out}");
        assert!(out.contains("# nightly sweep"), "comment preserved:\n{out}");
    }
}
