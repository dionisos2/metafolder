//! `mf` — the metafolder CLI: a thin client over the daemon's HTTP API
//! (spec-main "* CLI"). No direct database or filesystem-watching work
//! happens here.

pub mod client;
pub mod commands;
pub mod dsl;
pub mod fieldspec;
