//! Loading the global simplified-query grammar from the user configuration
//! (spec-config; spec-query "Configuration"):
//! `$XDG_CONFIG_HOME/metafolder/core/query-grammar`, installed by
//! `metafolder-sync-config`. A missing or malformed grammar is an error; there
//! is no embedded fallback. Expansion is a pure, client-side transformation
//! (GUI/CLI call [`crate::simplified::engine`] directly) — never the daemon.

use std::path::{Path, PathBuf};

use crate::config;
use crate::simplified::engine::validate;
use crate::simplified::grammar::{parse_grammar, Grammar};

/// `$XDG_CONFIG_HOME/metafolder/core/query-grammar`.
pub fn grammar_path() -> Option<PathBuf> {
    config::crate_config_dir("core").map(|dir| dir.join("query-grammar"))
}

/// Reads, parses and validates the grammar at `path`, returning both the
/// parsed grammar and its raw source text (for display, e.g. the GUI help
/// page). A missing or malformed file is an error; there is no fall back to a
/// shipped default (spec-config).
pub fn load_grammar_with_source(path: &Path) -> Result<(Grammar, String), String> {
    let src = config::read_required(path)?;
    let grammar = parse_grammar(&src)?;
    validate(&grammar)?;
    Ok((grammar, src))
}

/// Reads, parses and validates the grammar at `path`. A missing or malformed
/// file is an error; there is no fall back to a shipped default (spec-config).
pub fn load_grammar(path: &Path) -> Result<Grammar, String> {
    load_grammar_with_source(path).map(|(grammar, _)| grammar)
}

/// Resolves the configured path and loads the grammar.
pub fn load() -> Result<Grammar, String> {
    load_source().map(|(grammar, _)| grammar)
}

/// Resolves the configured path and loads the grammar with its raw source text.
pub fn load_source() -> Result<(Grammar, String), String> {
    let path = grammar_path().ok_or("cannot determine the configuration directory")?;
    load_grammar_with_source(&path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::simplified::engine::{expand, expand_at};

    /// The shipped grammar, embedded for tests only (never a runtime fallback).
    const DEFAULT_GRAMMAR: &str = include_str!("../../default-config/query-grammar");

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
        assert_eq!(expand(&g, "fav genre=jazz").unwrap(), "rating >= 4 AND genre = \"jazz\"");
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
    fn default_grammar_expands_presence_predicates() {
        let g = parse_grammar(DEFAULT_GRAMMAR).unwrap();
        // `?` present, `!?` absent (value is Nothing), `??` unknown (never set).
        // `p` is the alias for the file path field `mfr_path`.
        assert_eq!(expand(&g, "p ?").unwrap(), "mfr_path IS PRESENT");
        assert_eq!(expand(&g, "p !?").unwrap(), "mfr_path IS ABSENT");
        assert_eq!(expand(&g, "p ??").unwrap(), "mfr_path IS UNKNOWN");
        // A bare field passes through; combines with other terms.
        assert_eq!(expand(&g, "tag ?? p !?").unwrap(), "tag IS UNKNOWN AND mfr_path IS ABSENT");
        // Every expansion is valid normal DSL.
        for input in ["p ?", "p !?", "p ??"] {
            crate::dsl::parse_query(&expand(&g, input).unwrap()).expect("valid DSL");
        }
    }

    #[test]
    fn default_grammar_expands_osm_predicates() {
        let g = parse_grammar(DEFAULT_GRAMMAR).unwrap();
        // Path-mode OSM over a tree_ref field (`p` = mfr_path); multi-term
        // needs a quoted string.
        assert_eq!(expand(&g, r#"p osm "scien fic""#).unwrap(), r#"osm(mfr_path, "scien fic")"#);
        // Direct-mode OSM over a field's own text.
        assert_eq!(expand(&g, r#"label osmd "sf""#).unwrap(), r#"osmd(label, "sf")"#);
        // A single term may be a bare word (no quotes).
        assert_eq!(expand(&g, "name osmd config").unwrap(), r#"osmd(name, "config")"#);
        // Composes with the boolean skeleton like any other predicate.
        assert_eq!(
            expand(&g, r#"fav p osm "jazz""#).unwrap(),
            r#"rating >= 4 AND osm(mfr_path, "jazz")"#
        );
        // Every expansion is valid normal DSL.
        for input in [r#"p osm "scien fic""#, r#"label osmd "sf""#, "name osmd config"] {
            crate::dsl::parse_query(&expand(&g, input).unwrap()).expect("valid DSL");
        }
    }

    #[test]
    fn default_grammar_expands_tag_macros() {
        let g = parse_grammar(DEFAULT_GRAMMAR).unwrap();
        // Bare `#X` = the fuzzy finder: ordered-substring match (osm) over the
        // tag path, so you can name a leaf without its ancestors. A `/` is an
        // osm term separator, not a path anchor: `#a/b` matches a path with `a`
        // then `b` in order, not necessarily contiguous or from the root.
        assert_eq!(
            expand(&g, "#musique").unwrap(),
            r#"tag -> (mf_schema = "tag" AND osm(path, "musique"))"#
        );
        assert_eq!(
            expand(&g, "#musique/jazz").unwrap(),
            r#"tag -> (mf_schema = "tag" AND osm(path, "musique jazz"))"#
        );
        // A quoted value is one contiguous osm term.
        assert_eq!(
            expand(&g, r#"#"musique/jazz""#).unwrap(),
            r#"tag -> (mf_schema = "tag" AND osm(path, "musique/jazz"))"#
        );
        // `#=P` anchors the *full* path from the root and matches its subtree
        // (`=>*`); an unquoted slash-path is accepted.
        assert_eq!(
            expand(&g, "#=musique").unwrap(),
            r#"tag -> (mf_schema = "tag" AND path =>* "musique")"#
        );
        assert_eq!(
            expand(&g, "#=musique/jazz").unwrap(),
            r#"tag -> (mf_schema = "tag" AND path =>* "musique/jazz")"#
        );
        // `##=P` anchors the full path to that exact node (`=`).
        assert_eq!(
            expand(&g, "##=musique/jazz").unwrap(),
            r#"tag -> (mf_schema = "tag" AND path = "musique/jazz")"#
        );
        // Composes with the boolean skeleton like any other predicate.
        assert_eq!(
            expand(&g, "#musique rating>3").unwrap(),
            r#"tag -> (mf_schema = "tag" AND osm(path, "musique")) AND rating > 3"#
        );
        // Every expansion is valid normal DSL.
        for input in
            ["#musique", "#musique/jazz", r#"#"musique/jazz""#, "#=musique/jazz", "##=musique/jazz"]
        {
            crate::dsl::parse_query(&expand(&g, input).unwrap()).expect("valid DSL");
        }
    }

    #[test]
    fn default_grammar_accepts_explicit_and() {
        let g = parse_grammar(DEFAULT_GRAMMAR).unwrap();
        let expected = "a = \"x\" AND b = \"y\"";
        assert_eq!(expand(&g, "a=x b=y").unwrap(), expected);
        assert_eq!(expand(&g, "a=x AND b=y").unwrap(), expected);
    }

    #[test]
    fn default_grammar_expands_relative_date_macros() {
        let g = parse_grammar(DEFAULT_GRAMMAR).unwrap();
        let now = 1_000_000_000_000;
        assert_eq!(
            expand_at(&g, "modified younger 3d", now).unwrap(),
            format!("mfr_mtime > @{}", now - 3 * 86_400_000)
        );
        assert_eq!(
            expand_at(&g, "created older 2 weeks", now).unwrap(),
            format!("mfr_btime < @{}", now - 2 * 604_800_000)
        );
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
        let g = parse_grammar(DEFAULT_GRAMMAR).unwrap();
        let dsl = expand(&g, "modified since \"2024-01-01\"").unwrap();
        crate::dsl::parse_query(&dsl).expect("expanded date macro is valid DSL");
    }

    #[test]
    fn load_grammar_with_source_returns_raw_text_and_a_valid_grammar() {
        let dir = std::env::temp_dir()
            .join("metafolder-tests")
            .join(format!("mf-core-grammar-src-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("query-grammar");
        std::fs::write(&path, DEFAULT_GRAMMAR).unwrap();

        let (grammar, source) = load_grammar_with_source(&path).expect("loads");
        // The source is the file content verbatim, as loaded.
        assert_eq!(source, DEFAULT_GRAMMAR);
        assert!(source.contains("predicate"), "raw grammar source is returned");
        // The grammar is the same one `load_grammar` parses and validates.
        validate(&grammar).expect("returned grammar validates");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_grammar_errors_when_missing() {
        let dir = std::env::temp_dir()
            .join("metafolder-tests")
            .join(format!("mf-core-grammar-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("query-grammar");
        let err = load_grammar(&path).unwrap_err();
        assert!(err.contains("missing"), "got: {err}");
        assert!(!path.exists(), "no default is installed");
    }
}
