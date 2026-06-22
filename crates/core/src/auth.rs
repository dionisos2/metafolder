//! Loopback session-token authentication (spec-auth).
//!
//! The daemon and the GUI HTTP servers bind `127.0.0.1`, which is reachable
//! from the user's own browser. To keep web content out, every request must
//! carry a per-service secret token that only a process able to read a
//! user-only token file can supply. The token lives in a per-user runtime
//! directory (`$XDG_RUNTIME_DIR/metafolder/` or a hardened
//! `/tmp/metafolder-<uid>/`), one file per service (`daemon.token`,
//! `gui.token`), mode `0600`, inside a `0700` directory.
//!
//! This module is std-only (no external crate): randomness comes from
//! `/dev/urandom`, in the spirit of the project's dependency-free helpers.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Authorization header prefix for the bearer scheme.
pub const BEARER_PREFIX: &str = "Bearer ";

/// The per-user runtime directory holding the session tokens (not created).
///
/// `$XDG_RUNTIME_DIR/metafolder` when `XDG_RUNTIME_DIR` is absolute, otherwise
/// `/tmp/metafolder-<uid>` (Unix). `Err` when neither is resolvable.
pub fn runtime_dir() -> Result<PathBuf, String> {
    runtime_base(std::env::var_os("XDG_RUNTIME_DIR").as_deref(), current_uid()).ok_or_else(|| {
        "cannot locate a runtime directory for the session token \
         (set XDG_RUNTIME_DIR)"
            .to_string()
    })
}

/// Pure resolution of the runtime base directory, parameterised for tests.
/// An absolute `$XDG_RUNTIME_DIR` wins (relative values are ignored, per XDG);
/// otherwise the per-uid `/tmp` fallback is used.
fn runtime_base(xdg: Option<&OsStr>, uid: Option<u32>) -> Option<PathBuf> {
    if let Some(dir) = xdg.map(PathBuf::from).filter(|p| p.is_absolute()) {
        return Some(dir.join("metafolder"));
    }
    uid.map(|uid| PathBuf::from(format!("/tmp/metafolder-{uid}")))
}

/// The current user's uid, derived from the owner of `$HOME` (no `libc`
/// dependency). Used to name and verify the `/tmp` fallback directory.
#[cfg(unix)]
fn current_uid() -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    let home = std::env::var_os("HOME")?;
    std::fs::metadata(home).ok().map(|m| m.uid())
}

#[cfg(not(unix))]
fn current_uid() -> Option<u32> {
    None
}

/// Server side: ensure the token for `service` exists and return it.
///
/// Creates (and hardens) the runtime directory, then reuses an existing token
/// file or generates a new one written `0600`. Stable across restarts: an
/// existing non-empty token is reused so long-running clients keep working.
pub fn ensure_token(service: &str) -> Result<String, String> {
    ensure_token_in(&runtime_dir()?, service)
}

/// Client side: read the token for `service`. A missing/empty file is an
/// error (the server is not running, or not as this user).
pub fn read_token(service: &str) -> Result<String, String> {
    read_token_in(&runtime_dir()?, service)
}

fn token_file(dir: &Path, service: &str) -> PathBuf {
    dir.join(format!("{service}.token"))
}

fn ensure_token_in(dir: &Path, service: &str) -> Result<String, String> {
    create_secure_dir(dir)?;
    let path = token_file(dir, service);
    if let Ok(existing) = std::fs::read_to_string(&path) {
        let existing = existing.trim();
        if !existing.is_empty() {
            return Ok(existing.to_string());
        }
    }
    let token = generate_token()?;
    write_token_file(&path, &token)?;
    Ok(token)
}

fn read_token_in(dir: &Path, service: &str) -> Result<String, String> {
    let path = token_file(dir, service);
    let contents = std::fs::read_to_string(&path)
        .map_err(|e| format!("cannot read the {service} token ({}): {e}", path.display()))?;
    let token = contents.trim();
    if token.is_empty() {
        return Err(format!("the {service} token file {} is empty", path.display()));
    }
    Ok(token.to_string())
}

/// A fresh token: 32 bytes of `/dev/urandom`, hex-encoded (64 ASCII chars).
pub fn generate_token() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    read_random(&mut bytes)?;
    Ok(hex_encode(&bytes))
}

/// Extract the token from an `Authorization: Bearer <token>` header value.
pub fn bearer_token(header: Option<&str>) -> Option<&str> {
    header?.strip_prefix(BEARER_PREFIX)
}

/// Constant-time string comparison (no early exit on the first differing
/// byte). Token lengths are fixed, so the length-mismatch fast path leaks
/// nothing useful.
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

// ── Unix filesystem primitives ──────────────────────────────────────────────

#[cfg(unix)]
fn read_random(buf: &mut [u8]) -> Result<(), String> {
    use std::io::Read;
    let mut file =
        std::fs::File::open("/dev/urandom").map_err(|e| format!("cannot open /dev/urandom: {e}"))?;
    file.read_exact(buf).map_err(|e| format!("cannot read /dev/urandom: {e}"))
}

/// Create the runtime directory `0700`, or verify a pre-existing one is safe
/// (a real directory, owned by us, mode `0700`, not a symlink). `/tmp` is
/// world-writable, so an unsafe pre-existing directory is refused rather than
/// trusted with a secret.
#[cfg(unix)]
fn create_secure_dir(dir: &Path) -> Result<(), String> {
    use std::fs::{DirBuilder, Permissions};
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

    match DirBuilder::new().mode(0o700).create(dir) {
        Ok(()) => {
            // Neutralise the umask: force the mode we intend.
            std::fs::set_permissions(dir, Permissions::from_mode(0o700))
                .map_err(|e| format!("cannot set permissions on {}: {e}", dir.display()))
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => verify_secure_dir(dir),
        Err(e) => Err(format!("cannot create runtime directory {}: {e}", dir.display())),
    }
}

#[cfg(unix)]
fn verify_secure_dir(dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::symlink_metadata(dir)
        .map_err(|e| format!("cannot stat runtime directory {}: {e}", dir.display()))?;
    if meta.file_type().is_symlink() {
        return Err(format!("runtime directory {} is a symlink; refusing", dir.display()));
    }
    if !meta.is_dir() {
        return Err(format!("runtime path {} is not a directory", dir.display()));
    }
    if let Some(uid) = current_uid() {
        if meta.uid() != uid {
            return Err(format!(
                "runtime directory {} is not owned by the current user; refusing",
                dir.display()
            ));
        }
    }
    if meta.mode() & 0o777 != 0o700 {
        return Err(format!(
            "runtime directory {} has insecure permissions (expected 0700); refusing",
            dir.display()
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn write_token_file(path: &Path, token: &str) -> Result<(), String> {
    use std::fs::{OpenOptions, Permissions};
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("cannot write token file {}: {e}", path.display()))?;
    file.write_all(token.as_bytes())
        .map_err(|e| format!("cannot write token file {}: {e}", path.display()))?;
    // Force 0600 even if the file pre-existed with a wider mode.
    std::fs::set_permissions(path, Permissions::from_mode(0o600))
        .map_err(|e| format!("cannot set permissions on {}: {e}", path.display()))
}

// ── Non-Unix fallback (Windows: deferred hardening, see spec-platform) ───────

#[cfg(not(unix))]
fn read_random(buf: &mut [u8]) -> Result<(), String> {
    use std::io::Read;
    let mut file = std::fs::File::open("/dev/urandom")
        .map_err(|_| "secure randomness is unavailable on this platform".to_string())?;
    file.read_exact(buf).map_err(|e| format!("cannot read randomness: {e}"))
}

#[cfg(not(unix))]
fn create_secure_dir(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir)
        .map_err(|e| format!("cannot create runtime directory {}: {e}", dir.display()))
}

#[cfg(not(unix))]
fn write_token_file(path: &Path, token: &str) -> Result<(), String> {
    std::fs::write(path, token)
        .map_err(|e| format!("cannot write token file {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("metafolder-auth-{tag}-{}", std::process::id()));
        p
    }

    #[test]
    fn runtime_base_prefers_absolute_xdg() {
        let base = runtime_base(Some(OsStr::new("/run/user/1000")), Some(1000));
        assert_eq!(base, Some(PathBuf::from("/run/user/1000/metafolder")));
    }

    #[test]
    fn runtime_base_ignores_relative_xdg_and_uses_tmp_uid() {
        let base = runtime_base(Some(OsStr::new("rel/dir")), Some(1000));
        assert_eq!(base, Some(PathBuf::from("/tmp/metafolder-1000")));
    }

    #[test]
    fn runtime_base_falls_back_to_tmp_when_no_xdg() {
        let base = runtime_base(None, Some(42));
        assert_eq!(base, Some(PathBuf::from("/tmp/metafolder-42")));
    }

    #[test]
    fn runtime_base_none_without_xdg_or_uid() {
        assert_eq!(runtime_base(None, None), None);
    }

    #[test]
    fn generate_token_is_64_hex_chars_and_unique() {
        let a = generate_token().unwrap();
        let b = generate_token().unwrap();
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b, "two tokens must differ");
    }

    #[test]
    fn constant_time_eq_matches_string_equality() {
        assert!(constant_time_eq("abcd", "abcd"));
        assert!(!constant_time_eq("abcd", "abce"));
        assert!(!constant_time_eq("abcd", "abcde"));
        assert!(!constant_time_eq("", "x"));
        assert!(constant_time_eq("", ""));
    }

    #[test]
    fn bearer_token_strips_prefix() {
        assert_eq!(bearer_token(Some("Bearer xyz")), Some("xyz"));
        assert_eq!(bearer_token(Some("Basic xyz")), None);
        assert_eq!(bearer_token(None), None);
    }

    #[test]
    fn ensure_token_is_stable_and_readable() {
        let dir = temp_dir("stable");
        let _ = std::fs::remove_dir_all(&dir);

        let first = ensure_token_in(&dir, "daemon").unwrap();
        assert_eq!(first.len(), 64);
        let second = ensure_token_in(&dir, "daemon").unwrap();
        assert_eq!(first, second, "token must be stable across calls");

        let read = read_token_in(&dir, "daemon").unwrap();
        assert_eq!(read, first);

        // Distinct services get distinct tokens.
        let gui = ensure_token_in(&dir, "gui").unwrap();
        assert_ne!(gui, first);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn token_file_is_0600_in_0700_dir() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("perms");
        let _ = std::fs::remove_dir_all(&dir);

        ensure_token_in(&dir, "daemon").unwrap();
        let dir_mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        let file_mode = std::fs::metadata(token_file(&dir, "daemon")).unwrap().permissions().mode()
            & 0o777;
        assert_eq!(dir_mode, 0o700, "runtime dir must be 0700");
        assert_eq!(file_mode, 0o600, "token file must be 0600");

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlinked_runtime_dir() {
        let real = temp_dir("symlink-target");
        let link = temp_dir("symlink-dir");
        let _ = std::fs::remove_dir_all(&real);
        let _ = std::fs::remove_file(&link);
        std::fs::create_dir_all(&real).unwrap();
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let err = ensure_token_in(&link, "daemon").unwrap_err();
        assert!(err.contains("symlink"), "got: {err}");

        std::fs::remove_file(&link).unwrap();
        std::fs::remove_dir_all(&real).unwrap();
    }

    #[test]
    fn read_token_reports_missing() {
        let dir = temp_dir("missing");
        let _ = std::fs::remove_dir_all(&dir);
        let err = read_token_in(&dir, "daemon").unwrap_err();
        assert!(err.contains("cannot read"), "got: {err}");
    }
}
