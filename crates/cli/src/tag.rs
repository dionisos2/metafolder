//! The hierarchical-tag model behind `mf tag` (doc "Hierarchical tags"). It
//! lives in `metafolder_core::tag`, shared with the GUI's `mf:tag`: the
//! field-name convention (the `[tag]` config table), the pure subsumption
//! helpers, and the rewrite an answer makes. Re-exported here, where the CLI's
//! config and commands have always found it.

pub use metafolder_core::tag::*;
