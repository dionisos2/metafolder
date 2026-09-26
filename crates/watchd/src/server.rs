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

use anyhow::Result;

use crate::filter::{AccessFilter, CredSource, Subscriber};
use crate::proto::{self, ClientMsg, Denied, Event, ServerMsg, WirePath};

/// How many messages may sit in one subscriber's queue before the broker drops
/// (announced by `Overflow`). A few hundred kilobytes at worst — the cost of
/// *not* bounding a queue is a broker that a single slow subscriber pushes
/// into unbounded memory, taking every other subscriber with it.
const CLIENT_QUEUE: usize = 4096;

/// What the server tells the event source about the *union* of the roots all
/// subscribers watch, after every subscription change. The fanotify side uses
/// it to place and lift its mount marks.
pub trait RootSink: Send + Sync {
    fn set_roots(&self, roots: Vec<PathBuf>) -> Result<()>;
}

/// A sink that keeps nothing — for tests, and for a broker fed synthetically.
pub struct NoRoots;

impl RootSink for NoRoots {
    fn set_roots(&self, _roots: Vec<PathBuf>) -> Result<()> {
        Ok(())
    }
}

struct Client {
    id: u64,
    peer: Subscriber,
    /// The roots this subscriber may see (absolute, canonical). Read by the
    /// broadcast, written by the subscriber's own reader thread.
    roots: Mutex<Vec<PathBuf>>,
    tx: SyncSender<ServerMsg>,
    /// Set when a message was dropped for this subscriber; the writer thread
    /// turns it into an in-order `Overflow` marker before the next message.
    overflow: AtomicBool,
}

struct Inner<C: CredSource> {
    filter: Mutex<AccessFilter<C>>,
    sink: Arc<dyn RootSink>,
    clients: Mutex<Vec<Arc<Client>>>,
    next_id: AtomicU64,
    queue_cap: usize,
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
        Self::with_queue_cap(filter, sink, CLIENT_QUEUE)
    }

    /// The same broker with a smaller per-subscriber queue — what the tests
    /// use to provoke an overflow without writing megabytes.
    pub fn with_queue_cap(
        filter: AccessFilter<C>,
        sink: Arc<dyn RootSink>,
        queue_cap: usize,
    ) -> Self {
        Self {
            inner: Arc::new(Inner {
                filter: Mutex::new(filter),
                sink,
                clients: Mutex::new(Vec::new()),
                next_id: AtomicU64::new(1),
                queue_cap,
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
    pub fn attach(&self, stream: UnixStream, peer: Subscriber) {
        let (tx, rx) = std::sync::mpsc::sync_channel(self.inner.queue_cap);
        let client = Arc::new(Client {
            id: self.inner.next_id.fetch_add(1, Ordering::Relaxed),
            peer,
            roots: Mutex::new(Vec::new()),
            tx,
            overflow: AtomicBool::new(false),
        });
        lock(&self.inner.clients).push(Arc::clone(&client));

        let (read_half, write_half) = match stream.try_clone() {
            Ok(w) => (stream, w),
            Err(err) => {
                eprintln!("[watchd] could not split a connection: {err}");
                self.drop_client(client.id);
                return;
            }
        };

        // Writer: the overflow marker is emitted *in order*, before the first
        // message that follows the gap, so the subscriber never has to guess
        // where the gap was.
        {
            let inner = Arc::clone(&self.inner);
            let client = Arc::clone(&client);
            std::thread::spawn(move || {
                let mut out = write_half;
                for msg in rx {
                    if client.overflow.swap(false, Ordering::Relaxed)
                        && write_msg(&mut out, &ServerMsg::Overflow {}).is_err()
                    {
                        break;
                    }
                    if write_msg(&mut out, &msg).is_err() {
                        break;
                    }
                }
                // Either side going away ends this subscriber.
                drop_client(&inner, client.id);
            });
        }

        // Reader: `Subscribe` is the only input, and it is answered in order
        // through the same queue as the events.
        {
            let inner = Arc::clone(&self.inner);
            let client = Arc::clone(&client);
            std::thread::spawn(move || {
                let mut lines = BufReader::new(read_half).lines();
                while let Ok(Some(line)) = lines.next().transpose() {
                    match proto::decode::<ClientMsg>(&line) {
                        Ok(ClientMsg::Subscribe { roots }) => {
                            apply_subscription(&inner, &client, roots);
                        }
                        Err(err) => {
                            let _ = client.tx.try_send(ServerMsg::Error {
                                message: format!("unparsable message: {err}"),
                            });
                        }
                    }
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
                let mut visible = |p: &std::path::Path| filter.may_see(&client.peer, p);
                event.adapt(&roots, &mut visible)
            };
            let Some(adapted) = adapted else { continue };
            if let Err(TrySendError::Full(_)) =
                client.tx.try_send(ServerMsg::Event { event: adapted })
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

    fn drop_client(&self, id: u64) {
        drop_client(&self.inner, id);
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
        if let Err(err) = inner.sink.set_roots(roots) {
            eprintln!("[watchd] could not update the marks: {err:#}");
        }
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
            let canonical =
                filter.real_path(root.as_path()).filter(|p| filter.may_watch(&client.peer, p));
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
    if let Err(err) = inner.sink.set_roots(union) {
        let _ = client.tx.try_send(ServerMsg::Error { message: format!("{err:#}") });
    }
    let _ = client.tx.try_send(ServerMsg::Subscribed {
        roots: allowed.iter().map(|p| WirePath::from(p.as_path())).collect(),
        denied,
    });
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
        fn set_roots(&self, roots: Vec<PathBuf>) -> Result<()> {
            self.seen.lock().unwrap().push(roots);
            Ok(())
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
}
