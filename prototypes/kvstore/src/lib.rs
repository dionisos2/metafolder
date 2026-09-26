//! Prototype of the disk-resident storage and query backend
//! (docs/spec-storage.org, increment 1). Throwaway: it exists to measure the
//! key layout on the everyday gestures before anything in the daemon changes.

pub mod model;
pub mod query;
pub mod reference;
pub mod store;
