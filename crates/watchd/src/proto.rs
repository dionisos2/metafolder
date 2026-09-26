//! The wire protocol between the broker and its subscribers
//! (docs/watcher-fanotify.md "The broker"): one JSON object per line over a
//! Unix stream socket — NDJSON, so a subscriber can be written in any language
//! and the stream is readable while it runs.
//!
//! Paths are **absolute**. The broker knows roots, not repositories: it is the
//! subscriber that re-anchors a path to its own repository root (the daemon's
//! `watcher::relative` already answers `None` for anything outside).

use std::path::{Path, PathBuf};

use serde::{de::DeserializeOwned, Deserialize, Serialize};

/// What the broker saw, in the daemon's own vocabulary — the `FsEvent` forms
/// of spec-file-tracking "Event semantics", so a subscriber's translation is a
/// rename of fields and nothing more.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    Create {
        path: String,
    },
    Remove {
        path: String,
    },
    /// Both sides of a move, in one event (fanotify's `FAN_RENAME` reports
    /// them atomically — no cookie correlation needed).
    Rename {
        from: String,
        to: String,
    },
    /// A move whose destination left the subscriber's roots.
    RenameFrom {
        path: String,
    },
    /// A move whose source was outside the subscriber's roots.
    RenameTo {
        path: String,
    },
    ModifyData {
        path: String,
    },
    ModifyMeta {
        path: String,
    },
}

impl Event {
    /// Every path this event names.
    pub fn paths(&self) -> Vec<&str> {
        match self {
            Event::Create { path }
            | Event::Remove { path }
            | Event::RenameFrom { path }
            | Event::RenameTo { path }
            | Event::ModifyData { path }
            | Event::ModifyMeta { path } => vec![path],
            Event::Rename { from, to } => vec![from, to],
        }
    }

    /// Rewrites this event for one subscriber: kept only where it falls under
    /// `roots` *and* `visible` says the subscriber could have discovered the
    /// path itself. A two-sided rename degrades to its one-sided form when the
    /// other side is out of scope — the same distinction the daemon draws for
    /// a move that leaves the watched tree (spec-file-tracking "File Watcher").
    pub fn adapt(&self, roots: &[PathBuf], visible: &mut dyn FnMut(&str) -> bool) -> Option<Event> {
        let in_scope = |p: &str, visible: &mut dyn FnMut(&str) -> bool| {
            let path = Path::new(p);
            visible(p) && roots.iter().any(|r| path.starts_with(r))
        };
        match self {
            Event::Rename { from, to } => match (in_scope(from, visible), in_scope(to, visible)) {
                (true, true) => Some(Event::Rename { from: from.clone(), to: to.clone() }),
                (true, false) => Some(Event::RenameFrom { path: from.clone() }),
                (false, true) => Some(Event::RenameTo { path: to.clone() }),
                (false, false) => None,
            },
            _ => {
                let path = self.paths()[0];
                in_scope(path, visible).then(|| self.clone())
            }
        }
    }
}

/// A root the subscriber may not see, and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Denied {
    pub root: String,
    pub reason: String,
}

/// Subscriber → broker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ClientMsg {
    /// Declare the roots this subscriber wants events under. Idempotent: a new
    /// `subscribe` *replaces* the set. The broker answers `Subscribed` once it
    /// has checked each root against the subscriber's own credentials.
    Subscribe { roots: Vec<String> },
}

/// Broker → subscriber.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ServerMsg {
    /// The answer to [`ClientMsg::Subscribe`]: what is being watched, and what
    /// was refused (with the reason, so the daemon can report it rather than
    /// silently watch less).
    Subscribed {
        roots: Vec<String>,
        denied: Vec<Denied>,
    },
    Event {
        event: Event,
    },
    /// Events were *dropped* for this subscriber: its queue overflowed (a slow
    /// consumer). Everything between this marker and the previous message is
    /// gone. The subscriber recovers the way a daemon that was down does — a
    /// reconcile (spec-file-tracking "Reconcile") — never by guessing.
    Overflow {},
    Error {
        message: String,
    },
}

/// One line of the stream — `msg` with its trailing newline.
pub fn encode<M: Serialize>(msg: &M) -> String {
    let mut line = serde_json::to_string(msg).expect("wire messages always serialize");
    line.push('\n');
    line
}

/// Parses one stream line (its newline optional).
pub fn decode<M: DeserializeOwned>(line: &str) -> serde_json::Result<M> {
    serde_json::from_str(line.trim_end_matches('\n'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(msg: &ServerMsg) {
        let line = encode(msg);
        assert!(line.ends_with('\n'));
        assert_eq!(decode::<ServerMsg>(&line).unwrap(), *msg);
    }

    #[test]
    fn test_every_message_survives_the_wire() {
        round_trip(&ServerMsg::Subscribed {
            roots: vec!["/repo".into()],
            denied: vec![Denied { root: "/other".into(), reason: "not accessible".into() }],
        });
        round_trip(&ServerMsg::Event {
            event: Event::Rename { from: "/repo/a".into(), to: "/repo/b".into() },
        });
        round_trip(&ServerMsg::Overflow {});
        round_trip(&ServerMsg::Error { message: "boom".into() });
        let line = encode(&ClientMsg::Subscribe { roots: vec!["/repo".into()] });
        assert_eq!(
            decode::<ClientMsg>(&line).unwrap(),
            ClientMsg::Subscribe { roots: vec!["/repo".into()] }
        );
    }

    #[test]
    fn test_the_wire_shape_is_the_documented_one() {
        // The format is a promise to non-Rust subscribers (the design note
        // names NDJSON), so it is asserted as text, not only through serde.
        assert_eq!(
            encode(&Event::ModifyData { path: "/repo/x".into() }),
            "{\"kind\":\"modify_data\",\"path\":\"/repo/x\"}\n"
        );
        assert_eq!(
            encode(&ClientMsg::Subscribe { roots: vec![] }),
            "{\"op\":\"subscribe\",\"roots\":[]}\n"
        );
    }

    #[test]
    fn test_a_rename_degrades_to_its_visible_side() {
        let roots = vec![PathBuf::from("/repo")];
        let ev = Event::Rename { from: "/repo/a".into(), to: "/elsewhere/b".into() };
        let mut see_all = |_: &str| true;
        assert_eq!(
            ev.adapt(&roots, &mut see_all),
            Some(Event::RenameFrom { path: "/repo/a".into() })
        );

        let ev = Event::Rename { from: "/elsewhere/a".into(), to: "/repo/b".into() };
        assert_eq!(
            ev.adapt(&roots, &mut see_all),
            Some(Event::RenameTo { path: "/repo/b".into() })
        );

        let ev = Event::Rename { from: "/elsewhere/a".into(), to: "/other/b".into() };
        assert_eq!(ev.adapt(&roots, &mut see_all), None);
    }

    #[test]
    fn test_an_event_the_subscriber_could_not_have_seen_is_dropped() {
        let roots = vec![PathBuf::from("/repo")];
        let ev = Event::Create { path: "/repo/secret/x".into() };
        let mut deny_secret = |p: &str| !p.contains("secret");
        assert_eq!(ev.adapt(&roots, &mut deny_secret), None);
    }

    #[test]
    fn test_an_event_outside_every_root_is_dropped() {
        let roots = vec![PathBuf::from("/repo")];
        let ev = Event::Remove { path: "/repo-other/x".into() };
        let mut see_all = |_: &str| true;
        assert_eq!(ev.adapt(&roots, &mut see_all), None);
    }
}
