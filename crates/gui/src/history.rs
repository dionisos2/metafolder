//! Per-repo input history (spec-gui "Input history"): one plain-text file per
//! text zone under `.metafolder/gui/history/<zone>`, one entry per line,
//! oldest first. Purely a GUI concern — the daemon has no part in it; the GUI
//! resolves the repository's `.metafolder/` location (via `GET /repos`) and
//! reads/writes the files itself. The files are ordinary trackable content
//! (covered in practice by the default `\.metafolder(/.*)?$` ignore pattern).

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::daemon_proxy::DaemonProxy;

/// Entries kept per zone; older ones are dropped on append.
pub const MAX_ENTRIES: usize = 1000;

const HISTORY_SUBDIR: &str = "gui/history";
const MAX_ZONE_LEN: usize = 64;

/// Serializes read-modify-write cycles on the history files. Process-local:
/// concurrent GUI instances rely on the atomic rename (last writer wins).
static LOCK: Mutex<()> = Mutex::new(());

/// Validates a zone name: `[a-z0-9:_-]{1,64}`. The charset excludes `/`, `.`
/// and uppercase, so a zone can never traverse out of the history directory
/// nor collide with the `<zone>.tmp` write staging file.
pub fn validate_zone(zone: &str) -> Result<(), String> {
    let ok = !zone.is_empty()
        && zone.len() <= MAX_ZONE_LEN
        && zone.bytes().all(|b| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b':' | b'_' | b'-'));
    if ok {
        Ok(())
    } else {
        Err(format!("invalid history zone '{zone}' (expected [a-z0-9:_-]{{1,{MAX_ZONE_LEN}}})"))
    }
}

fn zone_file(metafolder_dir: &Path, zone: &str) -> PathBuf {
    metafolder_dir.join(HISTORY_SUBDIR).join(zone)
}

/// Resolves a repository uuid (dashed or 32-hex) to its `.metafolder/`
/// directory: `GET /repos` gives `internal_dir` (= `<metafolder>/internal`),
/// whose parent is the `.metafolder/` location — correct wherever it lives
/// (inside the root or external).
pub async fn metafolder_dir_of(daemon: &DaemonProxy, repo: &str) -> Result<PathBuf, String> {
    let wanted = repo.replace('-', "").to_lowercase();
    let res = daemon.request("GET", "/repos", None).await?;
    if res.status != 200 {
        return Err(format!("GET /repos failed with status {}", res.status));
    }
    let repos = res.body.as_array().cloned().unwrap_or_default();
    for entry in repos {
        let uuid = entry["repo_uuid"].as_str().unwrap_or_default().replace('-', "").to_lowercase();
        if uuid == wanted {
            let internal = entry["internal_dir"]
                .as_str()
                .ok_or_else(|| "GET /repos entry has no internal_dir".to_string())?;
            return Path::new(internal)
                .parent()
                .map(Path::to_path_buf)
                .ok_or_else(|| "internal_dir has no parent".to_string());
        }
    }
    Err(format!("repository {repo} is not loaded"))
}

/// Reads a zone's entries, oldest first. With `limit`, only the newest N are
/// returned (still oldest first). A missing file is an empty history.
pub fn read(
    metafolder_dir: &Path,
    zone: &str,
    limit: Option<usize>,
) -> Result<Vec<String>, String> {
    validate_zone(zone)?;
    let _guard = LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    let mut entries = read_lines(metafolder_dir, zone)?;
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
pub fn append(metafolder_dir: &Path, zone: &str, entry: &str) -> Result<bool, String> {
    validate_zone(zone)?;
    if entry.trim().is_empty() || entry.contains('\n') || entry.contains('\r') {
        return Err("a history entry must be a non-empty single line".to_string());
    }
    let _guard = LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    let mut entries = read_lines(metafolder_dir, zone)?;
    if entries.last().map(String::as_str) == Some(entry) {
        return Ok(false);
    }
    entries.push(entry.to_string());
    if entries.len() > MAX_ENTRIES {
        entries.drain(..entries.len() - MAX_ENTRIES);
    }
    let file = zone_file(metafolder_dir, zone);
    let dir = file.parent().expect("zone file has the history dir as parent");
    std::fs::create_dir_all(dir)
        .map_err(|e| format!("cannot create history directory: {e}"))?;
    let mut content = entries.join("\n");
    content.push('\n');
    let tmp = file.with_extension("tmp");
    std::fs::write(&tmp, content).map_err(|e| format!("cannot write history file: {e}"))?;
    std::fs::rename(&tmp, &file).map_err(|e| format!("cannot replace history file: {e}"))?;
    Ok(true)
}

/// Raw line read of a zone file (no locking, no limit). Skips empty lines so a
/// hand-edited file with blank runs stays well-formed.
fn read_lines(metafolder_dir: &Path, zone: &str) -> Result<Vec<String>, String> {
    let file = zone_file(metafolder_dir, zone);
    let bytes = match std::fs::read(&file) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("cannot read history file: {e}")),
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

    fn temp_metafolder(prefix: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("metafolder_gui_history_{prefix}_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
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
    fn test_read_missing_file_is_empty() {
        let dir = temp_metafolder("missing");
        assert_eq!(read(&dir, "z", None).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn test_append_then_read_roundtrip_under_gui_history() {
        let dir = temp_metafolder("roundtrip");
        assert!(append(&dir, "shell:command", "repo:list").unwrap());
        assert_eq!(read(&dir, "shell:command", None).unwrap(), vec!["repo:list"]);
        // The entry is a newline-terminated line under <metafolder>/gui/history/.
        let file = dir.join("gui/history/shell:command");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "repo:list\n");
    }

    #[test]
    fn test_entries_are_oldest_first_and_limit_keeps_the_newest() {
        let dir = temp_metafolder("order");
        for entry in ["one", "two", "three"] {
            append(&dir, "z", entry).unwrap();
        }
        assert_eq!(read(&dir, "z", None).unwrap(), vec!["one", "two", "three"]);
        assert_eq!(read(&dir, "z", Some(2)).unwrap(), vec!["two", "three"]);
    }

    #[test]
    fn test_consecutive_duplicate_is_deduped_but_not_distant_ones() {
        let dir = temp_metafolder("dedup");
        assert!(append(&dir, "z", "a").unwrap());
        assert!(!append(&dir, "z", "a").unwrap());
        assert!(append(&dir, "z", "b").unwrap());
        assert!(append(&dir, "z", "a").unwrap());
        assert_eq!(read(&dir, "z", None).unwrap(), vec!["a", "b", "a"]);
    }

    #[test]
    fn test_empty_or_multiline_entry_is_rejected() {
        let dir = temp_metafolder("bad_entry");
        for entry in ["", "   ", "a\nb", "a\rb"] {
            assert!(append(&dir, "z", entry).is_err(), "entry {entry:?} accepted");
        }
        assert_eq!(read(&dir, "z", None).unwrap(), Vec::<String>::new());
    }

    #[test]
    fn test_invalid_zone_creates_nothing() {
        let dir = temp_metafolder("bad_zone");
        for zone in ["Bad", "a/b", "a.b"] {
            assert!(append(&dir, zone, "entry").is_err());
        }
        assert!(!dir.join("gui/history").exists());
    }

    #[test]
    fn test_cap_keeps_the_newest_1000() {
        let dir = temp_metafolder("cap");
        let history_dir = dir.join("gui/history");
        std::fs::create_dir_all(&history_dir).unwrap();
        let lines: String = (0..1000).map(|i| format!("entry-{i}\n")).collect();
        std::fs::write(history_dir.join("z"), lines).unwrap();
        append(&dir, "z", "the-newest").unwrap();
        let got = read(&dir, "z", None).unwrap();
        assert_eq!(got.len(), 1000);
        assert_eq!(got.first().unwrap(), "entry-1"); // entry-0 dropped
        assert_eq!(got.last().unwrap(), "the-newest");
    }

    #[test]
    fn test_concurrent_appends_lose_nothing() {
        let dir = temp_metafolder("concurrent");
        let threads: Vec<_> = (0..8)
            .map(|t| {
                let dir = dir.clone();
                std::thread::spawn(move || {
                    for i in 0..25 {
                        append(&dir, "z", &format!("t{t}-{i}")).unwrap();
                    }
                })
            })
            .collect();
        for handle in threads {
            handle.join().unwrap();
        }
        assert_eq!(read(&dir, "z", None).unwrap().len(), 8 * 25);
    }
}
