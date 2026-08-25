//! Per-repo "recently viewed metarecords" list (a GUI-side concern, like the
//! input history — the daemon plays no part). A single plain-text file
//! `.metafolder/gui/recent`, one entry per line, **newest first**, each line
//! `<uuid-32hex>\t<iso8601>`. Unlike the append-only input history this is an
//! LRU set: touching an already-present uuid moves it to the front and refreshes
//! its timestamp (no duplicates), so re-viewing never evicts distinct records.
//!
//! The file is ordinary trackable content (covered in practice by the default
//! `\.metafolder(/.*)?$` ignore pattern). Repo `.metafolder/` resolution is
//! shared with the input history (`history::metafolder_dir_of`).

use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Entries kept; older ones are dropped on touch.
pub const MAX_ENTRIES: usize = 200;

const RECENT_FILE: &str = "gui/recent";

/// One viewed metarecord: its uuid (32-char lowercase hex) and the ISO-8601
/// timestamp of the most recent view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub uuid: String,
    pub viewed_at: String,
}

/// Serializes read-modify-write cycles on the recent file. Process-local;
/// concurrent GUI instances rely on the atomic rename (last writer wins).
static LOCK: Mutex<()> = Mutex::new(());

/// Normalizes a uuid to 32-char lowercase hex (dashes stripped), or `Err` if it
/// is not a valid uuid. The charset (hex only, no `/`/`.`/whitespace) also makes
/// the stored line safe to split on the tab separator.
pub fn normalize_uuid(uuid: &str) -> Result<String, String> {
    let normalized: String = uuid.chars().filter(|&c| c != '-').flat_map(char::to_lowercase).collect();
    let ok = normalized.len() == 32 && normalized.bytes().all(|b| b.is_ascii_hexdigit());
    if ok {
        Ok(normalized)
    } else {
        Err(format!("invalid metarecord uuid '{uuid}' (expected 32 hex digits)"))
    }
}

fn recent_file(metafolder_dir: &Path) -> PathBuf {
    metafolder_dir.join(RECENT_FILE)
}

/// Reads the recent entries, newest first. With `limit`, only the newest N are
/// returned. A missing file is an empty list.
pub fn read(metafolder_dir: &Path, limit: Option<usize>) -> Result<Vec<Entry>, String> {
    let _guard = LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    let mut entries = read_entries(metafolder_dir)?;
    if let Some(limit) = limit {
        entries.truncate(limit);
    }
    Ok(entries)
}

/// Records a view of `uuid` at `viewed_at` (an ISO-8601 timestamp): removes any
/// existing entry for the same uuid, inserts it at the front, caps to the newest
/// [`MAX_ENTRIES`] and rewrites atomically. `viewed_at` must be a non-empty
/// single line without a tab.
pub fn touch(metafolder_dir: &Path, uuid: &str, viewed_at: &str) -> Result<(), String> {
    let uuid = normalize_uuid(uuid)?;
    if viewed_at.trim().is_empty() || viewed_at.contains('\t') || viewed_at.contains(['\n', '\r']) {
        return Err("a view timestamp must be a non-empty single line without a tab".to_string());
    }
    let _guard = LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    let mut entries = read_entries(metafolder_dir)?;
    entries.retain(|e| e.uuid != uuid);
    entries.insert(0, Entry { uuid, viewed_at: viewed_at.to_string() });
    entries.truncate(MAX_ENTRIES);

    let file = recent_file(metafolder_dir);
    let dir = file.parent().expect("recent file has the gui dir as parent");
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create gui directory: {e}"))?;
    let mut content: String =
        entries.iter().map(|e| format!("{}\t{}\n", e.uuid, e.viewed_at)).collect();
    content.shrink_to_fit();
    let tmp = file.with_extension("tmp");
    std::fs::write(&tmp, content).map_err(|e| format!("cannot write recent file: {e}"))?;
    std::fs::rename(&tmp, &file).map_err(|e| format!("cannot replace recent file: {e}"))?;
    Ok(())
}

/// Raw parse of the recent file (no locking): newest-first lines
/// `<uuid>\t<viewed_at>`. Malformed lines (no tab, bad uuid) are skipped so a
/// hand-edited file stays well-formed.
fn read_entries(metafolder_dir: &Path) -> Result<Vec<Entry>, String> {
    let file = recent_file(metafolder_dir);
    let bytes = match std::fs::read(&file) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("cannot read recent file: {e}")),
    };
    Ok(String::from_utf8_lossy(&bytes)
        .lines()
        .filter_map(|line| {
            let (uuid, viewed_at) = line.split_once('\t')?;
            let uuid = normalize_uuid(uuid).ok()?;
            (!viewed_at.is_empty()).then(|| Entry { uuid, viewed_at: viewed_at.to_string() })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_metafolder(prefix: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("metafolder-tests")
            .join(format!("metafolder_gui_recent_{prefix}_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn uuids() -> (String, String, String) {
        (
            "00000000000000000000000000000001".to_string(),
            "00000000000000000000000000000002".to_string(),
            "00000000000000000000000000000003".to_string(),
        )
    }

    #[test]
    fn test_normalize_uuid_table() {
        assert_eq!(
            normalize_uuid("00000000-0000-0000-0000-000000000001").unwrap(),
            "00000000000000000000000000000001"
        );
        assert_eq!(
            normalize_uuid("ABCDEF00000000000000000000000001").unwrap(),
            "abcdef00000000000000000000000001"
        );
        for bad in ["", "xyz", "0001", &"0".repeat(33), "0000000000000000000000000000000g"] {
            assert!(normalize_uuid(bad).is_err(), "'{bad}' should be rejected");
        }
    }

    #[test]
    fn test_read_missing_file_is_empty() {
        let dir = temp_metafolder("missing");
        assert_eq!(read(&dir, None).unwrap(), Vec::<Entry>::new());
    }

    #[test]
    fn test_touch_then_read_roundtrip_under_gui_recent() {
        let dir = temp_metafolder("roundtrip");
        let (a, _, _) = uuids();
        touch(&dir, &a, "2026-08-15T10:00:00Z").unwrap();
        assert_eq!(
            read(&dir, None).unwrap(),
            vec![Entry { uuid: a.clone(), viewed_at: "2026-08-15T10:00:00Z".into() }]
        );
        let file = dir.join("gui/recent");
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            format!("{a}\t2026-08-15T10:00:00Z\n")
        );
    }

    #[test]
    fn test_entries_are_newest_first() {
        let dir = temp_metafolder("order");
        let (a, b, c) = uuids();
        touch(&dir, &a, "2026-08-15T10:00:00Z").unwrap();
        touch(&dir, &b, "2026-08-15T10:00:01Z").unwrap();
        touch(&dir, &c, "2026-08-15T10:00:02Z").unwrap();
        let got: Vec<String> = read(&dir, None).unwrap().into_iter().map(|e| e.uuid).collect();
        assert_eq!(got, vec![c, b, a]);
    }

    #[test]
    fn test_retouch_moves_to_front_without_duplicate_and_updates_timestamp() {
        let dir = temp_metafolder("mtf");
        let (a, b, _) = uuids();
        touch(&dir, &a, "2026-08-15T10:00:00Z").unwrap();
        touch(&dir, &b, "2026-08-15T10:00:01Z").unwrap();
        touch(&dir, &a, "2026-08-15T10:00:02Z").unwrap();
        assert_eq!(
            read(&dir, None).unwrap(),
            vec![
                Entry { uuid: a, viewed_at: "2026-08-15T10:00:02Z".into() },
                Entry { uuid: b, viewed_at: "2026-08-15T10:00:01Z".into() },
            ]
        );
    }

    #[test]
    fn test_uuid_is_normalized_and_deduped_across_forms() {
        let dir = temp_metafolder("norm");
        touch(&dir, "00000000-0000-0000-0000-000000000001", "2026-08-15T10:00:00Z").unwrap();
        touch(&dir, "00000000000000000000000000000001", "2026-08-15T10:00:01Z").unwrap();
        let got = read(&dir, None).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].uuid, "00000000000000000000000000000001");
        assert_eq!(got[0].viewed_at, "2026-08-15T10:00:01Z");
    }

    #[test]
    fn test_limit_returns_the_newest_n() {
        let dir = temp_metafolder("limit");
        let (a, b, c) = uuids();
        touch(&dir, &a, "2026-08-15T10:00:00Z").unwrap();
        touch(&dir, &b, "2026-08-15T10:00:01Z").unwrap();
        touch(&dir, &c, "2026-08-15T10:00:02Z").unwrap();
        let got: Vec<String> =
            read(&dir, Some(2)).unwrap().into_iter().map(|e| e.uuid).collect();
        assert_eq!(got, vec![c, b]);
    }

    #[test]
    fn test_cap_keeps_the_newest_max_entries() {
        let dir = temp_metafolder("cap");
        for i in 0..(MAX_ENTRIES + 5) {
            let uuid = format!("{i:032x}");
            touch(&dir, &uuid, "2026-08-15T10:00:00Z").unwrap();
        }
        let got = read(&dir, None).unwrap();
        assert_eq!(got.len(), MAX_ENTRIES);
        // Newest first: the last touched is at the front, the oldest 5 are gone.
        assert_eq!(got.first().unwrap().uuid, format!("{:032x}", MAX_ENTRIES + 4));
        assert_eq!(got.last().unwrap().uuid, format!("{:032x}", 5));
    }

    #[test]
    fn test_invalid_uuid_creates_nothing() {
        let dir = temp_metafolder("bad_uuid");
        assert!(touch(&dir, "not-a-uuid", "2026-08-15T10:00:00Z").is_err());
        assert!(!dir.join("gui/recent").exists());
    }

    #[test]
    fn test_bad_timestamp_rejected() {
        let dir = temp_metafolder("bad_ts");
        let (a, _, _) = uuids();
        for ts in ["", "  ", "a\tb", "a\nb"] {
            assert!(touch(&dir, &a, ts).is_err(), "timestamp {ts:?} accepted");
        }
        assert!(!dir.join("gui/recent").exists());
    }
}
