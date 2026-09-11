//! The finder's DSL text and its IR must mean the same thing.
//!
//! `panel-shim/finder.js` builds the finder's quick-filter clause twice: as IR
//! (what the panel runs) and as DSL text (what `mf gui query` hands a script,
//! spec-gui "Finder"). Two builders, one meaning — so they are pinned to the
//! same vectors, `panel-shim/finder-vectors.json`:
//!
//! - `frontend/tests/finder.test.ts` asserts the JS builders produce each
//!   case's `text` and `ir`;
//! - this test asserts the real parser turns that `text` into that `ir`.
//!
//! Together they close the loop: the text a script receives parses to exactly
//! the query the panel is displaying. A change to either builder that drifts
//! from the other fails on one side or the other.

use metafolder_core::dsl::parse_query;
use serde_json::Value;

/// The vectors, shared with the vitest suite.
fn cases() -> Vec<Value> {
    let raw = include_str!("../panel-shim/finder-vectors.json");
    let doc: Value = serde_json::from_str(raw).expect("the vectors are valid JSON");
    doc["cases"].as_array().expect("cases is an array").clone()
}

#[test]
fn test_the_finder_text_parses_to_the_finder_ir() {
    let cases = cases();
    assert!(!cases.is_empty(), "the vectors must not be empty");
    for case in cases {
        let name = case["name"].as_str().unwrap_or("<unnamed>");
        let text = case["text"].as_str().expect("every case has a text");
        let expected = &case["ir"];

        let parsed =
            parse_query(text).unwrap_or_else(|e| panic!("{name}: {text:?} must parse: {e}"));
        let actual = serde_json::to_value(&parsed).expect("a Query serializes");
        assert_eq!(&actual, expected, "{name}: {text:?} does not mean its IR");
    }
}

#[test]
fn test_every_vector_keeps_its_terms_whitespace_free() {
    // The invariant the round-trip rests on: `splitTerms` never yields a term
    // with whitespace, and the parser re-splits the quoted string the same way.
    // A vector that broke it would parse back as several terms and the two
    // builders could never agree.
    for case in cases() {
        let name = case["name"].as_str().unwrap_or("<unnamed>");
        for term in case["terms"].as_array().expect("terms is an array") {
            let term = term.as_str().expect("a term is a string");
            assert!(
                !term.chars().any(char::is_whitespace),
                "{name}: the term {term:?} carries whitespace"
            );
        }
    }
}
