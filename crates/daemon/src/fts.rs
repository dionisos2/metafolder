//! Trigram FTS pre-filter for `MATCHES` (spec-query "MATCHES via FTS5").
//!
//! `MATCHES` runs a regex; the `field_text` FTS5 trigram index lets the SQL
//! engine pre-filter to the rows that contain a literal substring *every* match
//! must include, then apply `REGEXP` only to those survivors. The pre-filter is
//! a pure optimisation: it can only over-select (a sound over-approximation of
//! the regex), and `REGEXP` re-checks every candidate, so results are identical
//! to the full scan.
//!
//! The soundness obligation is therefore on the literal: the returned string
//! must be contained in every string the pattern matches. We extract it
//! conservatively from the parsed HIR — only contiguous *mandatory* literal runs
//! count, and anything that could make the literal optional or non-contiguous
//! (alternation, a `min == 0` repetition, a character class, `.`) breaks the
//! run. When in doubt we return `None`, and the caller falls back to the plain
//! `REGEXP` scan. The whole thing is cross-checked against the scan by
//! `tests/fts_oracle.rs`.

use regex_syntax::hir::{Hir, HirKind};

/// The SQLite trigram tokenizer needs ≥ 3 characters to form a trigram, so a
/// shorter literal cannot be used as a `MATCH` phrase.
const MIN_TRIGRAM_CHARS: usize = 3;

/// A literal substring (≥ 3 chars) that every match of `pattern` must contain,
/// usable as a trigram `MATCH` pre-filter, or `None` if none can be soundly
/// extracted (the caller then scans with `REGEXP` alone).
pub fn required_fts_literal(pattern: &str) -> Option<String> {
    let hir = regex_syntax::parse(pattern).ok()?;
    let mut runs: Vec<Vec<u8>> = Vec::new();
    let mut current: Vec<u8> = Vec::new();
    collect_runs(&hir, &mut runs, &mut current);
    flush(&mut runs, &mut current);
    // The longest valid-UTF-8 run with at least one trigram wins (longer ⇒ more
    // selective). Invalid UTF-8 runs cannot be a phrase, so they are dropped.
    runs.into_iter()
        .filter_map(|r| String::from_utf8(r).ok())
        .filter(|s| s.chars().count() >= MIN_TRIGRAM_CHARS)
        .max_by_key(|s| s.chars().count())
}

/// Wraps a literal as a quoted FTS5 phrase (internal `"` doubled). Bind this as
/// a parameter to `text MATCH ?` — never interpolate it into the SQL.
pub fn match_phrase(literal: &str) -> String {
    format!("\"{}\"", literal.replace('"', "\"\""))
}

/// Appends the bytes of contiguous mandatory literals into `current`, pushing
/// and resetting it at every boundary that breaks the mandatory literal text.
fn collect_runs(hir: &Hir, runs: &mut Vec<Vec<u8>>, current: &mut Vec<u8>) {
    match hir.kind() {
        // A literal contributes its bytes to the current contiguous run.
        HirKind::Literal(lit) => current.extend_from_slice(&lit.0),
        // A concatenation glues adjacent literal runs together.
        HirKind::Concat(subs) => subs.iter().for_each(|s| collect_runs(s, runs, current)),
        // A capturing group is transparent to matching.
        HirKind::Capture(cap) => collect_runs(&cap.sub, runs, current),
        // Zero-width assertions (anchors, word boundaries) and the empty regex
        // neither contribute text nor break a run.
        HirKind::Look(_) | HirKind::Empty => {}
        // A repetition breaks the run; if the sub is mandatory (min ≥ 1) its own
        // mandatory literals are captured standalone (not glued to the outside,
        // which would be unsound).
        HirKind::Repetition(rep) => {
            flush(runs, current);
            if rep.min >= 1 {
                collect_runs(&rep.sub, runs, current);
                flush(runs, current);
            }
        }
        // A class (incl. `.`) matches one of several chars — not a fixed literal;
        // an alternation has no literal common to every branch. Both break.
        HirKind::Class(_) | HirKind::Alternation(_) => flush(runs, current),
    }
}

fn flush(runs: &mut Vec<Vec<u8>>, current: &mut Vec<u8>) {
    if !current.is_empty() {
        runs.push(std::mem::take(current));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_literal_is_extracted() {
        assert_eq!(required_fts_literal("report").as_deref(), Some("report"));
        assert_eq!(required_fts_literal("invoice2024").as_deref(), Some("invoice2024"));
    }

    #[test]
    fn longest_mandatory_run_wins() {
        // Both "foo" and "bar" are required; either is sound, the longest (here
        // a tie ⇒ one of them) is returned.
        let lit = required_fts_literal("foo.*bar").unwrap();
        assert!(lit == "foo" || lit == "bar", "got {lit}");
        // A clearly-longer mandatory tail is preferred.
        assert_eq!(required_fts_literal("[0-9]+/report").as_deref(), Some("/report"));
        assert_eq!(required_fts_literal("wiki/[0-9]+/annual_report").as_deref(),
                   Some("/annual_report"));
    }

    #[test]
    fn optional_and_anchored_segments_handled() {
        // `(bc)?` is optional and must not glue "a".."def"; "def" is still required.
        assert_eq!(required_fts_literal("a(bc)?def").as_deref(), Some("def"));
        // Anchors are zero-width and do not break a run.
        assert_eq!(required_fts_literal("^report$").as_deref(), Some("report"));
        // A literal tail after an alternation prefix.
        assert_eq!(required_fts_literal("(report|invoice)_2024").as_deref(), Some("_2024"));
    }

    #[test]
    fn unusable_patterns_return_none() {
        // No literal ≥ 3 chars that every match must contain → fall back to scan.
        assert_eq!(required_fts_literal("ab"), None);
        assert_eq!(required_fts_literal("a.c"), None);
        assert_eq!(required_fts_literal("^.*$"), None);
        assert_eq!(required_fts_literal("cat|dog"), None); // no common literal
        assert_eq!(required_fts_literal("(abc|xyz)"), None);
        // Case-insensitive folds literals into classes → conservatively None.
        assert_eq!(required_fts_literal("(?i)report"), None);
        // Invalid regex → None (the caller compiles separately and reports it).
        assert_eq!(required_fts_literal("(unclosed"), None);
    }

    #[test]
    fn match_phrase_escapes_quotes() {
        assert_eq!(match_phrase("report"), "\"report\"");
        assert_eq!(match_phrase(r#"a"b"#), "\"a\"\"b\"");
    }
}
