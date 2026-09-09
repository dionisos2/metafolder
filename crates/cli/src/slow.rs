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
        lines.push(format!("    {key:<10}{value}"));
    }
    for phase in &entry.phases {
        lines.push(render_phase(phase));
    }
    lines
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
    format!("{indent}{name:<24}{:>8}", format_ms(phase.ms))
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
            Phase { name: "wait:conn".into(), ms: 3100, count: 1, depth: 0 },
            Phase { name: "resolve.uuids".into(), ms: 1650, count: 1, depth: 0 },
            Phase { name: "validate.schema".into(), ms: 900, count: 12, depth: 1 },
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
    fn test_durations_are_read_at_a_glance() {
        assert_eq!(format_ms(840), "840ms");
        assert_eq!(format_ms(4820), "4.82s");
        assert_eq!(format_ms(64_000), "1m04s");
    }
}
