//! Per-repo input history (spec-gui "Input history"): one plain-text file per
//! text zone under `.metafolder/internal/history/<zone>`, one entry per line,
//! oldest first. Lives in `internal/` so appends are invisible to tracking
//! (the watcher and reconcile exclude that directory by absolute path).

use std::path::PathBuf;

use metafolder_core::sync::MutexExt;

use crate::error::ApiError;
use crate::state::RepoState;

/// Entries kept per zone; older ones are dropped on append.
pub const MAX_ENTRIES: usize = 1000;

const HISTORY_DIR: &str = "history";
const MAX_ZONE_LEN: usize = 64;

/// Validates a zone name: `[a-z0-9:_-]{1,64}`. The charset excludes `/`, `.`
/// and uppercase, so a zone can never traverse out of the history directory
/// nor collide with the `<zone>.tmp` write staging file.
pub fn validate_zone(zone: &str) -> Result<(), ApiError> {
    let ok = !zone.is_empty()
        && zone.len() <= MAX_ZONE_LEN
        && zone.bytes().all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b':' | b'_' | b'-'));
    if ok {
        Ok(())
    } else {
        Err(ApiError::bad_request(format!(
            "invalid history zone '{zone}' (expected [a-z0-9:_-]{{1,{MAX_ZONE_LEN}}})"
        )))
    }
}

fn zone_file(repo: &RepoState, zone: &str) -> PathBuf {
    repo.internal_dir().join(HISTORY_DIR).join(zone)
}

/// Reads a zone's entries, oldest first. With `limit`, only the newest N are
/// returned (still oldest first). A missing file is an empty history.
pub fn read(repo: &RepoState, zone: &str, limit: Option<usize>) -> Result<Vec<String>, ApiError> {
    validate_zone(zone)?;
    let _guard = repo.history_lock.lock_recover();
    let mut entries = read_lines(repo, zone)?;
    if let Some(limit) = limit {
        if entries.len() > limit {
            entries.drain(..entries.len() - limit);
        }
    }
    Ok(entries)
}

/// Appends one entry to a zone. Returns `false` when the entry equals the
/// current newest one (consecutive dedup — nothing is written). The file is
/// capped to the newest [`MAX_ENTRIES`] and rewritten atomically (temp file +
/// rename), so a crash never leaves a truncated history.
pub fn append(repo: &RepoState, zone: &str, entry: &str) -> Result<bool, ApiError> {
    validate_zone(zone)?;
    if entry.trim().is_empty() || entry.contains('\n') || entry.contains('\r') {
        return Err(ApiError::bad_request(
            "a history entry must be a non-empty single line".to_string(),
        ));
    }
    let _guard = repo.history_lock.lock_recover();
    let mut entries = read_lines(repo, zone)?;
    if entries.last().map(String::as_str) == Some(entry) {
        return Ok(false);
    }
    entries.push(entry.to_string());
    if entries.len() > MAX_ENTRIES {
        entries.drain(..entries.len() - MAX_ENTRIES);
    }
    let file = zone_file(repo, zone);
    let dir = file.parent().expect("zone file has the history dir as parent");
    std::fs::create_dir_all(dir)
        .map_err(|e| ApiError::internal(format!("cannot create history directory: {e}")))?;
    let mut content = entries.join("\n");
    content.push('\n');
    let tmp = file.with_extension("tmp");
    std::fs::write(&tmp, content)
        .map_err(|e| ApiError::internal(format!("cannot write history file: {e}")))?;
    std::fs::rename(&tmp, &file)
        .map_err(|e| ApiError::internal(format!("cannot replace history file: {e}")))?;
    Ok(true)
}

/// Raw line read of a zone file (no locking, no limit). Skips empty lines so a
/// hand-edited file with blank runs stays well-formed.
fn read_lines(repo: &RepoState, zone: &str) -> Result<Vec<String>, ApiError> {
    let file = zone_file(repo, zone);
    let bytes = match std::fs::read(&file) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(ApiError::internal(format!("cannot read history file: {e}"))),
    };
    Ok(String::from_utf8_lossy(&bytes)
        .lines()
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo;
    use crate::state::RepoState;
    use std::sync::Arc;

    fn temp_repo(prefix: &str) -> Arc<RepoState> {
        let root = std::env::temp_dir()
            .join(format!("metafolder_history_unit_{prefix}_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let opened = repo::init_repository(&root, None, None).unwrap();
        Arc::new(RepoState::from_opened(opened))
    }

    #[test]
    fn test_validate_zone_table() {
        for ok in ["z", "shell:command", "metarecord-list:finder", "a_b-c:0", &"x".repeat(64)] {
            assert!(validate_zone(ok).is_ok(), "'{ok}' should be valid");
        }
        for bad in ["", "A", "a.b", "a/b", "a b", "é", "..", &"x".repeat(65)] {
            assert!(validate_zone(bad).is_err(), "'{bad}' should be rejected");
        }
    }

    #[test]
    fn test_concurrent_appends_lose_nothing() {
        let repo = temp_repo("concurrent");
        let threads: Vec<_> = (0..8)
            .map(|t| {
                let repo = repo.clone();
                std::thread::spawn(move || {
                    for i in 0..25 {
                        append(&repo, "z", &format!("t{t}-{i}")).unwrap();
                    }
                })
            })
            .collect();
        for handle in threads {
            handle.join().unwrap();
        }
        let entries = read(&repo, "z", None).unwrap();
        assert_eq!(entries.len(), 8 * 25, "all distinct appends must survive");
    }
}
