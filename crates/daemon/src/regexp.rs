//! Compiling user-supplied regular expressions with a bounded compile size.
//!
//! Patterns come from user data: `mf_ignore` field values
//! ([`crate::eligibility`]), the `Matches` query operator
//! ([`crate::query_exec`]) and the SQLite `REGEXP` UDF ([`crate::db`]). The
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
    regex::RegexBuilder::new(pattern)
        .size_limit(SIZE_LIMIT)
        .dfa_size_limit(SIZE_LIMIT)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

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
