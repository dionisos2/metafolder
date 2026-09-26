//! `metafolder-watchd` — the privileged fanotify broker
//! (docs/watcher-fanotify.md "The broker"; spec-file-tracking "Watch sources
//! and regimes"). One per machine: it holds the fanotify group covering the
//! mounts of every subscribed repository root, and streams the events to the
//! subscribers' daemons over a Unix socket — each seeing only what its own
//! credentials could discover.
//!
//! It is meant to run as a system service with exactly two capabilities —
//! `CAP_SYS_ADMIN` (to mark a mount) and `CAP_DAC_READ_SEARCH` (to resolve a
//! file handle to a path) — and nothing else in the process is privileged by
//! construction: the daemon stays unprivileged, which is the whole point of
//! the split. [`fanotify::preflight`] fails at startup with the remedy named
//! when they are missing.

use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use anyhow::{Context, Result};
use clap::Parser;

use metafolder_watchd::fanotify::{self, Fanotify};
use metafolder_watchd::filter::{AccessFilter, SystemCreds};
use metafolder_watchd::proto::Event;
use metafolder_watchd::server::{Broker, RootSink};

#[derive(Parser)]
#[command(
    about = "The metafolder fanotify broker: fanotify events to subscribers over a Unix socket"
)]
struct Args {
    /// Where to listen for subscribers.
    #[arg(long, default_value = "/run/metafolder/watchd.sock")]
    socket: PathBuf,
}

/// The bridge from subscriptions to mount marks: whatever the subscribers
/// watch is what the kernel is asked to report on.
struct MarkSink {
    fanotify: Arc<Mutex<Fanotify>>,
}

impl RootSink for MarkSink {
    fn set_roots(&self, roots: Vec<PathBuf>) -> Result<()> {
        lock(&self.fanotify).sync_roots(&roots)
    }
}

/// `std::sync::Mutex` that survives a poisoned sibling (see `server::lock`).
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

fn main() -> Result<()> {
    let args = Args::parse();

    // Fail closed, with the remedy in hand: a broker that cannot mark a mount
    // or resolve a handle is a broker that would silently stream nothing.
    let probe =
        args.socket.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("/"));
    fanotify::preflight(probe).context("refusing to start")?;

    let fanotify = Arc::new(Mutex::new(Fanotify::open()?));
    let sink = Arc::new(MarkSink { fanotify: Arc::clone(&fanotify) });
    let broker = Arc::new(Broker::new(AccessFilter::new(SystemCreds), sink));

    let listener = bind(&args.socket)?;

    // The kernel side: one thread reads the group and feeds the broadcast loop
    // that `serve` runs below. A read error is reported and retried — the
    // group is the machine's only source of events.
    let (tx, rx) = std::sync::mpsc::channel::<Event>();
    {
        let fanotify = Arc::clone(&fanotify);
        let broker = Arc::clone(&broker);
        std::thread::spawn(move || loop {
            match lock(&fanotify).read_events() {
                Ok(out) => {
                    for event in out.events {
                        if tx.send(event).is_err() {
                            return; // The broker is shutting down.
                        }
                    }
                    if out.kernel_overflow {
                        // The kernel dropped events: every subscriber must
                        // reconcile, so everyone is told.
                        broker.broadcast_overflow();
                    }
                }
                Err(err) => {
                    eprintln!("[watchd] fanotify read failed: {err:#}");
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
        });
    }

    eprintln!("[watchd] listening on {}", args.socket.display());
    broker.serve(listener, rx);
    Ok(())
}

/// Binds the socket, replacing a stale one. World-connectable *on purpose*:
/// the gate is the per-uid filter, not the socket's mode — a subscriber can
/// always connect and then learns only what its own uid may see
/// (docs/watcher-fanotify.md "Permissions").
fn bind(socket: &Path) -> Result<UnixListener> {
    // `symlink_metadata`, never `exists()`: a stale socket left as a broken
    // symlink is a stale socket all the same, and `bind` would then fail with
    // EADDRINUSE instead of replacing it.
    if std::fs::symlink_metadata(socket).is_ok() {
        std::fs::remove_file(socket)
            .with_context(|| format!("cannot remove the stale socket at {socket:?}"))?;
    }
    if let Some(dir) = socket.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {dir:?}"))?;
    }
    let listener = UnixListener::bind(socket).with_context(|| format!("cannot bind {socket:?}"))?;
    std::fs::set_permissions(socket, std::fs::Permissions::from_mode(0o666))
        .with_context(|| format!("cannot open up {socket:?}"))?;
    Ok(listener)
}
