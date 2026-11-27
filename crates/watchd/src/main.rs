//! `metafolder-watchd` — the privileged fanotify broker
//! (doc "The fanotify broker"; doc "Watch sources and regimes"). One per machine: it
//! holds the fanotify group covering the
//! filesystems of every subscribed repository root, and streams the events to the
//! subscribers' daemons over a Unix socket — each seeing only what its own
//! credentials could discover.
//!
//! It is meant to run as a system service with exactly two capabilities —
//! `CAP_SYS_ADMIN` (to mark a filesystem) and `CAP_DAC_READ_SEARCH` (to resolve a
//! file handle to a path) — and nothing else in the process is privileged by
//! construction: the daemon stays unprivileged, which is the whole point of
//! the split. `fanotify::preflight` fails at startup with the remedy named
//! when they are missing.

#[cfg(target_os = "linux")]
use std::path::PathBuf;

#[cfg(target_os = "linux")]
use anyhow::Result;
#[cfg(target_os = "linux")]
use clap::Parser;

#[cfg(target_os = "linux")]
#[derive(Parser)]
#[command(
    about = "The metafolder fanotify broker: fanotify events to subscribers over a Unix socket"
)]
struct Args {
    /// Where to listen for subscribers.
    #[arg(long, default_value = "/run/metafolder/watchd.sock")]
    socket: PathBuf,
}

#[cfg(target_os = "linux")]
fn main() -> Result<()> {
    metafolder_watchd::service::run(&Args::parse().socket)
}

/// fanotify is Linux's: elsewhere the daemon watches with its notify source,
/// and there is no broker to run (doc "Platform").
#[cfg(not(target_os = "linux"))]
fn main() -> std::process::ExitCode {
    eprintln!("metafolder-watchd runs on Linux only: it is a fanotify broker");
    std::process::ExitCode::FAILURE
}
