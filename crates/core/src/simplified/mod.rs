//! Simplified query language: an ergonomic, user-configurable surface that
//! transpiles to the normal query DSL text (spec-query "* Simplified query
//! language"). The grammar engine is a small hand-written recursive-descent /
//! PEG-style interpreter with output templates; it emits normal DSL text that
//! `crate::dsl::parse_query` then turns into the `Query` IR.

pub mod engine;
pub mod grammar;
pub mod lexer;
pub mod load;
pub mod template;

/// Expands simplified-language text to the normal DSL with the configured
/// grammar (`load::load`), relative dates against the local clock — pure and
/// client-side, never a daemon round-trip (spec-query).
pub fn expand_configured(text: &str) -> Result<String, String> {
    engine::expand(&load::load()?, text)
}
