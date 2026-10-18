//! The fanotify watch source — the *coverage* regime of doc "Watch sources and regimes".
//! It is a *client* of the machine's fanotify
//! broker (crates/watchd, doc "The fanotify broker"): one kernel registration
//! covers the repository root, and the events arrive over a Unix socket, each
//! one already narrowed to what this daemon's own uid may see. No
//! per-directory watches, no budget, no placement walk — `refresh` has nothing
//! to place.
//!
//! The broker is a separate process and can go away. The client then says so
//! and reconnects for ever: events in between are lost, exactly like the events
//! of a daemon that was down, and a `reconcile` is what closes the gap
//! (doc "Event batching"). The *first* connection is different:
//! it is made synchronously, so a load that wanted fanotify either knows it is
//! covered or falls back (or fails) with the reason.
//!
//! A filesystem mounted beneath the root that the broker cannot mark — most
//! FUSE filesystems report a zero fsid (gocryptfs) — is named by the broker,
//! and watched by an inotify source of its own, inside it only
//! (doc "Uncovered mounts").

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use metafolder_core::sync::MutexExt;
use uuid::Uuid;

use metafolder_watchd::proto::{self, ClientMsg, Event, ServerMsg};

use crate::executor::FsEvent;
use crate::state::RepoState;
use crate::tree_cache::TreeCache;
use crate::watcher::{inotify, relative, Gap, Placement, Regime};

/// How long the first connection waits for the broker's handshake before the
/// load decides the broker is not there. Generous for a local socket, short
/// enough that a daemon starting with a half-dead broker still comes up.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// How long between reconnection attempts once the stream has died.
const RECONNECT_DELAY: Duration = Duration::from_secs(2);

/// What closes a gap in the stream, as a diagnostic says it: what a daemon
/// that was down does. A reconcile picks up the new files, but follows a move
/// only on a full hash (most files have none: it offers a candidate) and never
/// writes `nothing` — what moved or went is the orphan scan's (doc "Orphans").
const CLOSE_THE_GAP: &str = "run `mf reconcile` to pick up the new files, then \
                             `mf orphan detect` for those moved or deleted meanwhile";

pub(crate) struct Source {
    // The connection lives on the client thread, which carries the
    // repository's `Uuid` for diagnostics and *no* `Arc<RepoState>` — an Arc
    // would keep the repository (and its exclusive lock) alive for ever
    // (.semgrep/invariants.yml `mf-repostate-arc-in-background-task`). What the
    // source keeps is the way to end that thread when it is dropped.
    stop: Arc<Stop>,
    uncovered: Arc<Uncovered>,
}

/// The mounts beneath the root the broker cannot cover, and the inotify
/// source watching inside them (doc "Uncovered mounts"). Shared with the
/// client thread, which is told of them by the broker.
struct Uncovered {
    inotify: Arc<inotify::Source>,
    gaps: Mutex<Vec<Gap>>,
    /// The root as the broker names it (canonical), and as the repository does.
    canonical: PathBuf,
    root: PathBuf,
    repo: Weak<RepoState>,
}

impl Uncovered {
    /// Records what the broker said is uncovered now; whether that changed.
    fn set(&self, gaps: &[proto::Gap]) -> bool {
        let mut gaps: Vec<Gap> = gaps
            .iter()
            .filter_map(|g| {
                let rest = g.mount.as_path().strip_prefix(&self.canonical).ok()?;
                Some(Gap { path: self.root.join(rest), reason: g.reason.clone() })
            })
            .collect();
        gaps.sort_by(|a, b| a.path.cmp(&b.path));
        let mounts = gaps.iter().map(|g| g.path.clone()).collect();
        *self.gaps.lock_recover() = gaps;
        self.inotify.set_within(mounts)
    }

    /// [`set`](Self::set), and the watches placed again when it changed —
    /// on a thread of its own: placing waits for the store's lock, and the
    /// stream must go on being read meanwhile.
    fn told(&self, gaps: &[proto::Gap]) {
        if !self.set(gaps) {
            return;
        }
        let repo = self.repo.clone();
        std::thread::spawn(move || {
            if let Some(repo) = repo.upgrade() {
                let conn = repo.conn.lock_recover();
                repo.refresh_watches(&conn);
            }
        });
    }
}

/// How a dropped [`Source`] ends its client thread: the flag says "do not
/// (re)connect", and the live stream is shut down so a read blocked on it
/// returns at once. Both under one lock, so a connection made concurrently
/// with the drop is either seen by it or refused by [`Stop::attach`].
#[derive(Default)]
struct Stop {
    state: std::sync::Mutex<StopState>,
}

#[derive(Default)]
struct StopState {
    stopped: bool,
    stream: Option<UnixStream>,
}

impl Stop {
    /// Records `reader`'s stream as the live one; `false` once stopped (the
    /// caller then drops the connection and leaves).
    fn attach(&self, reader: &BufReader<UnixStream>) -> bool {
        let mut state = self.state.lock_recover();
        if state.stopped {
            return false;
        }
        state.stream = reader.get_ref().try_clone().ok();
        true
    }

    fn is_stopped(&self) -> bool {
        self.state.lock_recover().stopped
    }

    fn stop(&self) {
        let mut state = self.state.lock_recover();
        state.stopped = true;
        if let Some(stream) = state.stream.take() {
            // The broker sees the subscription end; the reader sees EOF.
            let _ = stream.shutdown(std::net::Shutdown::Both);
        }
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        self.stop.stop();
    }
}

/// Why [`pump`] returned.
enum Ended {
    /// The stream ended: the broker went away (or the source was dropped,
    /// which [`Stop`] tells apart).
    Stream,
    /// The ingest thread is gone: the repository was unloaded.
    Unloaded,
}

impl Source {
    /// Connects to the broker, subscribes to the repository root (the
    /// handshake — its answer *means* "you are covered"), and hands the stream
    /// to a thread that feeds `tx`.
    pub(crate) fn start(
        repo: &Arc<RepoState>,
        socket: &Path,
        tx: Sender<Vec<(FsEvent, Option<i64>)>>,
    ) -> Result<Arc<Self>> {
        // The broker resolves and matches paths itself (its events carry
        // kernel-resolved names), so the root is subscribed under the same
        // name it will come back with.
        let root = std::fs::canonicalize(&repo.config.root).with_context(|| {
            format!("cannot resolve the repository root {:?}", repo.config.root)
        })?;
        let internal_dir = repo.internal_dir();

        // The handshake is synchronous: a load that wanted fanotify either
        // knows it is covered, or falls back (or fails) with the reason.
        let (reader, gaps) = subscribe(socket, &root)?;
        let uncovered = Arc::new(Uncovered {
            inotify: inotify::Source::start_within(repo, tx.clone())?,
            gaps: Mutex::new(Vec::new()),
            canonical: root.clone(),
            root: repo.config.root.clone(),
            repo: Arc::downgrade(repo),
        });
        // Before the load's placement, which watches inside them at once.
        uncovered.set(&gaps);

        // From here on the stream *is* the source. The thread owns every later
        // (re)connection; the handshake one is handed over as-is.
        let repo_uuid = repo.uuid();
        let socket = socket.to_path_buf();
        let stop = Arc::new(Stop::default());
        let thread_stop = stop.clone();
        let thread_uncovered = uncovered.clone();
        std::thread::spawn(move || {
            let uncovered = thread_uncovered;
            let stop = thread_stop;
            let mut reader = Some(reader);
            loop {
                let current = match reader.take() {
                    Some(current) => current,
                    None => match subscribe(&socket, &root) {
                        Ok((current, gaps)) => {
                            uncovered.told(&gaps);
                            if stop.is_stopped() {
                                return;
                            }
                            // Events while the connection was down are gone,
                            // like those of a daemon that was down: the gap
                            // closes with a reconcile (doc "Event batching").
                            crate::diagnostics::error_for(
                                "watcher",
                                format!(
                                    "the fanotify broker is back at {}: changes made while it \
                                     was away were not seen — {CLOSE_THE_GAP}",
                                    socket.display()
                                ),
                                repo_uuid,
                            );
                            current
                        }
                        Err(_) => {
                            std::thread::sleep(RECONNECT_DELAY);
                            if stop.is_stopped() {
                                return;
                            }
                            continue;
                        }
                    },
                };
                if !stop.attach(&current) {
                    return; // Dropped while connecting: the repository is gone.
                }
                let on_gaps = |gaps: &[proto::Gap]| uncovered.told(gaps);
                if let Ended::Unloaded =
                    pump(current, &root, &internal_dir, &repo_uuid, &tx, &on_gaps)
                {
                    return;
                }
                if stop.is_stopped() {
                    return; // The stream ended because the source was dropped.
                }
                crate::diagnostics::error_for(
                    "watcher",
                    format!(
                        "the fanotify broker connection was lost ({}): changes go unnoticed \
                         until it returns — then {CLOSE_THE_GAP}",
                        socket.display()
                    ),
                    repo_uuid,
                );
                std::thread::sleep(RECONNECT_DELAY);
                if stop.is_stopped() {
                    return;
                }
            }
        });

        Ok(Arc::new(Source { stop, uncovered }))
    }
}

/// Connects and subscribes. The answer *means* "you are covered", so it is
/// waited for — with a timeout, so a half-dead broker cannot wedge a load.
///
/// Returns the *reader*, never a fresh stream: the handshake's `BufReader` may
/// already hold the first events behind the answer (the broker sends them at
/// once), and reading from anything else would lose exactly those — and the
/// mounts beneath the root the broker cannot cover.
fn subscribe(socket: &Path, root: &Path) -> Result<(BufReader<UnixStream>, Vec<proto::Gap>)> {
    let stream = UnixStream::connect(socket)
        .with_context(|| format!("no broker at {}", socket.display()))?;
    stream.set_read_timeout(Some(HANDSHAKE_TIMEOUT))?;
    let mut writer = stream.try_clone()?;
    let mut reader = BufReader::new(stream);
    writer
        .write_all(proto::encode(&ClientMsg::Subscribe { roots: vec![root.into()] }).as_bytes())?;
    let mut line = String::new();
    reader.read_line(&mut line).context("the broker did not answer the subscription")?;
    let gaps = match proto::decode::<ServerMsg>(&line)
        .context("the broker sent an unparsable answer")?
    {
        ServerMsg::Subscribed { roots, denied, gaps } if roots.len() == 1 && denied.is_empty() => {
            gaps
        }
        ServerMsg::Subscribed { denied, .. } => {
            let why = denied
                .first()
                .map(|d| d.reason.clone())
                .unwrap_or_else(|| "no reason given".to_string());
            bail!("the broker refused {}: {why}", root.display());
        }
        other => bail!("unexpected answer from the broker: {other:?}"),
    };
    reader.get_ref().set_read_timeout(None)?;
    Ok((reader, gaps))
}

/// Reads one stream to its end, handing every event to the ingest thread as a
/// batch, and every new list of uncovered mounts to `on_gaps`. Returns when the
/// stream ends or the repository is unloaded.
fn pump(
    reader: BufReader<UnixStream>,
    root: &Path,
    internal_dir: &Path,
    repo: &Uuid,
    tx: &Sender<Vec<(FsEvent, Option<i64>)>>,
    on_gaps: &dyn Fn(&[proto::Gap]),
) -> Ended {
    let mut lines = reader.lines();
    while let Ok(Some(line)) = lines.next().transpose() {
        match proto::decode::<ServerMsg>(&line) {
            Ok(ServerMsg::Event { event }) => {
                let events = translate(root, internal_dir, &event);
                if !events.is_empty() && tx.send(events).is_err() {
                    return Ended::Unloaded;
                }
            }
            Ok(ServerMsg::Overflow {}) => {
                // Events were dropped: the kernel's queue or ours at the
                // broker overflowed — the marker does not say which. Never
                // silent: the gap is exactly as real as a disconnect.
                crate::diagnostics::error_for(
                    "watcher",
                    format!("changes were lost (a fanotify queue overflowed): {CLOSE_THE_GAP}"),
                    *repo,
                );
            }
            Ok(ServerMsg::Gaps { gaps }) => on_gaps(&gaps),
            Ok(ServerMsg::Error { message }) => {
                crate::diagnostics::warn_for("watcher", format!("broker: {message}"), *repo);
            }
            Ok(_) => {} // Subscribed: the handshake already answered it.
            Err(err) => {
                crate::diagnostics::warn_for(
                    "watcher",
                    format!("unparsable message from the broker: {err}"),
                    *repo,
                );
            }
        }
    }
    Ended::Stream
}

/// One wire event into the internal forms. The broker has already narrowed the
/// stream to this subscriber's roots and rights, so the only filtering left is
/// the daemon's own hard skip (`.metafolder/internal/`, handled by
/// [`relative`]) — a rename that lands outside it degrades to its one-sided
/// form, exactly as a move out of the watched tree does.
fn translate(root: &Path, internal_dir: &Path, event: &Event) -> Vec<(FsEvent, Option<i64>)> {
    let rel = |p: &proto::WirePath| relative(root, internal_dir, p.as_path());
    match event {
        Event::Create { path } => {
            rel(path).map(|p| vec![(FsEvent::Create(p), None)]).unwrap_or_default()
        }
        Event::Remove { path } => {
            rel(path).map(|p| vec![(FsEvent::Remove(p), None)]).unwrap_or_default()
        }
        Event::ModifyData { path } => {
            rel(path).map(|p| vec![(FsEvent::ModifyData(p), None)]).unwrap_or_default()
        }
        Event::ModifyMeta { path } => {
            rel(path).map(|p| vec![(FsEvent::ModifyMeta(p), None)]).unwrap_or_default()
        }
        Event::RenameFrom { path } => {
            rel(path).map(|p| vec![(FsEvent::RenameFrom(p), None)]).unwrap_or_default()
        }
        Event::RenameTo { path } => {
            rel(path).map(|p| vec![(FsEvent::RenameTo(p), None)]).unwrap_or_default()
        }
        Event::Rename { from, to } => match (rel(from), rel(to)) {
            (Some(a), Some(b)) => vec![(FsEvent::Rename(a, b), None)],
            (Some(a), None) => vec![(FsEvent::RenameFrom(a), None)],
            (None, Some(b)) => vec![(FsEvent::RenameTo(b), None)],
            (None, None) => vec![],
        },
    }
}

impl crate::watcher::Source for Source {
    fn name(&self) -> &'static str {
        "fanotify"
    }

    fn regime(&self) -> Regime {
        Regime::Coverage
    }

    fn refresh(
        &self,
        conn: &dyn crate::store::Store,
        cache: &TreeCache,
        root: &Path,
        internal_dir: &Path,
        cap: Option<usize>,
    ) -> Placement {
        // Coverage is the kernel's: one mark per filesystem holds the tree,
        // whatever eligibility says — `mfr_watch_exceeded` is honoured by the
        // event filter instead (doc "Watch sources and regimes"). What is left
        // to place is inside the mounts the broker cannot cover.
        crate::watcher::Source::refresh(
            &*self.uncovered.inotify,
            conn,
            cache,
            root,
            internal_dir,
            cap,
        )
    }

    fn watched(&self) -> usize {
        // Only the watches inside uncovered mounts are counted.
        crate::watcher::Source::watched(&*self.uncovered.inotify)
    }

    fn watched_set(&self) -> HashSet<PathBuf> {
        // The watches inside uncovered mounts; everywhere else
        // `explain_watched` asks the *reasons* a path could be uncovered.
        crate::watcher::Source::watched_set(&*self.uncovered.inotify)
    }

    fn remounted(&self, points: &[PathBuf]) {
        crate::watcher::Source::remounted(&*self.uncovered.inotify, points)
    }

    fn gaps(&self) -> Vec<Gap> {
        self.uncovered.gaps.lock_recover().clone()
    }

    fn maintain(
        &self,
        repo: &RepoState,
        root: &Path,
        internal_dir: &Path,
        events: &[(FsEvent, Option<i64>)],
    ) {
        // Coverage does not follow the tree's shape; the watches inside the
        // uncovered mounts do.
        crate::watcher::Source::maintain(&*self.uncovered.inotify, repo, root, internal_dir, events)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use std::sync::mpsc;

    /// A stand-in broker: accepts one connection, answers the handshake, then
    /// sends scripted messages.
    struct FakeBroker {
        socket: PathBuf,
    }

    impl FakeBroker {
        fn start(script: Vec<ServerMsg>) -> Self {
            let socket = std::env::temp_dir()
                .join("metafolder-tests")
                .join(format!("mf_fake_watchd_{}.sock", Uuid::new_v4()));
            std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
            let _ = std::fs::remove_file(&socket);
            let listener = UnixListener::bind(&socket).unwrap();
            std::thread::spawn(move || {
                if let Ok((mut stream, _)) = listener.accept() {
                    // Read the subscription, then answer it.
                    let mut line = String::new();
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    reader.read_line(&mut line).unwrap();
                    let sub: ClientMsg = proto::decode(&line).unwrap();
                    let roots = match sub {
                        ClientMsg::Subscribe { roots } => roots,
                    };
                    stream
                        .write_all(
                            proto::encode(&ServerMsg::Subscribed {
                                roots,
                                denied: Vec::new(),
                                gaps: Vec::new(),
                            })
                            .as_bytes(),
                        )
                        .unwrap();
                    for msg in &script {
                        stream.write_all(proto::encode(msg).as_bytes()).unwrap();
                    }
                    // Keep the stream open until the test is done with it.
                    std::thread::sleep(Duration::from_secs(2));
                }
            });
            Self { socket }
        }
    }

    impl Drop for FakeBroker {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.socket);
        }
    }

    /// A `RepoState` is more than a translation test needs — `translate` is
    /// pure and the thread that feeds it is exercised through the channel.
    fn events_from(script: impl FnOnce(&Path) -> Vec<ServerMsg>) -> Vec<(FsEvent, Option<i64>)> {
        let root = std::env::temp_dir()
            .join("metafolder-tests")
            .join(format!("mf_fake_repo_{}", Uuid::new_v4()));
        std::fs::create_dir_all(root.join("dir")).unwrap();
        let root = std::fs::canonicalize(&root).unwrap();
        let internal = root.join(".metafolder").join("internal");

        // The handshake, then the scripted stream, go through the client's own
        // pieces — driven here without a repository.
        let broker = FakeBroker::start(script(&root));
        let (reader, _) = subscribe(&broker.socket, &root).unwrap();
        let (tx, rx) = mpsc::channel();
        {
            let root = root.clone();
            let internal = internal.clone();
            std::thread::spawn(move || {
                pump(reader, &root, &internal, &Uuid::nil(), &tx, &|_| {});
            });
        }
        let mut out = Vec::new();
        while let Ok(batch) = rx.recv_timeout(Duration::from_secs(2)) {
            out.extend(batch);
        }
        std::fs::remove_dir_all(&root).ok();
        out
    }

    /// A path under the fake root, spelled the way the broker sends them.
    fn at(root: &Path, rest: &str) -> proto::WirePath {
        root.join(rest).into()
    }

    #[test]
    fn test_the_wire_events_become_the_internal_ones() {
        let got = events_from(|root| {
            vec![
                ServerMsg::Event { event: Event::Create { path: at(root, "dir/x") } },
                ServerMsg::Event {
                    event: Event::Rename { from: at(root, "dir/a"), to: at(root, "dir/b") },
                },
                ServerMsg::Event { event: Event::ModifyData { path: at(root, "dir/x") } },
            ]
        });
        let names: Vec<String> = got
            .iter()
            .map(|(ev, _)| match ev {
                FsEvent::Create(p) => format!("create {}", p.display()),
                FsEvent::Rename(a, b) => format!("rename {} -> {}", a.display(), b.display()),
                FsEvent::ModifyData(p) => format!("data {}", p.display()),
                other => format!("{other:?}"),
            })
            .collect();
        assert_eq!(names.len(), 3, "{names:?}");
        assert!(names[0].starts_with("create /dir/x"), "{names:?}");
        assert!(names[1].starts_with("rename /dir/a -> /dir/b"), "{names:?}");
        assert!(names[2].starts_with("data /dir/x"), "{names:?}");
    }

    #[test]
    fn test_the_internal_directory_is_skipped_like_every_source_skips_it() {
        let got = events_from(|root| {
            vec![
                ServerMsg::Event {
                    event: Event::Create { path: at(root, ".metafolder/internal/db") },
                },
                ServerMsg::Event {
                    event: Event::Rename {
                        from: at(root, ".metafolder/internal/db"),
                        to: at(root, "dir/db"),
                    },
                },
            ]
        });
        // The first is dropped; the second is an arrival from nowhere (the
        // source side is the daemon's own write, never named).
        assert_eq!(got.len(), 1);
        assert!(matches!(got[0].0, FsEvent::RenameTo(_)), "{:?}", got[0].0);
    }

    #[test]
    fn test_a_non_utf8_name_arrives_with_its_exact_bytes() {
        use std::os::unix::ffi::OsStrExt;
        let got = events_from(|root| {
            let path = root.join("dir").join(std::ffi::OsStr::from_bytes(b"caf\xE9"));
            vec![ServerMsg::Event { event: Event::Create { path: path.into() } }]
        });
        assert_eq!(got.len(), 1, "{got:?}");
        let FsEvent::Create(p) = &got[0].0 else { panic!("{:?}", got[0].0) };
        assert_eq!(p.name().unwrap().as_bytes(), b"caf\xE9");
    }

    #[test]
    fn test_an_overflow_is_announced_never_swallowed() {
        // The message itself is what the daemon turns into a diagnostic; the
        // translation of an overflow is "no events", and the notice above it
        // is what keeps it from being silent.
        let got = events_from(|root| {
            vec![
                ServerMsg::Overflow {},
                ServerMsg::Event { event: Event::Create { path: at(root, "dir/x") } },
            ]
        });
        assert_eq!(got.len(), 1, "the overflow contributes no events, the next one does");
    }
}
