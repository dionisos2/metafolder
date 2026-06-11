//! `mf` — the metafolder CLI: a thin client over the daemon's HTTP API
//! (spec-main "* CLI"). No direct database or filesystem-watching work
//! happens here.

pub mod client;
pub mod commands;
pub mod fieldspec;
pub mod gui;

// The query DSL parser lives in core (shared with the GUI); re-exported so
// `metafolder_cli::dsl::parse_query` keeps working.
pub use metafolder_core::dsl;
