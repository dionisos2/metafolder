//! `mf slow`: reading the repository's slow-operation log (spec-slow-log.org).
//!
//! The rendering lives here, apart from the command plumbing, because it is the
//! whole point of the command: an entry is a total *and* a breakdown, and a
//! reader must be able to see at a glance which phase ate the seconds.

use metafolder_core::slowlog::{Entry, Phase};

/// One entry as printed lines: a header, its context, then its phases.
pub fn render(entry: &Entry) -> Vec<String> {
    let mut lines = vec![format!(
        "{}  {:>7}  {:<6}  {}",
        timestamp(entry.at_ms),
        format_ms(entry.ms),
        entry.source,
        entry.op
    )];
    if let Some(id) = &entry.op_id {
        lines.push(format!("    {:<10}{id}", "op-id"));
    }
    for (key, value) in &entry.context {
        lines.push(format!("    {key:<10}{}", readable(value)));
    }
    if entry.reads > 0 {
        lines.push(format!("    {:<10}{}", "keys", format_count(entry.reads)));
    }
    for phase in &entry.phases {
        lines.push(render_phase(phase));
    }
    lines
}

/// A context value as a person reads it: whole up to a line's worth, cut
/// beyond. A query is logged whole so that it can be replayed, and printed
/// whole it would be the only thing on the screen; `--json` keeps it all.
fn readable(value: &str) -> String {
    const SHOWN: usize = metafolder_core::slowlog::MAX_CONTEXT_CHARS;
    if value.chars().count() <= SHOWN {
        return value.to_string();
    }
    let mut cut: String = value.chars().take(SHOWN).collect();
    cut.push('…');
    cut
}

/// A phase line: indented by its depth, so a phase nested in another reads as
/// part of it rather than as a second cost.
fn render_phase(phase: &Phase) -> String {
    let indent = " ".repeat(4 + 2 * phase.depth as usize);
    let name = if phase.count > 1 {
        format!("{} ×{}", phase.name, phase.count)
    } else {
        phase.name.clone()
    };
    let line = format!("{indent}{name:<24}{:>8}", format_ms(phase.ms));
    if phase.reads == 0 {
        return line;
    }
    format!("{line}{:>14} keys", format_count(phase.reads))
}

/// `210 008` — thousands apart, so a count of keys reads at a glance too.
fn format_count(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(' ');
        }
        out.push(c);
    }
    out
}

/// The `/query/profile` body that runs a logged query again: its query, and
/// the sort, limit and count it was evaluated with — the ones that decide how
/// (spec-slow-log "Replaying a query"). An error names why an entry cannot be
/// replayed: not a query, or a query cut short.
pub fn replay_body(entry: &Entry) -> Result<serde_json::Value, String> {
    let context = |key: &str| entry.context.iter().find(|(k, _)| k == key).map(|(_, v)| v);
    let Some(text) = context("query") else {
        return Err(format!("the entry of '{}' records no query to replay", entry.op));
    };
    let query: serde_json::Value = serde_json::from_str(text).map_err(|_| {
        "the logged query was not kept whole (too long, or logged before queries were), \
         so it cannot be replayed"
            .to_string()
    })?;
    let mut body = serde_json::json!({ "query": query });
    if let Some(sort) = context("sort").and_then(|s| serde_json::from_str(s).ok()) {
        body["sort"] = sort;
    }
    if let Some(limit) = context("limit").and_then(|l| l.parse::<u64>().ok()) {
        body["limit"] = limit.into();
    }
    if context("count").is_some_and(|c| c == "true") {
        body["count"] = true.into();
    }
    Ok(body)
}

/// `2026-09-09 14:03:22` — the ISO form without the `T` and the `Z`, which is
/// what a person reads a log with.
fn timestamp(at_ms: i64) -> String {
    metafolder_core::date::iso8601_from_ms(at_ms)
        .replace('T', " ")
        .trim_end_matches('Z')
        .to_string()
}

/// `840ms`, `4.82s`, `1m04s` — three ranges, because a log that says "4820ms"
/// makes the reader do the division on every line.
pub fn format_ms(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.2}s", ms as f64 / 1000.0)
    } else {
        format!("{}m{:02}s", ms / 60_000, (ms % 60_000) / 1000)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> Entry {
        let mut e = Entry::new("daemon", "POST /repos/:repo/query", 1_757_426_602_000, 4820);
        e.note("client", "mfr_path ->* \"/2024\"");
        e.phases = vec![
            Phase { name: "wait:conn".into(), ms: 3100, count: 1, depth: 0, reads: 0 },
            Phase { name: "resolve.uuids".into(), ms: 1650, count: 1, depth: 0, reads: 210_008 },
            Phase { name: "validate.schema".into(), ms: 900, count: 12, depth: 1, reads: 0 },
        ];
        e
    }

    #[test]
    fn test_the_header_leads_with_when_how_long_and_what() {
        let header = &render(&entry())[0];
        assert!(header.starts_with("2025-09-09 "), "{header}");
        assert!(header.contains("4.82s"), "the cost is what the reader scans for: {header}");
        assert!(header.contains("daemon"), "{header}");
        assert!(header.ends_with("POST /repos/:repo/query"), "{header}");
    }

    #[test]
    fn test_the_context_is_printed_before_the_phases() {
        let lines = render(&entry());
        let context = lines.iter().position(|l| l.contains("client")).unwrap();
        let first_phase = lines.iter().position(|l| l.contains("wait:conn")).unwrap();
        assert!(context < first_phase, "{lines:#?}");
    }

    #[test]
    fn test_a_nested_phase_is_indented_under_its_parent() {
        let lines = render(&entry());
        let parent = lines.iter().find(|l| l.contains("resolve.uuids")).unwrap();
        let child = lines.iter().find(|l| l.contains("validate.schema")).unwrap();
        let indent = |l: &str| l.len() - l.trim_start().len();
        assert!(indent(child) > indent(parent), "{child:?} under {parent:?}");
    }

    #[test]
    fn test_a_repeated_phase_says_how_many_times_it_ran() {
        // 12 validations of 75 ms and one of 900 ms are different problems.
        let line = render(&entry()).into_iter().find(|l| l.contains("validate.schema")).unwrap();
        assert!(line.contains("×12"), "{line}");
    }

    #[test]
    fn test_the_keys_read_are_printed_where_they_were_read() {
        // What tells a phase that read a lot from one that waited.
        let mut e = entry();
        e.reads = 210_008;
        let lines = render(&e);
        let total = lines.iter().find(|l| l.trim_start().starts_with("keys")).unwrap();
        assert!(total.contains("210 008"), "{total}");
        let phase = lines.iter().find(|l| l.contains("resolve.uuids")).unwrap();
        assert!(phase.ends_with("210 008 keys"), "{phase}");
        let wait = lines.iter().find(|l| l.contains("wait:conn")).unwrap();
        assert!(!wait.contains("keys"), "a phase that read nothing says nothing: {wait}");
    }

    #[test]
    fn test_an_entry_without_reads_prints_no_keys_line() {
        // The GUI's entries and the ones written before reads were counted.
        let mut e = entry();
        e.phases.iter_mut().for_each(|p| p.reads = 0);
        let lines = render(&e);
        assert!(!lines.iter().any(|l| l.contains("keys")), "{lines:#?}");
    }

    #[test]
    fn test_a_logged_query_replays_with_its_sort_limit_and_count() {
        let mut e = Entry::new("daemon", "POST /repos/:repo/query", 0, 4820);
        e.note("query", r#"{"type":"is_present","field":"rating"}"#);
        e.note("sort", r#"[{"field":"rating","order":"desc"}]"#);
        e.note("limit", "50");
        e.note("count", "true");
        let body = replay_body(&e).unwrap();
        assert_eq!(body["query"]["field"], "rating");
        assert_eq!(body["sort"][0]["order"], "desc");
        assert_eq!(body["limit"], 50);
        assert_eq!(body["count"], true);
    }

    #[test]
    fn test_a_query_logged_without_limit_or_count_replays_without_them() {
        let mut e = Entry::new("daemon", "POST /repos/:repo/query", 0, 4820);
        e.note("query", r#"{"type":"is_present","field":"rating"}"#);
        let body = replay_body(&e).unwrap();
        assert!(body.get("limit").is_none() && body.get("count").is_none(), "{body}");
    }

    #[test]
    fn test_a_query_cut_short_cannot_be_replayed() {
        // An entry from before queries were kept whole, or a giant one.
        let mut e = Entry::new("daemon", "POST /repos/:repo/query", 0, 4820);
        e.note("query", r#"{"type":"or","operands":[{"type":"eq","fie"#);
        let err = replay_body(&e).unwrap_err();
        assert!(err.contains("whole"), "{err}");
        let err = replay_body(&Entry::new("daemon", "watcher.flush", 0, 1)).unwrap_err();
        assert!(err.contains("query"), "{err}");
    }

    #[test]
    fn test_a_long_context_value_is_cut_for_reading() {
        // A query is kept whole to be replayed; printed whole, it would be
        // the only thing on the screen. `--json` has it all.
        let mut e = entry();
        e.note("query", "x".repeat(5_000));
        let line = render(&e).into_iter().find(|l| l.trim_start().starts_with("query")).unwrap();
        assert!(line.chars().count() < 300, "{} chars", line.chars().count());
        assert!(line.ends_with('…'), "{line}");
    }

    #[test]
    fn test_durations_are_read_at_a_glance() {
        assert_eq!(format_ms(840), "840ms");
        assert_eq!(format_ms(4820), "4.82s");
        assert_eq!(format_ms(64_000), "1m04s");
    }
}
