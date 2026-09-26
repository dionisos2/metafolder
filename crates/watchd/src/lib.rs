//! The privileged fanotify broker (spec-file-tracking "Watch sources and
//! regimes"; design: docs/watcher-fanotify.md). One process per machine holds
//! the fanotify group that covers the mounts of every subscribed repository
//! root, resolves the file handles the kernel reports into paths, and streams
//! the events to subscribers over a Unix socket — filtered per subscriber, so
//! no hop ever reveals more than the next hop's own credentials could discover
//! by walking the filesystem ([`filter`]).
//!
//! Three moving parts, each usable on its own:
//!
//! - [`proto`] — the NDJSON wire protocol: [`proto::ClientMsg`] in,
//!   [`proto::ServerMsg`] out, [`proto::Event`] in the daemon's own
//!   vocabulary;
//! - [`filter`] — the per-uid DAC filter, over a [`filter::CredSource`] that
//!   tests can fake;
//! - [`fanotify`] — the kernel side: one group, one mark per covered mount,
//!   file-handle events resolved to paths. [`server`] is the subscriber-facing
//!   half: one bounded queue per subscriber, and the rule that a slow consumer
//!   *loses* events and is told so ([`proto::ServerMsg::Overflow`]) rather than
//!   making the whole machine wait.

pub mod fanotify;
pub mod filter;
pub mod proto;
pub mod server;
