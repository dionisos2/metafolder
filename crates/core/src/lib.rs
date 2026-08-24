pub mod auth;
pub mod config;
#[cfg(feature = "sync-config")]
pub mod config_sync;
pub mod date;
pub mod dsl;
pub mod hex;
pub mod ignore;
pub mod ignore_presets;
pub mod metarecord;
pub mod progress;
pub mod query;
pub mod repo_init;
pub mod scripts;
pub mod simplified;
pub mod sync;
pub mod trash;

/// Wire-protocol version shared by every metafolder service (daemon, GUI, CLI).
///
/// This is **not** the crate semver (`CARGO_PKG_VERSION`): it identifies the
/// HTTP API contract — request/response shapes, query IR, event-log wire form —
/// so a client can detect that it is talking to a daemon it cannot understand.
/// The daemon reports it in `GET /health` (`api_version`); the GUI compares that
/// against its own compiled-in value and warns the user on a mismatch.
///
/// **Bump this by one whenever a change to the wire contract would make an
/// older client misbehave** (a renamed/removed field, a changed JSON shape, new
/// required parameters, altered semantics). Purely additive, backward-compatible
/// changes do not require a bump. It is a single monotonic integer on purpose:
/// same number ⇒ compatible, different number ⇒ refuse/ warn — no range
/// negotiation.
pub const API_VERSION: u32 = 2;
