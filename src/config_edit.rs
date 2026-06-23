//! Surgical, format-preserving edits to the desired-state YAML (ADR 0009).
//!
//! Every mutation operates on the *source text* — locating the affected span via
//! the span-aware `granit-parser` event stream and splicing bytes — never by
//! re-serializing the in-memory model. The user's comments, key ordering, and
//! formatting survive every edit. Cases the surgical writer cannot edit safely
//! (flow-style `jobs`, anchors/merge keys spanning the target, unparseable input)
//! are refused with an error pointing at `jobs edit`, never silently rewritten.
//!
//! The public entry points are consumed by the `jobs` mutation commands
//! (PDC-82–85); until those wire up, the module is exercised only by its tests.
#![allow(dead_code)]

use std::io::Write as _;
use std::ops::Range;
use std::path::Path;

use granit_parser::{Event, Parser, Span, StructureStyle};

/// Toggle a job's `enabled:` field in place, preserving all surrounding text.
///
/// Returns the edited YAML source. Errors if the job id is not found or the
/// surrounding YAML cannot be edited safely.
pub(crate) fn set_enabled(source: &str, job_id: &str, enabled: bool) -> anyhow::Result<String> {
    let events = collect_events(source)?;
    let item = locate_job(&events, job_id)?;
    let (range, text) = plan_set_enabled(source, &events, item, enabled)
        .ok_or_else(|| anyhow::anyhow!("cannot edit job '{job_id}' safely; use `jobs edit`"))?;
    Ok(splice(source, range, &text))
}

/// Splice `text` over the byte `range` of `source` (an empty range inserts).
fn splice(source: &str, range: Range<usize>, text: &str) -> String {
    let mut out = String::with_capacity(source.len() + text.len());
    out.push_str(&source[..range.start]);
    out.push_str(text);
    out.push_str(&source[range.end..]);
    out
}

/// Locate a job's mapping by id, refusing (rather than corrupting) any layout the
/// surgical writer can't edit safely — flow-style `jobs` or a flow-style job.
fn locate_job(events: &[(Event, Span)], job_id: &str) -> anyhow::Result<usize> {
    let seq = jobs_seq_start(events)
        .ok_or_else(|| anyhow::anyhow!("config has no block-style `jobs:` sequence"))?;
    if matches!(
        events[seq].0,
        Event::SequenceStart(StructureStyle::Flow, ..)
    ) {
        anyhow::bail!("`jobs` uses flow style; edit it with `jobs edit`");
    }
    let item =
        find_job_item(events, job_id).ok_or_else(|| anyhow::anyhow!("job '{job_id}' not found"))?;
    if matches!(
        events[item].0,
        Event::MappingStart(StructureStyle::Flow, ..)
    ) {
        anyhow::bail!("job '{job_id}' uses flow style; edit it with `jobs edit`");
    }
    Ok(item)
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
    while !matches!(
        events.get(i).map(|(e, _)| e),
        Some(Event::MappingEnd) | None
    ) {
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

/// Event index of the `jobs:` block sequence at the document root.
fn jobs_seq_start(events: &[(Event, Span)]) -> Option<usize> {
    let root = events
        .iter()
        .position(|(e, _)| matches!(e, Event::MappingStart(..)))?;
    map_entries(events, root).into_iter().find_map(|(k, v)| {
        (scalar_at(events, k) == Some("jobs")
            && matches!(
                events.get(v).map(|(e, _)| e),
                Some(Event::SequenceStart(..))
            ))
        .then_some(v)
    })
}

/// Event index of the job mapping (the `MappingStart`) whose `id` is `job_id`.
fn find_job_item(events: &[(Event, Span)], job_id: &str) -> Option<usize> {
    let seq = jobs_seq_start(events)?;
    let mut i = skip_comments(events, seq + 1);
    while !matches!(
        events.get(i).map(|(e, _)| e),
        Some(Event::SequenceEnd) | None
    ) {
        let item = i;
        if matches!(events[item].0, Event::MappingStart(..))
            && map_entries(events, item).iter().any(|&(k, v)| {
                scalar_at(events, k) == Some("id") && scalar_at(events, v) == Some(job_id)
            })
        {
            return Some(item);
        }
        i = skip_comments(events, end_of_node(events, item));
    }
    None
}

/// Byte range to splice and the replacement text to toggle/insert a job's
/// `enabled:` field. A non-empty range replaces an existing value; an empty
/// range inserts a new `enabled:` line indented to match the job's other keys.
fn plan_set_enabled(
    source: &str,
    events: &[(Event, Span)],
    item: usize,
    enabled: bool,
) -> Option<(Range<usize>, String)> {
    let literal = if enabled { "true" } else { "false" };
    let entries = map_entries(events, item);

    // Existing `enabled:` → replace its value span.
    if let Some(range) = entries.iter().find_map(|&(k, v)| {
        (scalar_at(events, k) == Some("enabled")).then(|| events[v].1.byte_range())?
    }) {
        return Some((range, literal.to_owned()));
    }

    // Absent → insert a new line after the first entry, indented to match a clean
    // sibling key (the first key shares the `- ` dash line, so its line indent is off).
    let (first_k, first_v) = *entries.first()?;
    let indent_key = entries.get(1).map_or(first_k, |&(k, _)| k);
    let indent_byte = events[indent_key].1.byte_range().map_or(0, |r| r.start);
    let indent = line_indent_of(source, indent_byte);
    let after_first = events[end_of_node(events, first_v) - 1].1.byte_range()?.end;
    let at = source[after_first..]
        .find('\n')
        .map_or(source.len(), |n| after_first + n);
    Some((at..at, format!("\n{indent}enabled: {literal}")))
}

/// Remove a job's entire block from the desired-state YAML, preserving siblings.
pub(crate) fn remove_job(source: &str, job_id: &str) -> anyhow::Result<String> {
    let events = collect_events(source)?;
    let item = locate_job(&events, job_id)?;
    let range = job_item_line_span(source, &events, item)
        .ok_or_else(|| anyhow::anyhow!("cannot locate the block for job '{job_id}'"))?;
    Ok(splice(source, range.clone(), ""))
}

/// Append a pre-formatted job block as the last item of the `jobs:` sequence.
///
/// `block` is one block-style list item (e.g. `  - id: foo\n    schedule: …`);
/// generating it is the caller's job. The existing file is untouched but for the
/// inserted lines.
pub(crate) fn append_job(source: &str, block: &str) -> anyhow::Result<String> {
    let events = collect_events(source)?;
    let seq = jobs_seq_start(&events)
        .ok_or_else(|| anyhow::anyhow!("config has no block-style `jobs:` sequence"))?;
    if matches!(
        events[seq].0,
        Event::SequenceStart(StructureStyle::Flow, ..)
    ) {
        anyhow::bail!("`jobs` uses flow style; edit it with `jobs edit`");
    }
    let last = last_job_item(&events, seq)
        .ok_or_else(|| anyhow::anyhow!("`jobs` is empty; add the first job with `jobs edit`"))?;
    let at = job_item_line_span(source, &events, last)
        .ok_or_else(|| anyhow::anyhow!("cannot locate the end of the `jobs:` sequence"))?
        .end;

    let mut insert = String::new();
    if at > 0 && !source[..at].ends_with('\n') {
        insert.push('\n');
    }
    insert.push_str(block.trim_end_matches('\n'));
    insert.push('\n');
    Ok(splice(source, at..at, &insert))
}

/// Atomically write `contents` to `path`: temp file in the same dir, fsync, rename.
pub(crate) fn atomic_write(path: &Path, contents: &str) -> anyhow::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("config path has no parent directory"))?;
    let mut tmp = tempfile::NamedTempFile::new_in(dir)?;
    tmp.write_all(contents.as_bytes())?;
    tmp.as_file().sync_all()?;
    tmp.persist(path)
        .map_err(|e| anyhow::anyhow!("failed to replace {}: {}", path.display(), e.error))?;
    Ok(())
}

/// Event index of the last job mapping in the `jobs:` sequence at `seq`.
fn last_job_item(events: &[(Event, Span)], seq: usize) -> Option<usize> {
    let mut i = skip_comments(events, seq + 1);
    let mut last = None;
    while !matches!(
        events.get(i).map(|(e, _)| e),
        Some(Event::SequenceEnd) | None
    ) {
        if matches!(events[i].0, Event::MappingStart(..)) {
            last = Some(i);
        }
        i = skip_comments(events, end_of_node(events, i));
    }
    last
}

/// Byte span of a job list item including its `- ` line prefix and trailing
/// newline, so removing the range leaves clean lines around the gap.
fn job_item_line_span(source: &str, events: &[(Event, Span)], item: usize) -> Option<Range<usize>> {
    let content_start = events[item].1.byte_range()?.start;
    let line_start = source[..content_start].rfind('\n').map_or(0, |n| n + 1);

    // End of removal. A block scalar's span can over-reach past its trailing newline
    // into the *next* line's indentation, so we don't trust this job's own span end
    // for non-last jobs: cut at the next sibling item's line start, independent of
    // any span quirk. The last job has no next sibling, so trim its (possibly
    // over-reaching) content end back to its final content line.
    let after_node = end_of_node(events, item);
    let next = skip_comments(events, after_node);
    let end = if let Some((Event::MappingStart(..), span)) = events.get(next) {
        let next_start = span.byte_range()?.start;
        source[..next_start].rfind('\n').map_or(0, |n| n + 1)
    } else {
        let content_end = (item..after_node)
            .filter(|&j| !matches!(events[j].0, Event::MappingEnd | Event::SequenceEnd))
            .filter_map(|j| events[j].1.byte_range())
            .map(|r| r.end)
            .max()?;
        let trimmed = source[..content_end].trim_end_matches([' ', '\t']);
        if trimmed.ends_with('\n') {
            trimmed.len()
        } else {
            source[content_end..]
                .find('\n')
                .map_or(source.len(), |n| content_end + n + 1)
        }
    };
    Some(line_start..end)
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

        assert!(
            out.contains("enabled: false"),
            "field should be toggled:\n{out}"
        );
        assert!(
            !out.contains("enabled: true"),
            "old value should be gone:\n{out}"
        );
        // Comments and untouched lines survive verbatim (the whole point of ADR 0009).
        assert!(
            out.contains("# keep the box tidy"),
            "standalone comment lost:\n{out}"
        );
        assert!(
            out.contains("# nightly sweep"),
            "inline comment lost:\n{out}"
        );
        assert!(
            out.contains("command: /usr/bin/true"),
            "unrelated line lost:\n{out}"
        );
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
        assert!(
            err.to_string().contains("nope"),
            "error should name the job: {err}"
        );
    }

    #[test]
    fn set_enabled_inserts_field_when_absent_targeting_only_the_named_job() {
        // `cleanup` relies on the default (no `enabled:` line); pausing must insert it.
        let out = set_enabled(TWO_JOBS_NO_ENABLED, "cleanup", false).expect("edit should succeed");
        assert!(
            out.contains("enabled: false"),
            "field should be inserted:\n{out}"
        );
        // The insertion must land inside `cleanup`, not touch `backup`.
        assert!(
            out.contains("enabled: true"),
            "backup's field must be untouched:\n{out}"
        );
        // Round-trips: the result still parses and reconciles to the intended state.
        let cleanup_block = out.split("- id: backup").next().unwrap();
        assert!(
            cleanup_block.contains("enabled: false"),
            "insertion landed in the wrong job:\n{out}"
        );
        assert!(out.contains("# nightly sweep"), "comment preserved:\n{out}");
    }

    #[test]
    fn remove_job_excises_block_and_keeps_siblings() {
        let out = remove_job(TWO_JOBS_NO_ENABLED, "cleanup").expect("remove should succeed");
        assert!(
            !out.contains("id: cleanup"),
            "removed job should be gone:\n{out}"
        );
        assert!(
            !out.contains("/usr/bin/true"),
            "removed job's body should be gone:\n{out}"
        );
        assert!(out.contains("id: backup"), "sibling kept:\n{out}");
        assert!(
            out.contains("command: /usr/bin/backup"),
            "sibling body kept:\n{out}"
        );
        assert!(out.contains("jobs:"), "jobs header kept:\n{out}");
    }

    #[test]
    fn remove_job_errors_on_unknown() {
        let err = remove_job(TWO_JOBS_NO_ENABLED, "nope").unwrap_err();
        assert!(
            err.to_string().contains("nope"),
            "error should name the job: {err}"
        );
    }

    #[test]
    fn remove_job_with_trailing_block_scalar_keeps_next_sibling() {
        // A block scalar (`|`) as the job's last field reports its end at column 0
        // of the next line; the line-extension must not swallow the next sibling.
        let src = "\
version: 1
jobs:
  - id: tricky
    schedule:
      every: 1h
    execution:
      command: |
        echo hello
        echo done
  - id: keepme
    schedule:
      every: 6h
    execution:
      command: /bin/true
";
        let out = remove_job(src, "tricky").expect("remove should succeed");
        assert!(!out.contains("id: tricky"), "tricky should be gone:\n{out}");
        assert!(
            out.contains("- id: keepme"),
            "keepme header must survive:\n{out}"
        );
        assert!(
            out.contains("command: /bin/true"),
            "keepme body must survive:\n{out}"
        );
    }

    #[test]
    fn remove_last_job_with_block_scalar_keeps_prior_sibling() {
        let src = "\
version: 1
jobs:
  - id: keep
    schedule:
      every: 1h
    execution:
      command: /bin/true
  - id: last
    schedule:
      every: 6h
    execution:
      command: |
        echo hi
        echo bye
";
        let out = remove_job(src, "last").expect("remove should succeed");
        assert!(!out.contains("id: last"), "last job gone:\n{out}");
        assert!(out.contains("- id: keep"), "prior sibling kept:\n{out}");
        assert!(
            out.contains("command: /bin/true"),
            "prior body kept:\n{out}"
        );
    }

    #[test]
    fn append_job_adds_block_after_last_job_preserving_existing() {
        let block = "  - id: newjob\n    schedule:\n      every: 1h\n    execution:\n      command: /bin/foo";
        let out = append_job(TWO_JOBS_NO_ENABLED, block).expect("append should succeed");
        assert!(
            out.contains("id: cleanup") && out.contains("id: backup"),
            "existing kept:\n{out}"
        );
        assert!(out.contains("id: newjob"), "new job added:\n{out}");
        assert!(
            out.find("id: newjob").unwrap() > out.find("id: backup").unwrap(),
            "new job should come last:\n{out}"
        );
        assert!(out.contains("# nightly sweep"), "comment preserved:\n{out}");
        assert!(out.ends_with('\n'), "trailing newline preserved:\n{out:?}");
    }

    #[test]
    fn surgical_edits_refuse_flow_style_jobs() {
        let flow =
            "version: 1\njobs: [{id: a, schedule: {every: 1h}, execution: {command: /bin/x}}]\n";
        let pause = set_enabled(flow, "a", false).unwrap_err();
        assert!(
            pause.to_string().contains("flow"),
            "should refuse flow style: {pause}"
        );
        let append = append_job(flow, "  - id: b").unwrap_err();
        assert!(
            append.to_string().contains("flow"),
            "append should refuse flow style: {append}"
        );
    }

    #[test]
    fn surgical_edits_error_on_unparseable_yaml() {
        let garbage = "version: 1\njobs: [1, 2";
        assert!(
            set_enabled(garbage, "a", false).is_err(),
            "must not edit unparseable YAML"
        );
        assert!(remove_job(garbage, "a").is_err());
    }

    #[test]
    fn atomic_write_replaces_existing_contents() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("periodic.config.yaml");
        std::fs::write(&path, "old\n").unwrap();
        atomic_write(&path, "new contents\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new contents\n");
    }
}
