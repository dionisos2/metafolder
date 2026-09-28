//! The subscriber-facing half of the broker (docs/watcher-fanotify.md "The
//! broker"): a Unix socket, one *bounded* queue per subscriber, and a broadcast
//! that adapts and filters each event per subscriber before queueing it.
//!
//! The bounding is the design: a subscriber that cannot keep up **loses**
//! events rather than making the whole machine wait. What it loses is never
//! silent — the overflow is announced in order (`ServerMsg::Overflow`) and the
//! subscriber recovers with a reconcile, exactly as a daemon that was down
//! does. The kernel side overflows the same way (`FAN_Q_OVERFLOW`), and
//! [`Broker::broadcast_overflow`] announces that to everyone.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex, MutexGuard};

use crate::filter::{AccessFilter, CredSource, Subscriber};
use crate::proto::{self, ClientMsg, Denied, Event, ServerMsg, WirePath};

/// How many messages may sit in one subscriber's queue before the broker drops
/// (announced by `Overflow`). A few hundred kilobytes at worst — the cost of
/// *not* bounding a queue is a broker that a single slow subscriber pushes
/// into unbounded memory, taking every other subscriber with it.
const CLIENT_QUEUE: usize = 4096;

/// How much of the broker one connection, and one user, may occupy. The
/// socket is world-connectable (the gate is the per-uid filter), so every
/// resource a peer can make the broker hold is bounded here.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Messages queued per subscriber before it is dropped from (`Overflow`).
    pub queue_cap: usize,
    /// Connections one uid may hold: another user's daemon can never be
    /// starved out by a flood of connections.
    pub per_uid: usize,
    /// Connections in all: two threads each, on a privileged process.
    pub total: usize,
}

impl Default for Limits {
    fn default() -> Self {
        // A daemon holds one connection per loaded repository.
        Limits { queue_cap: CLIENT_QUEUE, per_uid: 256, total: 1024 }
    }
}

/// The longest message a subscriber may send: a `Subscribe` naming its roots.
/// Past it the connection is dropped — a line is buffered whole before it is
/// parsed, and nobody may make a privileged process buffer without end.
const MAX_LINE: usize = 256 * 1024;

/// What the server tells the event source about the *union* of the roots all
/// subscribers watch, after every subscription change. The fanotify side uses
/// it to place and lift its filesystem marks.
pub trait RootSink: Send + Sync {
    /// Covers `roots`, and answers the ones it could not — every other root is
    /// covered. One root failing must not fail the others: a repository deleted
    /// while its daemon still subscribes is a root no mark can cover, and the
    /// next subscriber has nothing to do with it.
    fn set_roots(&self, roots: Vec<PathBuf>) -> Vec<Uncovered>;
}

/// A root the event source could not cover, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Uncovered {
    pub root: PathBuf,
    pub reason: String,
}

/// A sink that keeps nothing — for tests, and for a broker fed synthetically.
pub struct NoRoots;

impl RootSink for NoRoots {
    fn set_roots(&self, _roots: Vec<PathBuf>) -> Vec<Uncovered> {
        Vec::new()
    }
}

struct Client {
    id: u64,
    peer: Subscriber,
    /// The roots this subscriber may see (absolute, canonical). Read by the
    /// broadcast, written by the subscriber's own reader thread.
    roots: Mutex<Vec<PathBuf>>,
    tx: SyncSender<Out>,
    /// The connection itself, to hang up on the peer when the queue cannot
    /// carry the order to (it is full).
    conn: UnixStream,
    /// Set when a message was dropped for this subscriber; the writer thread
    /// turns it into an in-order `Overflow` marker before the next message.
    overflow: AtomicBool,
}

/// What a subscriber's writer thread is handed: a message to write, or the
/// order to hang up once everything queued before it is written.
enum Out {
    Msg(ServerMsg),
    Close,
}

struct Inner<C: CredSource> {
    filter: Mutex<AccessFilter<C>>,
    sink: Arc<dyn RootSink>,
    clients: Mutex<Vec<Arc<Client>>>,
    next_id: AtomicU64,
    limits: Limits,
}

/// The broker: a client registry and the two loops that serve it.
pub struct Broker<C: CredSource> {
    inner: Arc<Inner<C>>,
}

/// `std::sync::Mutex` that survives a poisoned sibling: the values behind these
/// locks are plain data, and a panic in one thread must not fail every other.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl<C: 'static + CredSource> Broker<C> {
    pub fn new(filter: AccessFilter<C>, sink: Arc<dyn RootSink>) -> Self {
        Self::with_limits(filter, sink, Limits::default())
    }

    /// The same broker with a smaller per-subscriber queue — what the tests
    /// use to provoke an overflow without writing megabytes.
    pub fn with_queue_cap(
        filter: AccessFilter<C>,
        sink: Arc<dyn RootSink>,
        queue_cap: usize,
    ) -> Self {
        Self::with_limits(filter, sink, Limits { queue_cap, ..Limits::default() })
    }

    pub fn with_limits(filter: AccessFilter<C>, sink: Arc<dyn RootSink>, limits: Limits) -> Self {
        Self {
            inner: Arc::new(Inner {
                filter: Mutex::new(filter),
                sink,
                clients: Mutex::new(Vec::new()),
                next_id: AtomicU64::new(1),
                limits,
            }),
        }
    }

    /// Accepts one connection: identifies the peer (`SO_PEERCRED`) and attaches
    /// it.
    pub fn attach_stream(&self, stream: UnixStream) {
        match peer_creds(&stream) {
            Ok((uid, gid, pid)) => {
                let peer = lock(&self.inner.filter).subscriber(uid, gid, pid);
                self.attach(stream, peer);
            }
            Err(err) => eprintln!("[watchd] rejected a connection: {err}"),
        }
    }

    /// Attaches an already-identified peer (what tests do; `attach_stream` is
    /// the thin `SO_PEERCRED` wrapper above).
    pub fn attach(&self, mut stream: UnixStream, peer: Subscriber) {
        {
            let clients = lock(&self.inner.clients);
            let mine = clients.iter().filter(|c| c.peer.uid == peer.uid).count();
            if mine >= self.inner.limits.per_uid || clients.len() >= self.inner.limits.total {
                drop(clients);
                let _ = write_msg(
                    &mut stream,
                    &ServerMsg::Error { message: "too many connections".to_string() },
                );
                return; // Dropping the stream hangs up.
            }
        }
        let (write_half, conn) = match (stream.try_clone(), stream.try_clone()) {
            (Ok(w), Ok(c)) => (w, c),
            (Err(err), _) | (_, Err(err)) => {
                eprintln!("[watchd] could not split a connection: {err}");
                return;
            }
        };
        let read_half = stream;
        let (tx, rx) = std::sync::mpsc::sync_channel(self.inner.limits.queue_cap);
        let client = Arc::new(Client {
            id: self.inner.next_id.fetch_add(1, Ordering::Relaxed),
            peer,
            roots: Mutex::new(Vec::new()),
            tx,
            conn,
            overflow: AtomicBool::new(false),
        });
        lock(&self.inner.clients).push(Arc::clone(&client));

        // Writer: the overflow marker is emitted *in order*, before the first
        // message that follows the gap, so the subscriber never has to guess
        // where the gap was.
        {
            let inner = Arc::clone(&self.inner);
            let client = Arc::clone(&client);
            std::thread::spawn(move || {
                let mut out = write_half;
                for item in rx {
                    let Out::Msg(msg) = item else { break };
                    if client.overflow.swap(false, Ordering::Relaxed)
                        && write_msg(&mut out, &ServerMsg::Overflow {}).is_err()
                    {
                        break;
                    }
                    if write_msg(&mut out, &msg).is_err() {
                        break;
                    }
                }
                // Either side going away ends this subscriber — and the
                // connection, so the reading thread's `read` returns too.
                let _ = client.conn.shutdown(std::net::Shutdown::Both);
                drop_client(&inner, client.id);
            });
        }

        // Reader: `Subscribe` is the only input, and it is answered in order
        // through the same queue as the events.
        {
            let inner = Arc::clone(&self.inner);
            let client = Arc::clone(&client);
            std::thread::spawn(move || {
                let mut input = BufReader::new(read_half);
                loop {
                    let line = match read_bounded_line(&mut input) {
                        Line::Complete(line) => line,
                        Line::End => break,
                        Line::TooLong => {
                            // Through the queue, so it is written before the
                            // hang-up below.
                            let _ = client.tx.try_send(Out::Msg(ServerMsg::Error {
                                message: format!("message too long (over {MAX_LINE} bytes)"),
                            }));
                            break;
                        }
                    };
                    match proto::decode::<ClientMsg>(&line) {
                        Ok(ClientMsg::Subscribe { roots }) => {
                            apply_subscription(&inner, &client, roots);
                        }
                        Err(err) => {
                            let _ = client.tx.try_send(Out::Msg(ServerMsg::Error {
                                message: format!("unparsable message: {err}"),
                            }));
                        }
                    }
                }
                // The writer hangs up once what is queued is written; with the
                // queue full, it cannot be told, and the connection is cut.
                if client.tx.try_send(Out::Close).is_err() {
                    let _ = client.conn.shutdown(std::net::Shutdown::Both);
                }
                drop_client(&inner, client.id);
            });
        }
    }

    /// Feeds one event to every subscriber that should see it.
    pub fn broadcast(&self, event: &Event) {
        for client in lock(&self.inner.clients).iter() {
            let roots = lock(&client.roots).clone();
            if roots.is_empty() {
                continue;
            }
            // Adapt per subscriber: inside its roots *and* discoverable with
            // its own credentials. The filter lock lives as long as the check
            // and no longer.
            let adapted = {
                let mut filter = lock(&self.inner.filter);
                let mut visible = |p: &std::path::Path| {
                    // The subscriber's innermost root holding `p`: listing
                    // is required from there down.
                    let root = roots
                        .iter()
                        .filter(|r| p.starts_with(r))
                        .max_by_key(|r| r.components().count());
                    root.is_some_and(|root| filter.may_see(&client.peer, root, p))
                };
                event.adapt(&roots, &mut visible)
            };
            let Some(adapted) = adapted else { continue };
            if let Err(TrySendError::Full(_)) =
                client.tx.try_send(Out::Msg(ServerMsg::Event { event: adapted }))
            {
                client.overflow.store(true, Ordering::Relaxed);
            }
        }
    }

    /// The kernel's own queue overflowed: everything since the last delivered
    /// event is gone for *every* subscriber.
    pub fn broadcast_overflow(&self) {
        for client in lock(&self.inner.clients).iter() {
            client.overflow.store(true, Ordering::Relaxed);
        }
    }

    /// Accepts connections and feeds events until the event channel closes.
    /// Runs the broadcast loop on the calling thread; returns when the source
    /// is gone (the broker is shutting down).
    pub fn serve(&self, listener: UnixListener, events: Receiver<Event>) {
        {
            let broker = Broker { inner: Arc::clone(&self.inner) };
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    match stream {
                        Ok(stream) => broker.attach_stream(stream),
                        Err(err) => {
                            eprintln!("[watchd] accept failed: {err}");
                            break;
                        }
                    }
                }
            });
        }
        while let Ok(event) = events.recv() {
            self.broadcast(&event);
        }
    }

    /// How many subscribers are connected (diagnostics, tests).
    pub fn client_count(&self) -> usize {
        lock(&self.inner.clients).len()
    }
}

/// Removes a subscriber and tells the event source about the roots that
/// remain. Idempotent: both of a client's threads end here.
fn drop_client<C: CredSource>(inner: &Arc<Inner<C>>, id: u64) {
    let mut clients = lock(&inner.clients);
    let before = clients.len();
    clients.retain(|c| c.id != id);
    if clients.len() != before {
        let roots = union_roots(&clients);
        drop(clients);
        report_uncovered(inner.sink.set_roots(roots));
    }
}

/// Handles one `Subscribe`: checks each root against the subscriber's own
/// credentials, records what passed, tells the event source about the new
/// union of roots, and only then answers — the `Subscribed` ack means *you are
/// covered*, so nothing may still be in flight behind it.
fn apply_subscription<C: CredSource>(
    inner: &Arc<Inner<C>>,
    client: &Arc<Client>,
    roots: Vec<WirePath>,
) {
    let mut allowed: Vec<PathBuf> = Vec::new();
    let mut denied: Vec<Denied> = Vec::new();
    {
        let mut filter = lock(&inner.filter);
        for root in &roots {
            // Stored under the kernel's own name for the path (symlinks
            // resolved): event paths come resolved too, so a root spelled with
            // a symlink in it would never prefix-match anything. A root that
            // cannot be resolved is denied like one that cannot be listed —
            // one uniform reason, which leaks neither existence nor
            // permissions.
            let canonical = filter
                .resolve(&client.peer, root.as_path())
                .filter(|p| filter.may_watch(&client.peer, p));
            match canonical {
                Some(path) => allowed.push(path),
                None => {
                    denied.push(Denied { root: root.clone(), reason: "not accessible".to_string() })
                }
            }
        }
    }
    *lock(&client.roots) = allowed.clone();
    let union = union_roots(&lock(&inner.clients));
    // What cannot be covered is this subscriber's business only when the root
    // is its own: it is refused that root, never told it is covered. Another
    // subscriber's (deleted since it subscribed, say) is logged, not sent.
    let (mine, others): (Vec<Uncovered>, Vec<Uncovered>) =
        inner.sink.set_roots(union).into_iter().partition(|u| allowed.contains(&u.root));
    report_uncovered(others);
    if !mine.is_empty() {
        allowed.retain(|root| !mine.iter().any(|u| u.root == *root));
        *lock(&client.roots) = allowed.clone();
        for u in mine {
            denied.push(Denied {
                root: WirePath::from(u.root),
                reason: format!("cannot be watched: {}", u.reason),
            });
        }
    }
    let _ = client.tx.try_send(Out::Msg(ServerMsg::Subscribed {
        roots: allowed.iter().map(|p| WirePath::from(p.as_path())).collect(),
        denied,
    }));
}

/// Roots no mark covers that no subscriber is being answered about: said in
/// the broker's own log.
fn report_uncovered(uncovered: Vec<Uncovered>) {
    for u in uncovered {
        eprintln!("[watchd] cannot cover {}: {}", u.root.display(), u.reason);
    }
}

fn union_roots(clients: &[Arc<Client>]) -> Vec<PathBuf> {
    let mut union: Vec<PathBuf> = Vec::new();
    for client in clients {
        for root in lock(&client.roots).iter() {
            if !union.contains(root) {
                union.push(root.clone());
            }
        }
    }
    union
}

enum Line {
    Complete(String),
    End,
    TooLong,
}

/// One newline-terminated line of at most [`MAX_LINE`] bytes. `lines()` would
/// buffer a line without end for as long as the peer keeps sending.
fn read_bounded_line(input: &mut BufReader<UnixStream>) -> Line {
    use std::io::Read;
    let mut buf = Vec::new();
    match input.by_ref().take(MAX_LINE as u64 + 1).read_until(b'\n', &mut buf) {
        Ok(0) | Err(_) => Line::End,
        Ok(_) if buf.last() == Some(&b'\n') => {
            buf.pop();
            Line::Complete(String::from_utf8_lossy(&buf).into_owned())
        }
        Ok(_) if buf.len() > MAX_LINE => Line::TooLong,
        Ok(_) => Line::End, // The peer hung up mid-line.
    }
}

fn write_msg(out: &mut UnixStream, msg: &ServerMsg) -> std::io::Result<()> {
    out.write_all(proto::encode(msg).as_bytes())
}

/// The peer of a connection, straight from the kernel (`SO_PEERCRED`): uid and
/// gid are all the socket gives — the group list comes from
/// [`AccessFilter::subscriber`].
fn peer_creds(stream: &UnixStream) -> std::io::Result<(u32, u32, i32)> {
    use std::os::unix::io::AsRawFd;
    let mut cred = libc::ucred { pid: 0, uid: 0, gid: 0 };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut libc::ucred as *mut libc::c_void,
            &mut len,
        )
    };
    if rc != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((cred.uid, cred.gid, cred.pid))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::filter::CredSource;
    use std::path::Path;

    struct AllowAll;
    impl CredSource for AllowAll {
        fn dir_meta(&self, _path: &Path) -> Option<(u32, u32, u32)> {
            Some((0, 0, 0o777))
        }
        fn groups_of(&self, _pid: i32) -> Vec<u32> {
            Vec::new()
        }
    }

    /// Records the roots the broker asked the event source to cover.
    struct RecordingSink {
        seen: std::sync::Mutex<Vec<Vec<PathBuf>>>,
    }
    impl RootSink for RecordingSink {
        fn set_roots(&self, roots: Vec<PathBuf>) -> Vec<Uncovered> {
            self.seen.lock().unwrap().push(roots);
            Vec::new()
        }
    }

    fn reader(stream: &UnixStream) -> BufReader<UnixStream> {
        BufReader::new(stream.try_clone().unwrap())
    }

    fn next_msg(r: &mut BufReader<UnixStream>) -> ServerMsg {
        let mut line = String::new();
        r.read_line(&mut line).unwrap();
        proto::decode(&line).unwrap()
    }

    fn subscribe(mut stream: &UnixStream, roots: &[&str]) {
        let msg = ClientMsg::Subscribe { roots: roots.iter().map(|&r| r.into()).collect() };
        stream.write_all(proto::encode(&msg).as_bytes()).unwrap();
    }

    fn attach_pair<C: 'static + CredSource>(broker: &Broker<C>) -> (UnixStream, UnixStream) {
        let (client, server) = UnixStream::pair().unwrap();
        let peer = Subscriber { uid: 1000, gid: 1000, pid: 42, groups: vec![1000] };
        broker.attach(server, peer);
        let writer = client.try_clone().unwrap();
        (client, writer)
    }

    #[test]
    fn test_a_subscriber_gets_the_events_under_its_roots_only() {
        let broker = Broker::new(AccessFilter::new(AllowAll), Arc::new(NoRoots));
        let (client, _writer) = attach_pair(&broker);
        let mut r = reader(&client);
        subscribe(&client, &["/repo"]);
        let sub = next_msg(&mut r);
        assert!(matches!(sub, ServerMsg::Subscribed { .. }), "{sub:?}");

        broker.broadcast(&Event::Create { path: "/repo/x".into() });
        broker.broadcast(&Event::Create { path: "/elsewhere/x".into() });
        let got = next_msg(&mut r);
        assert_eq!(got, ServerMsg::Event { event: Event::Create { path: "/repo/x".into() } });
    }

    #[test]
    fn test_a_rename_degrades_per_subscriber() {
        let broker = Broker::new(AccessFilter::new(AllowAll), Arc::new(NoRoots));
        let (client, _writer) = attach_pair(&broker);
        let mut r = reader(&client);
        subscribe(&client, &["/repo"]);
        let _ = next_msg(&mut r);

        // Out of the repository: the subscriber is told the file left, and
        // nothing about where it went (the destination is outside its roots).
        broker.broadcast(&Event::Rename { from: "/repo/a".into(), to: "/mount/b".into() });
        assert_eq!(
            next_msg(&mut r),
            ServerMsg::Event { event: Event::RenameFrom { path: "/repo/a".into() } }
        );
    }

    #[test]
    fn test_a_second_subscribe_replaces_the_roots() {
        let sink = Arc::new(RecordingSink { seen: std::sync::Mutex::new(Vec::new()) });
        let broker =
            Broker::new(AccessFilter::new(AllowAll), Arc::clone(&sink) as Arc<dyn RootSink>);
        let (client, _writer) = attach_pair(&broker);
        let mut r = reader(&client);

        subscribe(&client, &["/repo"]);
        let _ = next_msg(&mut r);
        subscribe(&client, &["/other"]);
        let _ = next_msg(&mut r);

        broker.broadcast(&Event::Remove { path: "/repo/x".into() });
        broker.broadcast(&Event::Remove { path: "/other/x".into() });
        // /repo is no longer watched by anyone: the sink saw the union shrink.
        assert_eq!(
            *sink.seen.lock().unwrap(),
            vec![vec![PathBuf::from("/repo")], vec![PathBuf::from("/other")]]
        );
        assert_eq!(
            next_msg(&mut r),
            ServerMsg::Event { event: Event::Remove { path: "/other/x".into() } }
        );
    }

    #[test]
    fn test_an_inaccessible_root_is_refused_with_one_uniform_reason() {
        struct OnlyRoot;
        impl CredSource for OnlyRoot {
            fn dir_meta(&self, path: &Path) -> Option<(u32, u32, u32)> {
                match path {
                    p if p == Path::new("/") => Some((0, 0, 0o755)),
                    p if p == Path::new("/mine") => Some((1000, 1000, 0o755)),
                    _ => Some((0, 0, 0o700)), // exists, but not for this uid
                }
            }
            fn groups_of(&self, _pid: i32) -> Vec<u32> {
                Vec::new()
            }
        }
        let broker = Broker::new(AccessFilter::new(OnlyRoot), Arc::new(NoRoots));
        let (client, _writer) = attach_pair(&broker);
        let mut r = reader(&client);
        subscribe(&client, &["/mine", "/root-owned", "/gone"]);

        match next_msg(&mut r) {
            ServerMsg::Subscribed { roots, denied } => {
                assert_eq!(roots, vec!["/mine".into()]);
                // The reason is identical whether the root exists or not.
                assert_eq!(denied.len(), 2);
                assert!(denied.iter().all(|d| d.reason == "not accessible"));
            }
            other => panic!("expected Subscribed, got {other:?}"),
        }
    }

    #[test]
    fn test_a_slow_subscriber_loses_events_and_is_told_in_order() {
        // A queue of two, and a client that does not read until the broker has
        // given up on it: the socket buffer absorbs what it can, then the
        // queue fills, then events are dropped — with an in-order marker.
        let broker = Broker::with_queue_cap(AccessFilter::new(AllowAll), Arc::new(NoRoots), 2);
        let (client, _writer) = attach_pair(&broker);
        let mut r = reader(&client);
        subscribe(&client, &["/repo"]);
        let _ = next_msg(&mut r);

        for i in 0..100_000 {
            broker.broadcast(&Event::ModifyData { path: format!("/repo/{i}").into() });
        }

        // Drain: the marker must appear, and events must follow it.
        let mut saw_overflow = false;
        let mut after = 0;
        for _ in 0..200_000 {
            match next_msg(&mut r) {
                ServerMsg::Overflow {} => {
                    // One marker per drop batch: a second one is legitimate
                    // and closes its own gap.
                    saw_overflow = true;
                }
                ServerMsg::Event { .. } => {
                    if saw_overflow {
                        after += 1;
                        if after >= 2 {
                            return;
                        }
                    }
                }
                other => panic!("unexpected message: {other:?}"),
            }
        }
        panic!("no overflow marker in the stream");
    }

    /// The socket is world-connectable: whatever any local user sends, the
    /// privileged broker's memory must not follow it.
    #[test]
    fn test_a_line_without_end_is_cut_off_not_buffered() {
        let broker = Broker::new(AccessFilter::new(AllowAll), Arc::new(NoRoots));
        let (client, mut writer) = attach_pair(&broker);
        client.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        let chunk = vec![b'x'; 64 * 1024];
        // Far past any sane message; the broker may hang up half-way (EPIPE).
        for _ in 0..64 {
            if writer.write_all(&chunk).is_err() {
                break;
            }
        }
        // Told why, then hung up on.
        let mut r = reader(&client);
        let mut rest = String::new();
        let n = r.read_line(&mut rest).expect("the broker answers instead of waiting for more");
        assert!(n > 0 && rest.contains("too long"), "{rest:?}");
        rest.clear();
        assert_eq!(r.read_line(&mut rest).unwrap_or(0), 0, "the connection must be closed");
    }

    /// A peer that stops talking is let go *entirely*: both of its threads
    /// end and the connection is closed, instead of a writer waiting for ever
    /// on a queue nothing feeds any more — one leaked thread per connection,
    /// which any local user could repeat until the broker runs out.
    #[test]
    fn test_a_peer_that_hangs_up_is_let_go() {
        let broker = Broker::new(AccessFilter::new(AllowAll), Arc::new(NoRoots));
        let (client, _writer) = attach_pair(&broker);
        client.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        let mut rest = String::new();
        let n = reader(&client).read_line(&mut rest).expect("closed, not left hanging");
        assert_eq!(n, 0, "{rest:?}");
        // Unregistered by its threads, which may finish just after the close.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while broker.client_count() != 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(broker.client_count(), 0);
    }

    #[test]
    fn test_one_user_cannot_take_every_connection() {
        let broker = Broker::with_limits(
            AccessFilter::new(AllowAll),
            Arc::new(NoRoots),
            Limits { queue_cap: 64, per_uid: 2, total: 3 },
        );
        let connect = |uid: u32| {
            let (client, server) = UnixStream::pair().unwrap();
            broker.attach(server, Subscriber { uid, gid: uid, pid: 1, groups: vec![uid] });
            client
        };
        let _a = connect(1000);
        let _b = connect(1000);
        let refused = connect(1000);
        refused.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        let mut r = reader(&refused);
        let mut line = String::new();
        r.read_line(&mut line).unwrap();
        assert!(line.contains("too many connections"), "{line:?}");
        // Another user still gets in — up to the machine-wide ceiling.
        let _c = connect(1001);
        assert_eq!(broker.client_count(), 3);
        let _d = connect(1002);
        assert_eq!(broker.client_count(), 3);
    }

    #[test]
    fn test_the_kernel_overflow_reaches_every_subscriber() {
        let broker = Broker::with_queue_cap(AccessFilter::new(AllowAll), Arc::new(NoRoots), 64);
        let (client, _writer) = attach_pair(&broker);
        let mut r = reader(&client);
        subscribe(&client, &["/repo"]);
        let _ = next_msg(&mut r);

        broker.broadcast_overflow();
        broker.broadcast(&Event::Create { path: "/repo/x".into() });
        assert!(matches!(next_msg(&mut r), ServerMsg::Overflow {}));
        assert!(matches!(next_msg(&mut r), ServerMsg::Event { .. }));
    }

    /// A sink that cannot cover the roots under `/gone`.
    struct FailingSink;
    impl RootSink for FailingSink {
        fn set_roots(&self, roots: Vec<PathBuf>) -> Vec<Uncovered> {
            roots
                .into_iter()
                .filter(|r| r.starts_with("/gone"))
                .map(|root| Uncovered { root, reason: "no mark for you".to_string() })
                .collect()
        }
    }

    fn wait_for_no_client<C: 'static + CredSource>(broker: &Broker<C>) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while broker.client_count() != 0 && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(broker.client_count(), 0);
    }

    #[test]
    fn test_a_subscriber_without_roots_yet_is_sent_nothing() {
        let broker = Broker::new(AccessFilter::new(AllowAll), Arc::new(NoRoots));
        let (client, _writer) = attach_pair(&broker);
        let mut r = reader(&client);
        broker.broadcast(&Event::Create { path: "/repo/before".into() });
        subscribe(&client, &["/repo"]);
        let _ = next_msg(&mut r);
        broker.broadcast(&Event::Create { path: "/repo/after".into() });
        assert_eq!(
            next_msg(&mut r),
            ServerMsg::Event { event: Event::Create { path: "/repo/after".into() } }
        );
    }

    #[test]
    fn test_an_unparsable_message_is_answered_and_the_connection_kept() {
        let broker = Broker::new(AccessFilter::new(AllowAll), Arc::new(NoRoots));
        let (mut client, _writer) = attach_pair(&broker);
        let mut r = reader(&client);
        client.write_all(b"not json\n").unwrap();
        let ServerMsg::Error { message } = next_msg(&mut r) else { panic!("an error") };
        assert!(message.starts_with("unparsable message"), "{message}");
        subscribe(&client, &["/repo"]);
        assert!(matches!(next_msg(&mut r), ServerMsg::Subscribed { .. }));
    }

    #[test]
    fn test_a_peer_hanging_up_mid_line_is_let_go() {
        let broker = Broker::new(AccessFilter::new(AllowAll), Arc::new(NoRoots));
        let (mut client, _writer) = attach_pair(&broker);
        client.write_all(b"{\"type\":\"subscr").unwrap();
        client.shutdown(std::net::Shutdown::Write).unwrap();
        client.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        let mut rest = String::new();
        assert_eq!(reader(&client).read_line(&mut rest).unwrap(), 0, "{rest:?}");
        wait_for_no_client(&broker);
    }

    #[test]
    fn test_a_root_no_mark_can_cover_is_refused_to_its_subscriber() {
        let broker = Broker::new(AccessFilter::new(AllowAll), Arc::new(FailingSink));
        let (client, _writer) = attach_pair(&broker);
        let mut r = reader(&client);
        subscribe(&client, &["/gone/repo", "/repo"]);
        let ServerMsg::Subscribed { roots, denied } = next_msg(&mut r) else {
            panic!("the answer, and nothing before it")
        };
        assert_eq!(roots, vec!["/repo".into()], "the other root is covered");
        assert_eq!(denied.len(), 1);
        assert_eq!(denied[0].root, "/gone/repo".into());
        assert_eq!(denied[0].reason, "cannot be watched: no mark for you");
        // Refused, so out of its roots: nothing is sent from there.
        broker.broadcast(&Event::Create { path: "/gone/repo/x".into() });
        broker.broadcast(&Event::Create { path: "/repo/y".into() });
        assert_eq!(
            next_msg(&mut r),
            ServerMsg::Event { event: Event::Create { path: "/repo/y".into() } }
        );
        // Leaving runs the sink again (failing again): logged, and let go.
        client.shutdown(std::net::Shutdown::Both).unwrap();
        wait_for_no_client(&broker);
    }

    /// A sink that stops covering a root once it is declared gone.
    #[derive(Default)]
    struct VanishingSink {
        gone: std::sync::Mutex<Vec<PathBuf>>,
    }
    impl RootSink for VanishingSink {
        fn set_roots(&self, roots: Vec<PathBuf>) -> Vec<Uncovered> {
            let gone = self.gone.lock().unwrap();
            roots
                .into_iter()
                .filter(|r| gone.contains(r))
                .map(|root| Uncovered { root, reason: "no such directory".to_string() })
                .collect()
        }
    }

    #[test]
    fn test_a_root_of_another_that_cannot_be_covered_is_not_this_subscribers_business() {
        // A repository deleted while its daemon subscribes: its root can no
        // longer be covered. The next subscriber's answer is its own, and
        // nothing comes before it.
        let sink = Arc::new(VanishingSink::default());
        let broker = Broker::new(AccessFilter::new(AllowAll), Arc::clone(&sink) as _);
        let (first, _w1) = attach_pair(&broker);
        let mut r1 = reader(&first);
        subscribe(&first, &["/theirs"]);
        assert!(
            matches!(next_msg(&mut r1), ServerMsg::Subscribed { denied, .. } if denied.is_empty())
        );
        sink.gone.lock().unwrap().push(PathBuf::from("/theirs"));
        let (second, _w2) = attach_pair(&broker);
        let mut r2 = reader(&second);
        subscribe(&second, &["/mine"]);
        assert_eq!(
            next_msg(&mut r2),
            ServerMsg::Subscribed { roots: vec!["/mine".into()], denied: vec![] }
        );
    }

    #[test]
    fn test_a_connection_whose_peer_cannot_be_identified_is_refused() {
        // SO_PEERCRED on something that is no socket: ENOTSOCK.
        use std::os::unix::io::{FromRawFd, IntoRawFd};
        let fd = std::fs::File::open("/dev/null").unwrap().into_raw_fd();
        let not_a_socket = unsafe { UnixStream::from_raw_fd(fd) };
        assert!(peer_creds(&not_a_socket).is_err());
        let broker = Broker::new(AccessFilter::new(AllowAll), Arc::new(NoRoots));
        broker.attach_stream(not_a_socket);
        assert_eq!(broker.client_count(), 0);
    }

    #[test]
    fn test_a_real_connection_is_identified_by_its_credentials() {
        let broker = Broker::new(AccessFilter::new(AllowAll), Arc::new(NoRoots));
        let (client, server) = UnixStream::pair().unwrap();
        assert_eq!(
            peer_creds(&server).unwrap(),
            (unsafe { libc::geteuid() }, unsafe { libc::getegid() }, std::process::id() as i32)
        );
        broker.attach_stream(server);
        assert_eq!(broker.client_count(), 1);
        drop(client);
        wait_for_no_client(&broker);
    }

    #[test]
    fn test_a_peer_that_stops_reading_while_an_overflow_is_pending_is_let_go() {
        // The in-order overflow marker is the write that fails.
        let broker = Broker::new(AccessFilter::new(AllowAll), Arc::new(NoRoots));
        let (client, _writer) = attach_pair(&broker);
        let mut r = reader(&client);
        subscribe(&client, &["/repo"]);
        let _ = next_msg(&mut r);
        client.shutdown(std::net::Shutdown::Read).unwrap();
        broker.broadcast_overflow();
        broker.broadcast(&Event::Create { path: "/repo/x".into() });
        wait_for_no_client(&broker);
    }

    #[test]
    fn test_a_peer_that_stops_reading_is_let_go_at_the_next_message() {
        let broker = Broker::new(AccessFilter::new(AllowAll), Arc::new(NoRoots));
        let (client, _writer) = attach_pair(&broker);
        let mut r = reader(&client);
        subscribe(&client, &["/repo"]);
        let _ = next_msg(&mut r);
        client.shutdown(std::net::Shutdown::Read).unwrap();
        broker.broadcast(&Event::Create { path: "/repo/x".into() });
        wait_for_no_client(&broker);
    }

    #[test]
    fn test_a_listener_that_fails_stops_accepting() {
        use std::os::unix::io::AsRawFd;
        let dir = std::env::temp_dir()
            .join("metafolder-tests")
            .join(format!("watchd-accept-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("s.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let fd = listener.as_raw_fd();
        let (tx, rx) = std::sync::mpsc::channel();
        let broker = Broker::new(AccessFilter::new(AllowAll), Arc::new(NoRoots));
        let serving = std::thread::spawn(move || broker.serve(listener, rx));
        // A live listener answers…
        drop(UnixStream::connect(&path).unwrap());
        // …until accept fails: the loop logs it and stops, and the listener
        // goes with it.
        unsafe { libc::shutdown(fd, libc::SHUT_RDWR) };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while UnixStream::connect(&path).is_ok() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(UnixStream::connect(&path).is_err(), "the listener is gone");
        drop(tx); // The event source ends: serve returns.
        serving.join().unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_a_connection_that_cannot_be_split_is_dropped() {
        // No descriptor left for the writer's half (EMFILE): the connection is
        // hung up rather than half served.
        if !crate::test_support::in_child(
            "server::tests::test_a_connection_that_cannot_be_split_is_dropped",
        ) {
            return;
        }
        let broker = Broker::new(AccessFilter::new(AllowAll), Arc::new(NoRoots));
        let (client, server) = UnixStream::pair().unwrap();
        let peer = Subscriber { uid: 1000, gid: 1000, pid: 42, groups: vec![1000] };
        // The lowest free descriptor becomes the limit: the next one fails.
        let probe = unsafe { libc::dup(0) };
        unsafe { libc::close(probe) };
        let mut old = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut old) };
        let low = libc::rlimit { rlim_cur: probe as libc::rlim_t, rlim_max: old.rlim_max };
        unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &low) };
        broker.attach(server, peer);
        unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &old) };
        assert_eq!(broker.client_count(), 0);
        client.set_read_timeout(Some(std::time::Duration::from_secs(5))).unwrap();
        let mut rest = String::new();
        assert_eq!(reader(&client).read_line(&mut rest).unwrap(), 0, "hung up");
    }
}
