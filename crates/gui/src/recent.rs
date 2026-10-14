//! Per-repo "recently viewed metarecords" list (a GUI-side concern, like the
//! input history — the daemon plays no part). A single plain-text file
//! `.metafolder/gui/recent`, one entry per line, **newest first**, each line
//! `<uuid-32hex>\t<iso8601>\t<rank>`. Unlike the append-only input history
//! this is a set: touching an already-present uuid moves it to the front,
//! refreshes its timestamp and adds one to its rank (no duplicates).
//!
//! The rank is zoxide's (doc "Frecent folders"): one per view, every rank
//! scaled by [`AGING`] whenever their total exceeds [`MAX_RANK_SUM`], and an
//! entry dropped once its rank falls under one — unless it is among the newest
//! [`MAX_ENTRIES`], which are always kept. What a rank is worth (the recency
//! weighting) is the reader's business, in the shipped `commands.js`. A line
//! of the older two-column form reads as rank 1.
//!
//! The file is ordinary trackable content (covered in practice by the default
//! `\.metafolder(/.*)?$` ignore pattern). Repo `.metafolder/` resolution is
//! shared with the input history (`history::metafolder_dir_of`).

use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// The newest entries, always kept whatever their rank.
pub const MAX_ENTRIES: usize = 200;

/// The total rank past which every rank is scaled by [`AGING`].
pub const MAX_RANK_SUM: f64 = 1000.0;

/// The factor applied to every rank when their total exceeds [`MAX_RANK_SUM`].
pub const AGING: f64 = 0.9;

const RECENT_FILE: &str = "gui/recent";

/// One viewed metarecord: its uuid (32-char lowercase hex), the ISO-8601
/// timestamp of the most recent view, and its rank (one per view, aged).
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub uuid: String,
    pub viewed_at: String,
    pub rank: f64,
}

/// Serializes read-modify-write cycles on the recent file. Process-local;
/// concurrent GUI instances rely on the atomic rename (last writer wins).
static LOCK: Mutex<()> = Mutex::new(());

/// Normalizes a uuid to 32-char lowercase hex (dashes stripped), or `Err` if it
/// is not a valid uuid. The charset (hex only, no `/`/`.`/whitespace) also makes
/// the stored line safe to split on the tab separator.
pub fn normalize_uuid(uuid: &str) -> Result<String, String> {
    let normalized: String =
        uuid.chars().filter(|&c| c != '-').flat_map(char::to_lowercase).collect();
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
/// existing entry for the same uuid, inserts it at the front with its rank plus
/// one, ages the ranks when their total exceeds [`MAX_RANK_SUM`] and rewrites
/// atomically. `viewed_at` must be a non-empty
/// single line without a tab.
pub fn touch(metafolder_dir: &Path, uuid: &str, viewed_at: &str) -> Result<(), String> {
    let uuid = normalize_uuid(uuid)?;
    if viewed_at.trim().is_empty() || viewed_at.contains('\t') || viewed_at.contains(['\n', '\r']) {
        return Err("a view timestamp must be a non-empty single line without a tab".to_string());
    }
    let _guard = LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    let mut entries = read_entries(metafolder_dir)?;
    let previous = entries.iter().position(|e| e.uuid == uuid).map(|i| entries.remove(i).rank);
    let rank = previous.unwrap_or(0.0) + 1.0;
    entries.insert(0, Entry { uuid, viewed_at: viewed_at.to_string(), rank });
    age(&mut entries);

    let file = recent_file(metafolder_dir);
    let dir = file.parent().expect("recent file has the gui dir as parent");
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create gui directory: {e}"))?;
    let mut content: String = entries
        .iter()
        .map(|e| {
            // Three decimals: aging would otherwise write float noise.
            format!("{}\t{}\t{}\n", e.uuid, e.viewed_at, (e.rank * 1000.0).round() / 1000.0)
        })
        .collect();
    content.shrink_to_fit();
    let tmp = file.with_extension("tmp");
    std::fs::write(&tmp, content).map_err(|e| format!("cannot write recent file: {e}"))?;
    std::fs::rename(&tmp, &file).map_err(|e| format!("cannot replace recent file: {e}"))?;
    Ok(())
}

/// Zoxide's aging: once the ranks add up to more than [`MAX_RANK_SUM`], every
/// rank is scaled by [`AGING`] and the entries left under one go — except the
/// newest [`MAX_ENTRIES`], so a burst of new views never empties the list.
fn age(entries: &mut Vec<Entry>) {
    if entries.iter().map(|e| e.rank).sum::<f64>() <= MAX_RANK_SUM {
        return;
    }
    for entry in entries.iter_mut() {
        entry.rank *= AGING;
    }
    let mut index = 0;
    entries.retain(|e| {
        index += 1;
        index <= MAX_ENTRIES || e.rank >= 1.0
    });
}

/// Raw parse of the recent file (no locking): newest-first lines
/// `<uuid>\t<viewed_at>[\t<rank>]`, a missing rank read as 1. Malformed lines
/// (no tab, bad uuid, a rank that is not a positive number) are skipped so a
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
            let (uuid, rest) = line.split_once('\t')?;
            let uuid = normalize_uuid(uuid).ok()?;
            let (viewed_at, rank) = match rest.split_once('\t') {
                Some((viewed_at, rank)) => (viewed_at, rank.parse::<f64>().ok()?),
                None => (rest, 1.0),
            };
            let valid = !viewed_at.is_empty() && rank.is_finite() && rank > 0.0;
            valid.then(|| Entry { uuid, viewed_at: viewed_at.to_string(), rank })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_metafolder(prefix: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("metafolder-tests")
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
            vec![Entry { uuid: a.clone(), viewed_at: "2026-08-15T10:00:00Z".into(), rank: 1.0 }]
        );
        let file = dir.join("gui/recent");
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            format!("{a}\t2026-08-15T10:00:00Z\t1\n")
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
                Entry { uuid: a, viewed_at: "2026-08-15T10:00:02Z".into(), rank: 2.0 },
                Entry { uuid: b, viewed_at: "2026-08-15T10:00:01Z".into(), rank: 1.0 },
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
        let got: Vec<String> = read(&dir, Some(2)).unwrap().into_iter().map(|e| e.uuid).collect();
        assert_eq!(got, vec![c, b]);
    }

    fn rank_of(dir: &Path, uuid: &str) -> f64 {
        read(dir, None).unwrap().into_iter().find(|e| e.uuid == uuid).unwrap().rank
    }

    #[test]
    fn test_a_line_without_rank_reads_as_one_view() {
        let dir = temp_metafolder("old_format");
        let (a, _, _) = uuids();
        std::fs::create_dir_all(dir.join("gui")).unwrap();
        std::fs::write(dir.join("gui/recent"), format!("{a}\t2026-08-15T10:00:00Z\n")).unwrap();
        assert_eq!(
            read(&dir, None).unwrap(),
            vec![Entry { uuid: a, viewed_at: "2026-08-15T10:00:00Z".into(), rank: 1.0 }]
        );
    }

    #[test]
    fn test_each_view_adds_one_to_the_rank() {
        let dir = temp_metafolder("rank");
        let (a, b, _) = uuids();
        touch(&dir, &a, "2026-08-15T10:00:00Z").unwrap();
        touch(&dir, &b, "2026-08-15T10:00:01Z").unwrap();
        touch(&dir, &a, "2026-08-15T10:00:02Z").unwrap();
        assert_eq!(rank_of(&dir, &a), 2.0);
        assert_eq!(rank_of(&dir, &b), 1.0);
        let text = std::fs::read_to_string(dir.join("gui/recent")).unwrap();
        assert_eq!(text, format!("{a}\t2026-08-15T10:00:02Z\t2\n{b}\t2026-08-15T10:00:01Z\t1\n"));
    }

    #[test]
    fn test_a_bad_rank_skips_the_line() {
        let dir = temp_metafolder("bad_rank");
        let (a, b, _) = uuids();
        std::fs::create_dir_all(dir.join("gui")).unwrap();
        let text = format!("{a}\t2026-08-15T10:00:00Z\tx\n{b}\t2026-08-15T10:00:00Z\t-1\n");
        std::fs::write(dir.join("gui/recent"), text).unwrap();
        assert_eq!(read(&dir, None).unwrap(), Vec::<Entry>::new());
    }

    #[test]
    fn test_an_often_viewed_entry_outlives_the_newest_max_entries() {
        let dir = temp_metafolder("frequent");
        let (a, _, _) = uuids();
        for _ in 0..3 {
            touch(&dir, &a, "2026-08-15T10:00:00Z").unwrap();
        }
        for i in 0..(MAX_ENTRIES + 5) {
            touch(&dir, &format!("{:032x}", i + 100), "2026-08-15T10:00:01Z").unwrap();
        }
        let got = read(&dir, None).unwrap();
        assert_eq!(got.len(), MAX_ENTRIES + 6);
        assert_eq!(got.last().unwrap().uuid, a);
    }

    #[test]
    fn test_aging_scales_ranks_and_drops_the_old_rare_entries() {
        let dir = temp_metafolder("aging");
        let (a, _, _) = uuids();
        // `a` holds most of the total rank; MAX_ENTRIES + 10 single views
        // after it bring the total one past MAX_RANK_SUM on the very last one.
        let heavy = MAX_RANK_SUM as usize - MAX_ENTRIES - 10 + 1;
        for _ in 0..heavy {
            touch(&dir, &a, "2026-08-15T10:00:00Z").unwrap();
        }
        // Ten single views that will fall out of the newest MAX_ENTRIES.
        for i in 0..10 {
            touch(&dir, &format!("{:032x}", i + 100), "2026-08-15T10:00:01Z").unwrap();
        }
        for i in 0..MAX_ENTRIES {
            touch(&dir, &format!("{:032x}", i + 1000), "2026-08-15T10:00:02Z").unwrap();
        }
        let got = read(&dir, None).unwrap();
        // The total exceeded MAX_RANK_SUM on the last touch: every rank was
        // scaled by AGING, the ten old single views fell under one and went;
        // the newest MAX_ENTRIES stay whatever their rank.
        assert_eq!(got.len(), MAX_ENTRIES + 1);
        assert!(got[..MAX_ENTRIES].iter().all(|e| (e.rank - AGING).abs() < 1e-9));
        assert_eq!(got[MAX_ENTRIES].uuid, a);
        assert!((got[MAX_ENTRIES].rank - heavy as f64 * AGING).abs() < 1e-6);
        assert!(got.iter().map(|e| e.rank).sum::<f64>() <= MAX_RANK_SUM);
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
