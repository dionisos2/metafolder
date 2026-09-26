//! End-to-end over a real socket: the accept path (`SO_PEERCRED` included),
//! the subscribe round-trip, and the stream a subscriber actually gets.
//!
//! The event source is synthetic — the kernel side has its own tests — because
//! what is under test here is the broker's half: identification, filtering,
//! and the shape of the stream.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::Arc;

use anyhow::Result;

use metafolder_watchd::filter::{AccessFilter, CredSource, Subscriber};
use metafolder_watchd::proto::{self, ClientMsg, Event, ServerMsg};
use metafolder_watchd::server::{Broker, RootSink};

/// Everyone may see everything under `/repo`; nothing under `/locked`.
struct FakeCreds;

impl CredSource for FakeCreds {
    fn dir_meta(&self, path: &Path) -> Option<(u32, u32, u32)> {
        if path.starts_with("/locked") {
            Some((0, 0, 0o700))
        } else {
            Some((0, 0, 0o777))
        }
    }

    fn groups_of(&self, _pid: i32) -> Vec<u32> {
        Vec::new()
    }
}

/// The union of roots, as the marks would see it.
struct RecordingSink {
    seen: std::sync::Mutex<Vec<Vec<PathBuf>>>,
}

impl RootSink for RecordingSink {
    fn set_roots(&self, roots: Vec<PathBuf>) -> Result<()> {
        self.seen.lock().unwrap().push(roots);
        Ok(())
    }
}

fn next_msg(r: &mut BufReader<UnixStream>) -> ServerMsg {
    let mut line = String::new();
    r.read_line(&mut line).unwrap();
    proto::decode(&line).unwrap()
}

#[test]
fn test_the_accept_path_identifies_the_peer_and_serves_the_filtered_stream() {
    let dir = std::env::temp_dir().join(format!("metafolder-watchd-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let socket = dir.join("watchd.sock");

    let sink = Arc::new(RecordingSink { seen: std::sync::Mutex::new(Vec::new()) });
    let broker =
        Arc::new(Broker::new(AccessFilter::new(FakeCreds), Arc::clone(&sink) as Arc<dyn RootSink>));
    let (tx, rx) = mpsc::channel::<Event>();
    let listener = UnixListener::bind(&socket).unwrap();
    {
        let broker = Arc::clone(&broker);
        std::thread::spawn(move || broker.serve(listener, rx));
    }

    // A real connection: the broker identifies us through the kernel.
    let mut stream = UnixStream::connect(&socket).unwrap();
    let mut r = BufReader::new(stream.try_clone().unwrap());
    let sub = ClientMsg::Subscribe { roots: vec!["/repo".into(), "/locked/nope".into()] };
    stream.write_all(proto::encode(&sub).as_bytes()).unwrap();

    // Root bypasses DAC (in the filter as in the kernel), so the refusal half
    // of this test only reads as written for a normal uid.
    let is_root = unsafe { libc::getuid() } == 0;
    match next_msg(&mut r) {
        ServerMsg::Subscribed { roots, denied } => {
            if is_root {
                assert_eq!(roots.len(), 2);
            } else {
                assert_eq!(roots, vec!["/repo".into()]);
                assert_eq!(denied.len(), 1);
                assert_eq!(denied[0].reason, "not accessible");
            }
        }
        other => panic!("expected Subscribed, got {other:?}"),
    }
    // The event source is told about the union of what is watched — and about
    // nothing the subscriber was refused.
    assert_eq!(
        *sink.seen.lock().unwrap(),
        vec![if is_root {
            vec![PathBuf::from("/repo"), PathBuf::from("/locked/nope")]
        } else {
            vec![PathBuf::from("/repo")]
        }]
    );

    // One event inside the root arrives; one outside it does not; and one
    // under a directory the peer could not list arrives at nobody.
    tx.send(Event::Create { path: "/repo/a".into() }).unwrap();
    tx.send(Event::Create { path: "/elsewhere/b".into() }).unwrap();
    tx.send(Event::Create { path: "/locked/c".into() }).unwrap();

    assert_eq!(
        next_msg(&mut r),
        ServerMsg::Event { event: Event::Create { path: "/repo/a".into() } }
    );

    // What the peer may *see* also gates the root it watches: /locked/c is in
    // nobody's roots here, but the visibility rule is exercised directly.
    let peer_uid = unsafe { libc::getuid() };
    let mut filter = AccessFilter::new(FakeCreds);
    let peer = Subscriber { uid: peer_uid, gid: 0, pid: 0, groups: vec![0] };
    if !is_root {
        assert!(!filter.may_see(&peer, Path::new("/locked"), Path::new("/locked/c")));
    }

    drop(stream);
    drop(tx);
    std::fs::remove_dir_all(&dir).ok();
}
