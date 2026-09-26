//! Compiling user-supplied regular expressions with a bounded compile size.
//!
//! Patterns come from user data: `mf_ignore` field values
//! ([`crate::eligibility`]), the `Matches` query operator
//! (the in-memory scans of [`crate::index`]) and the SQLite `REGEXP` UDF
//! ([`crate::db`]). The
//! `regex` crate already guarantees linear-match time (no catastrophic
//! backtracking), but its default compile-size budget is large (10 MiB); a
//! pathological pattern could still consume a lot of memory at compile time,
//! and that cost is paid repeatedly (e.g. once per reconcile walk). Cap the
//! compiled size so a hostile pattern is rejected instead.

/// Maximum size of the compiled program and of the lazy-DFA cache, per
/// pattern. Comfortably above any realistic ignore/query pattern, far below a
/// memory-exhaustion payload.
const SIZE_LIMIT: usize = 1 << 20; // 1 MiB

/// Compiles `pattern` with a bounded compile-size budget. Returns the same
/// `regex::Error` as `Regex::new` on an invalid or too-large pattern.
pub fn compile(pattern: &str) -> Result<regex::Regex, regex::Error> {
    regex::RegexBuilder::new(pattern).size_limit(SIZE_LIMIT).dfa_size_limit(SIZE_LIMIT).build()
}

/// Substrings every text matching `pattern` contains, lower-cased — what a
/// trigram index can look up to narrow a text search to its candidates
/// (spec-storage "Text: trigrams"). Conservative: what it cannot prove
/// required it leaves out, so an empty answer means "scan". A case-insensitive
/// letter (a class of one letter's case variants) counts as that letter, the
/// index being lower-cased too.
pub fn required_literals(pattern: &str) -> Vec<String> {
    use regex_syntax::hir::{Class, Hir, HirKind};

    /// The one character a class stands for when it holds only the case
    /// variants of that character.
    fn one_char(class: &Class) -> Option<char> {
        let chars: Vec<char> = match class {
            Class::Unicode(c) => {
                let mut out = Vec::new();
                for r in c.ranges() {
                    for ch in r.start()..=r.end() {
                        out.push(ch);
                        if out.len() > 4 {
                            return None;
                        }
                    }
                }
                out
            }
            Class::Bytes(c) => {
                let mut out = Vec::new();
                for r in c.ranges() {
                    for b in r.start()..=r.end() {
                        out.push(char::from(b));
                        if out.len() > 4 {
                            return None;
                        }
                    }
                }
                out
            }
        };
        let lower: Vec<String> = chars.iter().map(|c| c.to_lowercase().collect()).collect();
        let first = lower.first()?;
        (lower.iter().all(|l| l == first) && first.chars().count() == 1)
            .then(|| first.chars().next().expect("one char"))
    }

    /// Adds `hir`'s required literals to `out`, extending `run` (the literal
    /// being built across a concatenation) where it can.
    fn walk(hir: &Hir, run: &mut String, out: &mut Vec<String>) {
        let flush = |run: &mut String, out: &mut Vec<String>| {
            if !run.is_empty() {
                out.push(std::mem::take(run));
            }
        };
        match hir.kind() {
            HirKind::Literal(lit) => match std::str::from_utf8(&lit.0) {
                Ok(text) => run.push_str(&text.to_lowercase()),
                Err(_) => flush(run, out),
            },
            HirKind::Class(class) => match one_char(class) {
                Some(c) => run.extend(c.to_lowercase()),
                None => flush(run, out),
            },
            HirKind::Concat(items) => {
                for item in items {
                    walk(item, run, out);
                }
            }
            HirKind::Capture(cap) => walk(&cap.sub, run, out),
            HirKind::Repetition(rep) if rep.min >= 1 => {
                // Occurs once at least, but what follows need not touch it.
                flush(run, out);
                walk(&rep.sub, run, out);
                flush(run, out);
            }
            HirKind::Look(_) | HirKind::Empty => {}
            HirKind::Repetition(_) | HirKind::Alternation(_) => flush(run, out),
        }
    }

    let Ok(hir) = regex_syntax::parse(pattern) else { return Vec::new() };
    let (mut run, mut out) = (String::new(), Vec::new());
    walk(&hir, &mut run, &mut out);
    if !run.is_empty() {
        out.push(run);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_literals_are_what_every_match_contains() {
        let lits = |p: &str| required_literals(p);
        assert_eq!(lits("file000123"), ["file000123"]);
        assert_eq!(lits("(?i)000123.*TXT"), ["000123", "txt"], "case folded, split at .*");
        assert_eq!(lits("(?i)Hello"), ["hello"], "a case-insensitive letter is itself");
        assert_eq!(lits("^f[0-4]x"), ["f", "x"], "a class of several letters splits");
        assert_eq!(lits("(abc)+x"), ["abc", "x"], "a repeated group occurs once at least");
        assert_eq!(lits("(abc)*x"), ["x"], "an optional group guarantees nothing");
        assert!(lits("abc|abd").is_empty(), "an alternation guarantees no one literal");
        assert!(lits(".*").is_empty());
    }

    #[test]
    fn compiles_ordinary_patterns() {
        assert!(compile(r"\.metafolder(/.*)?$").is_ok());
        assert!(compile(r"^[a-z0-9_]+\.(mp4|mkv)$").is_ok());
    }

    #[test]
    fn rejects_oversized_patterns() {
        // A huge counted repetition expands past the 1 MiB compile budget and
        // is rejected, rather than allocating unbounded memory.
        assert!(compile(&format!("a{{{}}}", 5_000_000)).is_err());
    }

    #[test]
    fn still_rejects_syntactically_invalid_patterns() {
        assert!(compile("(unclosed").is_err());
    }
}
