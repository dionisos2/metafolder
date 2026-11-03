//! Carrying a filesystem path across the Tauri boundary.
//!
//! The frontend speaks JSON, but a POSIX path is a byte string that need not
//! be UTF-8 (doc "Tree names"). So the handle the frontend gets is the path's
//! *wire form*, the daemon's own ([`TreePath`]): a plain string while the path
//! is text — which is every path but a rare few, so the panels slice and join
//! it as they always did — and `{text, bytes}` when it is not. The frontend
//! shows the `text` and hands the handle back unchanged; [`from_handle`] takes
//! the bytes, exactly.
//!
//! The handle used to be the escaped *text* (`caf%E9.mp4`), read back by
//! looking at the disk: a file really named `caf%E9.mp4` shows the same, so
//! the reading was a guess, and a rename to a new name could not be told from
//! either. A handle that is a string now means that string's bytes, always.

use std::path::{Path, PathBuf};

use metafolder_core::metarecord::TreePath;

/// The handle for a path: its exact bytes, in the wire form.
pub fn to_handle(path: &Path) -> TreePath {
    TreePath::from_path(path)
}

/// The path a handle names: its bytes, exactly — never a reading of an
/// escape.
pub fn from_handle(handle: &TreePath) -> PathBuf {
    handle.to_path_buf()
}

/// The path a URL names (`/fsraw`, `/thumbnail`, …): `path_bytes` (lowercase
/// hex) for a path that is not text, else `path`, the string's own bytes.
/// `None` when neither is given, or the hex is malformed.
pub fn from_query(path: Option<&str>, path_bytes: Option<&str>) -> Option<PathBuf> {
    match path_bytes {
        Some(hex) => {
            metafolder_core::hex::decode(hex).map(|b| TreePath::from_bytes(b).to_path_buf())
        }
        None => path.map(PathBuf::from),
    }
}

/// Whether this path needs escaping to be shown — what marks a row in the file
/// manager as carrying a name no text can represent.
pub fn is_escaped(path: &Path) -> bool {
    std::str::from_utf8(to_handle(path).as_bytes()).is_err()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn latin1_path(dir: &Path) -> PathBuf {
        use std::os::unix::ffi::OsStrExt;
        dir.join(std::ffi::OsStr::from_bytes(b"caf\xe9.mp4"))
    }

    #[test]
    fn test_an_ordinary_path_is_its_own_handle() {
        // The common case must be untouched — "%" included.
        for text in ["/home/a/vidéo.mp4", "/tmp/100%.txt", "/tmp/caf%E9.mp4"] {
            let handle = to_handle(Path::new(text));
            assert_eq!(serde_json::to_value(&handle).unwrap(), serde_json::json!(text));
            assert_eq!(from_handle(&handle), PathBuf::from(text));
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_an_undecodable_path_round_trips_through_its_handle() {
        let path = latin1_path(Path::new("/tmp"));
        let handle = to_handle(&path);
        let wire = serde_json::to_value(&handle).unwrap();
        assert_eq!(wire["text"], "/tmp/caf%E9.mp4", "shown escaped");
        let back: TreePath = serde_json::from_value(wire).unwrap();
        assert_eq!(from_handle(&back), path, "the bytes come back");
    }

    /// No guess: a string handle is that string, even when a file whose bytes
    /// its escape decodes to is there and the literal one is not.
    #[cfg(unix)]
    #[test]
    fn test_a_string_handle_never_reads_an_escape() {
        let dir = tempfile::tempdir().unwrap();
        let raw = latin1_path(dir.path());
        std::fs::write(&raw, b"x").unwrap();
        let literal = dir.path().join("caf%E9.mp4");
        let handle: TreePath =
            serde_json::from_value(serde_json::json!(literal.to_str().unwrap())).unwrap();
        assert_eq!(from_handle(&handle), literal);
    }

    #[cfg(unix)]
    #[test]
    fn test_a_url_names_a_path_by_its_text_or_its_bytes() {
        assert_eq!(from_query(Some("/tmp/caf%E9.mp4"), None), Some("/tmp/caf%E9.mp4".into()));
        let hex = metafolder_core::hex::encode(b"/tmp/caf\xe9.mp4");
        assert_eq!(from_query(Some("ignored"), Some(&hex)), Some(latin1_path(Path::new("/tmp"))));
        assert_eq!(from_query(None, Some("zz")), None);
        assert_eq!(from_query(None, None), None);
    }

    #[cfg(unix)]
    #[test]
    fn test_only_an_undecodable_path_is_marked() {
        assert!(!is_escaped(Path::new("/tmp/100%.txt")));
        assert!(!is_escaped(Path::new("/tmp/vidéo.mp4")));
        assert!(is_escaped(&latin1_path(Path::new("/tmp"))));
    }
}
