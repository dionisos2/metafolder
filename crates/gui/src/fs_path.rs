//! Carrying a filesystem path across the Tauri boundary.
//!
//! The frontend speaks JSON, so every path it holds is a *string* — but a POSIX
//! path is a byte string that need not be UTF-8 (spec-data-model "Tree names").
//! Converting it lossily, as this used to, produced a path that names nothing:
//! the row appeared in the file manager and every action on it failed, because
//! `caf<?>.mp4` is three bytes where the disk has one.
//!
//! The handle the frontend gets is therefore the *escaped* form the rest of
//! metafolder displays — `caf%E9.mp4` — which is text, so it survives JSON, and
//! which the panels can still slice and join like the plain paths they are.
//! [`from_handle`] turns it back into the exact bytes before anything touches
//! the disk.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use metafolder_core::metarecord::{escaped_to_bytes, TreeName};

/// The handle for a path: itself when the path is text, its escaped form
/// otherwise.
pub fn to_handle(path: &Path) -> String {
    TreeName::from_bytes(path_bytes(path)).display().into_owned()
}

/// The path a handle names.
///
/// A handle that spells an escape has two readings — a file *really* named
/// `caf%E9.mp4` and one named with the byte `0xE9` — and unlike a query, an
/// operation on the disk needs exactly one. Existence decides: the literal
/// reading wins when it is a real file, which is both the commoner case and the
/// one the user typed if they typed anything. Neither existing is not an error
/// here — the caller reports what the operation itself returns (a rename to a
/// new name goes through this too).
pub fn from_handle(handle: &str) -> PathBuf {
    let verbatim = PathBuf::from(handle);
    match escaped_to_bytes(handle) {
        Some(bytes) if !verbatim.exists() => from_bytes(bytes),
        _ => verbatim,
    }
}

/// Whether this path needs escaping to be shown — what marks a row in the file
/// manager as carrying a name no text can represent.
pub fn is_escaped(path: &Path) -> bool {
    std::str::from_utf8(&path_bytes(path)).is_err()
}

#[cfg(unix)]
fn path_bytes(path: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    path.as_os_str().as_bytes().to_vec()
}

#[cfg(not(unix))]
fn path_bytes(path: &Path) -> Vec<u8> {
    path.to_string_lossy().into_owned().into_bytes()
}

#[cfg(unix)]
fn from_bytes(bytes: Vec<u8>) -> PathBuf {
    use std::os::unix::ffi::OsStringExt;
    PathBuf::from(OsString::from_vec(bytes))
}

#[cfg(not(unix))]
fn from_bytes(bytes: Vec<u8>) -> PathBuf {
    PathBuf::from(String::from_utf8_lossy(&bytes).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    fn latin1_path(dir: &Path) -> PathBuf {
        use std::os::unix::ffi::OsStrExt;
        dir.join(OsStr::from_bytes(b"caf\xe9.mp4"))
    }
    #[cfg(unix)]
    use std::ffi::OsStr;

    #[test]
    fn test_an_ordinary_path_is_its_own_handle() {
        // The common case must be untouched — "%" included, since a lone "%"
        // is not an escape.
        for text in ["/home/a/vidéo.mp4", "/tmp/100%.txt", "/tmp/%1234.txt"] {
            assert_eq!(to_handle(Path::new(text)), text);
            assert_eq!(from_handle(text), PathBuf::from(text));
        }
    }

    #[cfg(unix)]
    #[test]
    fn test_an_undecodable_path_round_trips_through_its_handle() {
        let path = latin1_path(Path::new("/tmp"));
        let handle = to_handle(&path);
        assert_eq!(handle, "/tmp/caf%E9.mp4");
        // What matters: the bytes come back, so the file can actually be opened.
        assert_eq!(from_handle(&handle), path);
    }

    #[cfg(unix)]
    #[test]
    fn test_a_file_really_named_like_an_escape_wins_when_it_exists() {
        // The one collision: existence decides, and the literal file is what
        // the handle names when it is there.
        let dir = tempfile::tempdir().unwrap();
        let literal = dir.path().join("caf%E9.mp4");
        std::fs::write(&literal, b"x").unwrap();

        let handle = to_handle(&literal);
        assert_eq!(from_handle(&handle), literal);
    }

    #[cfg(unix)]
    #[test]
    fn test_the_escaped_reading_is_used_when_the_literal_one_does_not_exist() {
        let dir = tempfile::tempdir().unwrap();
        let raw = latin1_path(dir.path());
        std::fs::write(&raw, b"x").unwrap();

        assert_eq!(from_handle(&to_handle(&raw)), raw);
    }

    #[test]
    fn test_a_handle_for_a_path_that_does_not_exist_yet_stays_verbatim() {
        // Creating "new%file.txt" must not silently become something else.
        let handle = "/tmp/does-not-exist/new%file.txt";
        assert_eq!(from_handle(handle), PathBuf::from(handle));
    }

    #[cfg(unix)]
    #[test]
    fn test_only_an_undecodable_path_is_marked() {
        assert!(!is_escaped(Path::new("/tmp/100%.txt")));
        assert!(!is_escaped(Path::new("/tmp/vidéo.mp4")));
        assert!(is_escaped(&latin1_path(Path::new("/tmp"))));
    }
}
