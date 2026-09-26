//! The wire protocol between the broker and its subscribers
//! (docs/watcher-fanotify.md "The broker"): one JSON object per line over a
//! Unix stream socket — NDJSON, so a subscriber can be written in any language
//! and the stream is readable while it runs.
//!
//! Paths are **absolute**. The broker knows roots, not repositories: it is the
//! subscriber that re-anchors a path to its own repository root (the daemon's
//! `watcher::relative` already answers `None` for anything outside).
//!
//! Paths are also **byte strings** ([`WirePath`]): a POSIX name need not be
//! UTF-8, and a file with a Latin-1 name is watched like any other
//! (spec-data-model "Tree names").

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use serde::{de::DeserializeOwned, Deserialize, Deserializer, Serialize, Serializer};

/// An absolute path on the wire, exact to the byte. A path that is valid UTF-8
/// (the overwhelmingly common case) is a plain JSON string; only one that no
/// text can represent takes the object form `{"text", "bytes"}` — the wire form
/// of a tree name on the daemon's own API (spec-data-model "Tree names"):
/// `bytes` (lowercase hex) is authoritative, `text` is for readers that only
/// display.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WirePath(PathBuf);

impl WirePath {
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    pub fn into_path_buf(self) -> PathBuf {
        self.0
    }
}

impl From<PathBuf> for WirePath {
    fn from(p: PathBuf) -> Self {
        WirePath(p)
    }
}

impl From<&Path> for WirePath {
    fn from(p: &Path) -> Self {
        WirePath(p.to_path_buf())
    }
}

impl From<&str> for WirePath {
    fn from(p: &str) -> Self {
        WirePath(PathBuf::from(p))
    }
}

impl From<String> for WirePath {
    fn from(p: String) -> Self {
        WirePath(PathBuf::from(p))
    }
}

impl std::fmt::Display for WirePath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.display().fmt(f)
    }
}

/// The two wire shapes of a [`WirePath`].
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum WireShape {
    Text(String),
    Bytes { text: String, bytes: String },
}

impl Serialize for WirePath {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let shape = match self.0.to_str() {
            Some(text) => WireShape::Text(text.to_string()),
            None => {
                let raw = self.0.as_os_str().as_bytes();
                WireShape::Bytes {
                    text: String::from_utf8_lossy(raw).into_owned(),
                    bytes: raw.iter().map(|b| format!("{b:02x}")).collect(),
                }
            }
        };
        shape.serialize(s)
    }
}

impl<'de> Deserialize<'de> for WirePath {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        match WireShape::deserialize(d)? {
            WireShape::Text(text) => Ok(WirePath(PathBuf::from(text))),
            WireShape::Bytes { bytes, .. } => {
                let raw = decode_hex(&bytes)
                    .ok_or_else(|| serde::de::Error::custom("`bytes` is not lowercase hex"))?;
                Ok(WirePath(PathBuf::from(OsStr::from_bytes(&raw))))
            }
        }
    }
}

/// Lowercase hex to bytes; `None` on anything else — a malformed path is
/// refused, never guessed at.
fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    let digit = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    hex.as_bytes().chunks(2).map(|pair| Some(digit(pair[0])? << 4 | digit(pair[1])?)).collect()
}

/// What the broker saw, in the daemon's own vocabulary — the `FsEvent` forms
/// of spec-file-tracking "Event semantics", so a subscriber's translation is a
/// rename of fields and nothing more.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Event {
    Create {
        path: WirePath,
    },
    Remove {
        path: WirePath,
    },
    /// Both sides of a move, in one event (fanotify's `FAN_RENAME` reports
    /// them atomically — no cookie correlation needed).
    Rename {
        from: WirePath,
        to: WirePath,
    },
    /// A move whose destination left the subscriber's roots.
    RenameFrom {
        path: WirePath,
    },
    /// A move whose source was outside the subscriber's roots.
    RenameTo {
        path: WirePath,
    },
    ModifyData {
        path: WirePath,
    },
    ModifyMeta {
        path: WirePath,
    },
}

impl Event {
    /// Every path this event names.
    pub fn paths(&self) -> Vec<&WirePath> {
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
    pub fn adapt(
        &self,
        roots: &[PathBuf],
        visible: &mut dyn FnMut(&Path) -> bool,
    ) -> Option<Event> {
        let in_scope = |p: &WirePath, visible: &mut dyn FnMut(&Path) -> bool| {
            let path = p.as_path();
            visible(path) && roots.iter().any(|r| path.starts_with(r))
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
    pub root: WirePath,
    pub reason: String,
}

/// Subscriber → broker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ClientMsg {
    /// Declare the roots this subscriber wants events under. Idempotent: a new
    /// `subscribe` *replaces* the set. The broker answers `Subscribed` once it
    /// has checked each root against the subscriber's own credentials.
    Subscribe { roots: Vec<WirePath> },
}

/// Broker → subscriber.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ServerMsg {
    /// The answer to [`ClientMsg::Subscribe`]: what is being watched, and what
    /// was refused (with the reason, so the daemon can report it rather than
    /// silently watch less).
    Subscribed {
        roots: Vec<WirePath>,
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

    /// `/repo/caf\xE9` — a Latin-1 name, invalid UTF-8.
    fn latin1() -> WirePath {
        use std::os::unix::ffi::OsStrExt;
        WirePath::from(PathBuf::from(std::ffi::OsStr::from_bytes(b"/repo/caf\xE9")))
    }

    #[test]
    fn test_a_non_utf8_path_crosses_the_wire_byte_for_byte() {
        // Paths are byte strings (spec-data-model "Tree names"): a file with a
        // Latin-1 name is watched like any other, not dropped.
        round_trip(&ServerMsg::Event { event: Event::Create { path: latin1() } });
        round_trip(&ServerMsg::Event {
            event: Event::Rename { from: latin1(), to: "/repo/b".into() },
        });
        let line = encode(&ClientMsg::Subscribe { roots: vec![latin1()] });
        assert_eq!(
            decode::<ClientMsg>(&line).unwrap(),
            ClientMsg::Subscribe { roots: vec![latin1()] }
        );
    }

    #[test]
    fn test_a_non_utf8_path_takes_the_documented_object_form() {
        // The same shape as a tree name on the daemon's API: a plain string
        // whenever the path is text, and only otherwise `text` (display) +
        // `bytes` (lowercase hex, authoritative).
        assert_eq!(
            encode(&Event::Remove { path: latin1() }),
            "{\"kind\":\"remove\",\"path\":{\"text\":\"/repo/caf\u{fffd}\",\
             \"bytes\":\"2f7265706f2f636166e9\"}}\n"
        );
        // Malformed bytes are refused rather than guessed at.
        assert!(decode::<Event>(r#"{"kind":"remove","path":{"text":"x","bytes":"e"}}"#).is_err());
        assert!(decode::<Event>(r#"{"kind":"remove","path":{"text":"x","bytes":"zz"}}"#).is_err());
    }

    #[test]
    fn test_a_rename_degrades_to_its_visible_side() {
        let roots = vec![PathBuf::from("/repo")];
        let ev = Event::Rename { from: "/repo/a".into(), to: "/elsewhere/b".into() };
        let mut see_all = |_: &Path| true;
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
        let mut deny_secret = |p: &Path| !p.to_string_lossy().contains("secret");
        assert_eq!(ev.adapt(&roots, &mut deny_secret), None);
    }

    #[test]
    fn test_an_event_outside_every_root_is_dropped() {
        let roots = vec![PathBuf::from("/repo")];
        let ev = Event::Remove { path: "/repo-other/x".into() };
        let mut see_all = |_: &Path| true;
        assert_eq!(ev.adapt(&roots, &mut see_all), None);
    }
}
