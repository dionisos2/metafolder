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
