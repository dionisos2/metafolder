//! What the `metafolder-watchd` binary runs: the kernel side and the server
//! put together ([`run`]). Its loops take their system calls as arguments, so
//! each is tested against fakes where the kernel cannot be made to fail on
//! demand.

use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};

use crate::fanotify::{self, Fanotify, MountWatch, ReadOutcome};
use crate::filter::{AccessFilter, CredSource, SystemCreds};
use crate::proto::Event;
use crate::server::{Broker, RootSink};

/// What the broker needs of the kernel side: marks that follow the
/// subscriptions, and one read's bytes turned into wire events. [`Fanotify`]
/// is the real one; `crate::sim` stands in for it where no capability is
/// held — everything above this seam (server, filter, protocol) is the same.
pub trait Group: Send + 'static {
    /// Covers `roots` (the union of every subscriber's) and nothing else.
    fn sync_roots(&mut self, roots: &[PathBuf]) -> Result<()>;
    /// One read's bytes, as wire events.
    fn translate(&mut self, buf: &[u8]) -> ReadOutcome;
}

impl Group for Fanotify {
    fn sync_roots(&mut self, roots: &[PathBuf]) -> Result<()> {
        Fanotify::sync_roots(self, roots)
    }

    fn translate(&mut self, buf: &[u8]) -> ReadOutcome {
        Fanotify::translate(self, buf)
    }
}

/// The bridge from subscriptions to filesystem marks: whatever the subscribers
/// watch is what the kernel is asked to report on.
pub struct MarkSink<G: Group> {
    pub group: Arc<Mutex<G>>,
}

impl<G: Group> RootSink for MarkSink<G> {
    fn set_roots(&self, roots: Vec<PathBuf>) -> Result<()> {
        lock(&self.group).sync_roots(&roots)
    }
}

/// `std::sync::Mutex` that survives a poisoned sibling (see `server::lock`).
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The broker, from the kernel group to the socket. Returns only on failure:
/// at start (fail closed, with the remedy in hand — a broker that cannot mark
/// a filesystem or resolve a handle would silently stream nothing), or when
/// the broadcast loop is gone.
pub fn run(socket: &Path) -> Result<()> {
    let probe = socket.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("/"));
    fanotify::preflight(probe).context("refusing to start")?;

    let fanotify = Arc::new(Mutex::new(Fanotify::open()?));
    let listener = bind(socket)?;

    // The mount table: a filesystem mounted under a subscribed root (a drive
    // plugged in) is one more filesystem to mark, one unmounted is a mark to
    // lift. Without the watch, coverage follows only the subscriptions.
    {
        let fanotify = Arc::clone(&fanotify);
        let watch = MountWatch::open().map(|mut w| move || w.wait());
        std::thread::spawn(move || {
            eprintln!("[watchd] {:#}", follow_mounts(watch, || lock(&fanotify).resync()))
        });
    }

    eprintln!("[watchd] listening on {}", socket.display());
    let reader = lock(&fanotify).reader();
    Err(serve(listener, fanotify, AccessFilter::new(SystemCreds), move |buf| reader.read(buf)))
}

/// The broker over any [`Group`]: subscribers accepted on `listener` and fed
/// on their own thread, the kernel side pumped on this one — until nothing
/// listens any more, which it returns. `read` happens *outside* the group's
/// lock: it blocks until something happens, and a subscription must be able
/// to place its marks meanwhile.
pub fn serve<G: Group, C: CredSource + 'static>(
    listener: UnixListener,
    group: Arc<Mutex<G>>,
    filter: AccessFilter<C>,
    read: impl FnMut(&mut [u8]) -> Result<usize>,
) -> anyhow::Error {
    let sink = Arc::new(MarkSink { group: Arc::clone(&group) });
    let broker = Arc::new(Broker::new(filter, sink));
    let (tx, rx) = std::sync::mpsc::channel::<Event>();
    {
        let broker = Arc::clone(&broker);
        std::thread::spawn(move || broker.serve(listener, rx));
    }
    pump(
        read,
        |bytes| lock(&group).translate(bytes),
        &tx,
        || broker.broadcast_overflow(),
        Duration::from_millis(100),
    )
}

/// Reads the group, translates what it read, and sends it on — until nothing
/// listens any more, which it returns as the error it is. A read error is
/// reported and retried after `backoff`: the group is the machine's only
/// source of events. A kernel overflow is announced to every subscriber (each
/// must reconcile).
pub fn pump(
    mut read: impl FnMut(&mut [u8]) -> Result<usize>,
    mut translate: impl FnMut(&[u8]) -> ReadOutcome,
    tx: &Sender<Event>,
    overflow: impl Fn(),
    backoff: Duration,
) -> anyhow::Error {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = match read(&mut buf) {
            Ok(n) => n,
            Err(err) => {
                eprintln!("[watchd] {err:#}");
                std::thread::sleep(backoff);
                continue;
            }
        };
        let out = translate(&buf[..n]);
        for event in out.events {
            if tx.send(event).is_err() {
                return anyhow!("the broadcast loop is gone");
            }
        }
        if out.kernel_overflow {
            overflow();
        }
    }
}

/// Resyncs the marks after each change of the mount table, for as long as it
/// can be watched — and returns why it no longer can (the table could not even
/// be opened, or waiting on it failed). A failed resync is reported and the
/// watch goes on.
pub fn follow_mounts(
    watch: Result<impl FnMut() -> Result<()>>,
    mut resync: impl FnMut() -> Result<()>,
) -> anyhow::Error {
    let mut wait = match watch {
        Ok(wait) => wait,
        Err(err) => return err.context("mounts appearing later will not be covered"),
    };
    loop {
        if let Err(err) = wait() {
            return err.context("the mount table can no longer be watched");
        }
        if let Err(err) = resync() {
            eprintln!("[watchd] {err:#}");
        }
    }
}

/// Binds the socket, replacing a stale one. World-connectable *on purpose*:
/// the gate is the per-uid filter, not the socket's mode — a subscriber can
/// always connect and then learns only what its own uid may see
/// (docs/watcher-fanotify.md "Permissions").
pub fn bind(socket: &Path) -> Result<UnixListener> {
    bind_with(socket, |p| std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o666)))
}

/// [`bind`], with the opening-up step passed in.
fn bind_with(
    socket: &Path,
    open_up: impl FnOnce(&Path) -> std::io::Result<()>,
) -> Result<UnixListener> {
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
    open_up(socket).with_context(|| format!("cannot open up {socket:?}"))?;
    Ok(listener)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    fn create(p: &str) -> Event {
        Event::Create { path: p.into() }
    }

    #[test]
    fn test_the_pump_retries_a_failed_read_announces_overflows_and_stops_when_unheard() {
        let (tx, rx) = std::sync::mpsc::channel();
        let rx = RefCell::new(Some(rx));
        let mut reads = vec![Err(anyhow!("interrupted somehow")), Ok(1), Ok(1)].into_iter();
        let mut calls = 0;
        let overflows = std::cell::Cell::new(0);
        let err = pump(
            |_| reads.next().unwrap(),
            |_| {
                calls += 1;
                if calls == 1 {
                    return ReadOutcome { events: vec![create("/a")], kernel_overflow: true };
                }
                // The first batch was delivered; then the listener goes.
                let rx = rx.borrow_mut().take().unwrap();
                assert_eq!(rx.try_recv().unwrap(), create("/a"));
                drop(rx);
                ReadOutcome { events: vec![create("/b")], kernel_overflow: false }
            },
            &tx,
            || overflows.set(overflows.get() + 1),
            Duration::ZERO,
        );
        assert_eq!(err.to_string(), "the broadcast loop is gone");
        assert_eq!(overflows.get(), 1);
    }

    #[test]
    fn test_following_the_mounts_resyncs_after_each_change_until_it_cannot_watch() {
        let mut waits = vec![Ok(()), Ok(()), Err(anyhow!("poll failed"))].into_iter();
        let mut resyncs = 0;
        let err = follow_mounts(Ok(|| waits.next().unwrap()), || {
            resyncs += 1;
            if resyncs == 1 {
                Err(anyhow!("a mount refused")) // Reported; the watch goes on.
            } else {
                Ok(())
            }
        });
        assert_eq!(resyncs, 2);
        assert_eq!(format!("{err:#}"), "the mount table can no longer be watched: poll failed");

        let never = Err::<fn() -> Result<()>, _>(anyhow!("no /proc"));
        let err = follow_mounts(never, || Ok(()));
        assert_eq!(format!("{err:#}"), "mounts appearing later will not be covered: no /proc");
    }

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("metafolder-tests")
            .join(format!("watchd-bind-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_the_socket_is_bound_world_connectable_replacing_a_stale_one() {
        use std::os::unix::fs::MetadataExt;
        let dir = scratch_dir("ok");
        let socket = dir.join("run/watchd.sock"); // Its directory made on the way.
        let first = bind(&socket).unwrap();
        assert_eq!(std::fs::metadata(&socket).unwrap().mode() & 0o777, 0o666);
        drop(first); // Left behind, as by a broker that died.
        let _second = bind(&socket).expect("the stale socket is replaced");
        UnixStream::connect(&socket).expect("and it answers");

        // A stale socket left as a broken symlink is replaced all the same.
        let link = dir.join("link.sock");
        std::os::unix::fs::symlink(dir.join("nowhere"), &link).unwrap();
        bind(&link).expect("the broken link is replaced");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_a_socket_that_cannot_be_bound_says_why() {
        let dir = scratch_dir("fail");
        let text = |r: Result<UnixListener>| format!("{:#}", r.unwrap_err());

        let occupied = dir.join("occupied");
        std::fs::create_dir_all(occupied.join("inside")).unwrap();
        assert!(text(bind(&occupied)).starts_with("cannot remove the stale socket"));

        std::fs::write(dir.join("file"), b"").unwrap();
        assert!(text(bind(&dir.join("file/sub/s.sock"))).starts_with("cannot create"));

        // No path at all, so no parent: the kernel *autobinds* an empty name
        // (an abstract address) — and opening up "" is what fails. A broker
        // given `--socket ""` refuses to start either way.
        assert!(text(bind(Path::new(""))).starts_with("cannot open up"));

        let too_long = dir.join("x".repeat(200));
        assert!(text(bind(&too_long)).starts_with("cannot bind"));

        let refused = |_: &Path| Err(std::io::Error::from_raw_os_error(libc::EPERM));
        let err = bind_with(&dir.join("s.sock"), refused).unwrap_err();
        assert!(format!("{err:#}").starts_with("cannot open up"), "{err:#}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_subscriptions_become_marks() {
        if !crate::test_support::in_userns("service::tests::test_subscriptions_become_marks") {
            return;
        }
        let root = crate::test_support::scratch().join("sink");
        std::fs::create_dir_all(&root).unwrap();
        let ok = std::process::Command::new("mount").args(["-t", "tmpfs", "t"]).arg(&root).status();
        assert!(ok.is_ok_and(|s| s.success()));
        let sink = MarkSink { group: Arc::new(Mutex::new(Fanotify::open().unwrap())) };
        sink.set_roots(vec![root.clone()]).unwrap();
        assert!(sink.set_roots(vec![root.join("nope")]).is_err());
    }

    #[test]
    fn test_the_broker_serves_what_the_kernel_reports() {
        // The whole of `run`, in a user namespace over a tmpfs. Where handles
        // do not resolve it must refuse to start — and say so.
        if !crate::test_support::in_userns(
            "service::tests::test_the_broker_serves_what_the_kernel_reports",
        ) {
            return;
        }
        let root = crate::test_support::scratch().join("run");
        std::fs::create_dir_all(&root).unwrap();
        let ok = std::process::Command::new("mount").args(["-t", "tmpfs", "t"]).arg(&root).status();
        assert!(ok.is_ok_and(|s| s.success()));
        let socket = root.join("watchd.sock");
        let (done_tx, done) = std::sync::mpsc::channel();
        {
            let socket = socket.clone();
            std::thread::spawn(move || done_tx.send(run(&socket)));
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        let stream = loop {
            if let Ok(Err(err)) = done.try_recv() {
                assert!(format!("{err:#}").starts_with("refusing to start"), "{err:#}");
                eprintln!("skipped serving: {err:#}");
                return;
            }
            if let Ok(stream) = UnixStream::connect(&socket) {
                break stream;
            }
            assert!(std::time::Instant::now() < deadline, "the broker never listened");
            std::thread::sleep(Duration::from_millis(20));
        };
        stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut lines = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        let subscribe = crate::proto::ClientMsg::Subscribe { roots: vec![root.as_path().into()] };
        (&stream).write_all(crate::proto::encode(&subscribe).as_bytes()).unwrap();
        lines.read_line(&mut line).unwrap();
        assert!(line.contains("subscribed"), "{line}");
        std::fs::create_dir(root.join("new")).unwrap();
        line.clear();
        lines.read_line(&mut line).unwrap();
        let path = root.join("new");
        assert!(line.contains(path.to_str().unwrap()), "{line}");
    }
}
