//! Loading of the global simplified-query grammar (spec-query "Simplified
//! query language" / "Configuration"): `$XDG_CONFIG_HOME/metafolder/daemon/
//! query-grammar`, installed by `metafolder-sync-config` (spec-config) and
//! parsed/validated at startup. A missing or malformed grammar is a hard
//! error; there is no embedded fallback. The normal DSL needs no grammar.

use std::path::{Path, PathBuf};

use metafolder_core::simplified::engine::validate;
use metafolder_core::simplified::grammar::{parse_grammar, Grammar};


/// `$XDG_CONFIG_HOME/metafolder/daemon/query-grammar`.
pub fn grammar_path() -> Option<PathBuf> {
    metafolder_core::config::crate_config_dir("daemon").map(|d| d.join("query-grammar"))
}

/// Reads, parses and validates the grammar at `path`. A missing or malformed
/// file is an error; there is no fall back to a shipped default (spec-config).
pub fn load_grammar(path: &Path) -> Result<Grammar, String> {
    let src = metafolder_core::config::read_required(path)?;
    let grammar = parse_grammar(&src)?;
    validate(&grammar)?;
    Ok(grammar)
}

/// Startup entry point: resolves the path, then loads, parses and validates
/// the grammar. A missing or malformed grammar is a hard error (spec-config).
pub fn init() -> Result<Grammar, String> {
    let path = grammar_path().ok_or("cannot determine the daemon configuration directory")?;
    load_grammar(&path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use metafolder_core::simplified::engine::expand;

    /// The shipped grammar, embedded for tests only (never a runtime fallback).
    const DEFAULT_GRAMMAR: &str = include_str!("../default-config/query-grammar");

    #[test]
    fn default_grammar_parses_and_validates() {
        let g = parse_grammar(DEFAULT_GRAMMAR).expect("default grammar parses");
        validate(&g).expect("default grammar validates");
    }

    #[test]
    fn default_grammar_expands_examples() {
        let g = parse_grammar(DEFAULT_GRAMMAR).unwrap();
        assert_eq!(expand(&g, "genre=jazz").unwrap(), "genre = \"jazz\"");
        assert_eq!(expand(&g, "rating=4").unwrap(), "rating = 4");
        assert_eq!(expand(&g, "seen=true").unwrap(), "seen = true");
        assert_eq!(expand(&g, "size>=100MB").unwrap(), "mfr_size >= 104857600");
        assert_eq!(
            expand(&g, "fav genre=jazz").unwrap(),
            "rating >= 4 AND genre = \"jazz\""
        );
        assert_eq!(expand(&g, "a=x OR b=y").unwrap(), "a = \"x\" OR b = \"y\"");
    }

    #[test]
    fn default_grammar_supports_all_comparison_operators() {
        let g = parse_grammar(DEFAULT_GRAMMAR).unwrap();
        assert_eq!(expand(&g, "rating>3").unwrap(), "rating > 3");
        assert_eq!(expand(&g, "rating<3").unwrap(), "rating < 3");
        assert_eq!(expand(&g, "rating>=3").unwrap(), "rating >= 3");
        assert_eq!(expand(&g, "rating<=3").unwrap(), "rating <= 3");
        assert_eq!(expand(&g, "rating!=3").unwrap(), "rating != 3");
        assert_eq!(expand(&g, "genre!=jazz").unwrap(), "genre != \"jazz\"");
    }

    #[test]
    fn default_grammar_accepts_explicit_and() {
        let g = parse_grammar(DEFAULT_GRAMMAR).unwrap();
        // Juxtaposition and the explicit AND keyword are equivalent.
        let expected = "a = \"x\" AND b = \"y\"";
        assert_eq!(expand(&g, "a=x b=y").unwrap(), expected);
        assert_eq!(expand(&g, "a=x AND b=y").unwrap(), expected);
    }

    #[test]
    fn default_grammar_expands_relative_date_macros() {
        use metafolder_core::simplified::engine::expand_at;
        let g = parse_grammar(DEFAULT_GRAMMAR).unwrap();
        let now = 1_000_000_000_000;
        // younger = more recent than (now - duration); older = before it.
        assert_eq!(
            expand_at(&g, "modified younger 3d", now).unwrap(),
            format!("mfr_mtime > @{}", now - 3 * 86_400_000)
        );
        assert_eq!(
            expand_at(&g, "created older 2 weeks", now).unwrap(),
            format!("mfr_btime < @{}", now - 2 * 604_800_000)
        );
        // Abbreviations and word/number juxtaposition both tokenize alike.
        assert_eq!(
            expand_at(&g, "modified younger 30min", now).unwrap(),
            format!("mfr_mtime > @{}", now - 30 * 60_000)
        );
        assert_eq!(
            expand_at(&g, "modified older 6h", now).unwrap(),
            format!("mfr_mtime < @{}", now - 6 * 3_600_000)
        );
    }

    #[test]
    fn default_grammar_expands_absolute_date_macros() {
        let g = parse_grammar(DEFAULT_GRAMMAR).unwrap();
        assert_eq!(
            expand(&g, "modified since \"2024-01-01\"").unwrap(),
            "mfr_mtime >= @\"2024-01-01\""
        );
        assert_eq!(
            expand(&g, "created before \"2024-06-01\"").unwrap(),
            "mfr_btime < @\"2024-06-01\""
        );
        assert_eq!(
            expand(&g, "created between \"2023-01-01\" and \"2024-01-01\"").unwrap(),
            "mfr_btime >= @\"2023-01-01\" AND mfr_btime <= @\"2024-01-01\""
        );
    }

    #[test]
    fn date_macros_produce_valid_dsl() {
        // The expanded text must be accepted by the normal DSL parser.
        let g = parse_grammar(DEFAULT_GRAMMAR).unwrap();
        let dsl = expand(&g, "modified since \"2024-01-01\"").unwrap();
        metafolder_core::dsl::parse_query(&dsl).expect("expanded date macro is valid DSL");
    }

    #[test]
    fn load_grammar_errors_when_missing() {
        let dir = std::env::temp_dir().join(format!("mf-grammar-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("query-grammar");
        let err = load_grammar(&path).unwrap_err();
        assert!(err.contains("missing"), "got: {err}");
        assert!(!path.exists(), "no default is installed");
    }
}
