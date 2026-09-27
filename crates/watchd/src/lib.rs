//! The privileged fanotify broker (spec-file-tracking "Watch sources and
//! regimes"; design: docs/watcher-fanotify.md). One process per machine holds
//! the fanotify group that covers the filesystems of every subscribed repository
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
//! - [`fanotify`] — the kernel side: one group, one mark per covered filesystem,
//!   file-handle events resolved to paths. [`server`] is the subscriber-facing
//!   half: one bounded queue per subscriber, and the rule that a slow consumer
//!   *loses* events and is told so ([`proto::ServerMsg::Overflow`]) rather than
//!   making the whole machine wait;
//! - [`service`] — the two put together, as the `metafolder-watchd` binary runs
//!   them.
//!
//! Coverage: every production line runs under `cargo test`, the kernel-facing
//! ones in a user namespace over a tmpfs (`unshare -rm`). Those that need a
//! handle *resolved* run only where the kernel allows it there (Linux 6.10+)
//! and nothing forbids it — not in a container whose seccomp profile refuses
//! `open_by_handle_at`: measure on a host (`cargo llvm-cov -p metafolder-watchd`).

pub mod fanotify;
pub mod filter;
pub mod proto;
pub mod server;
pub mod service;
#[cfg(test)]
mod test_support;
