//! Ordering heuristic for `mf order` (spec-data-model "* CLI"): assigns an
//! integer `order-position` to the children of a folder so a "sort by position"
//! puts them in a sensible order — album tracks, series seasons, etc.
//!
//! Pure, deterministic, and unit-tested here; the HTTP orchestration (read the
//! children, write the fields) lives in `commands::order`. Files and directories
//! are numbered independently (two separate fields), so this function is called
//! once per kind.
//!
//! For every item we know its basename `name`, an optional integer ordering
//! `meta` (e.g. `mfr_meta_track`), an optional creation time `btime`, and an
//! optional `existing` position (already set — never overwritten). Positions are
//! assigned in three phases over a running maximum `maxpos` (`next` = maxpos+1):
//!
//! 1. metadata: items with `meta`, sorted by it; anchored at `next`, then
//!    `pos = anchor_pos + (meta − anchor_meta)` (gaps proportional, ties tie).
//! 2. name: items whose basename is `<prefix><number><suffix>` are clustered by
//!    identical `(prefix, suffix)` (the *last* digit run is the number). A
//!    cluster needs ≥2 members and a minimal consecutive number gap ≤ threshold
//!    (a larger gap means the number is an id/hash, not an order). An
//!    already-placed member (e.g. one positioned by metadata) pins the cluster;
//!    otherwise its lowest-numbered member is placed at `next`. Others:
//!    `pos = anchor_pos + (number − anchor_number)`.
//! 3. creation date: everything left, by `btime` ascending (missing btime last,
//!    then by name), each at `next`.
//!
//! Gaps are intentional (they mirror the number differences); positions are not
//! compacted.

/// The default minimal-gap threshold (phase 2): a cluster whose closest two
/// numbers differ by more than this is treated as ids/hashes, not an order.
pub const DEFAULT_MAX_GAP: i64 = 1000;

/// The written fields (files and directories are numbered independently).
pub const FIELD_FILE: &str = "order_position_file";
pub const FIELD_DIR: &str = "order_position_dir";

/// One child to be ordered.
#[derive(Debug, Clone)]
pub struct Item {
    /// Opaque identifier (the metarecord uuid), echoed back in the result.
    pub key: String,
    /// Basename, for the name heuristic.
    pub name: String,
    /// Ordering metadata (e.g. track number), if present.
    pub meta: Option<i64>,
    /// Creation time in ms, if the filesystem reports it.
    pub btime: Option<i64>,
    /// A position already recorded — respected and never rewritten.
    pub existing: Option<i64>,
}

/// A position to write for an item that had none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment {
    pub key: String,
    pub position: i64,
}

/// Splits `name` into `(prefix, suffix, number)` around its *last* run of ASCII
/// digits, or `None` when it has none (or the run overflows `i64`).
fn last_number(name: &str) -> Option<(String, String, i64)> {
    let bytes = name.as_bytes();
    let last_digit =
        name.char_indices().filter(|(_, c)| c.is_ascii_digit()).map(|(i, _)| i).next_back()?;
    let mut end = last_digit;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    let mut start = last_digit;
    while start > 0 && bytes[start - 1].is_ascii_digit() {
        start -= 1;
    }
    let number = name[start..end].parse::<i64>().ok()?;
    Some((name[..start].to_string(), name[end..].to_string(), number))
}

/// Assigns positions to the `items` that lack one, per the module algorithm.
/// Returns only the items to write (those with `existing == None`), sorted by
/// position then key for a stable output.
pub fn assign_positions(items: &[Item], threshold: i64) -> Vec<Assignment> {
    let n = items.len();
    // Position of each item once placed (pre-existing, or assigned by a phase).
    let mut placed: Vec<Option<i64>> = items.iter().map(|it| it.existing).collect();
    let mut maxpos: i64 = placed.iter().flatten().copied().max().unwrap_or(0);

    // ── Phase 1: metadata ────────────────────────────────────────────────
    let mut meta_idx: Vec<usize> =
        (0..n).filter(|&i| placed[i].is_none() && items[i].meta.is_some()).collect();
    if !meta_idx.is_empty() {
        meta_idx.sort_by(|&a, &b| {
            items[a]
                .meta
                .cmp(&items[b].meta)
                .then_with(|| items[a].name.cmp(&items[b].name))
                .then(a.cmp(&b))
        });
        let anchor_pos = maxpos + 1;
        let anchor_val = items[meta_idx[0]].meta.unwrap();
        for &i in &meta_idx {
            let pos = anchor_pos + (items[i].meta.unwrap() - anchor_val);
            placed[i] = Some(pos);
            maxpos = maxpos.max(pos);
        }
    }

    // ── Phase 2: name clusters ───────────────────────────────────────────
    let numinfo: Vec<Option<(String, String, i64)>> =
        items.iter().map(|it| last_number(&it.name)).collect();
    // Group indices by their (prefix, suffix) skeleton — a BTreeMap so the
    // iteration (hence the fallback order of unanchored clusters) is stable.
    let mut groups: std::collections::BTreeMap<(String, String), Vec<usize>> =
        std::collections::BTreeMap::new();
    for (i, info) in numinfo.iter().enumerate() {
        if let Some((prefix, suffix, _)) = info {
            groups.entry((prefix.clone(), suffix.clone())).or_default().push(i);
        }
    }
    let num = |i: usize| numinfo[i].as_ref().unwrap().2;

    struct Cluster {
        members: Vec<usize>,
        anchor: usize,
        anchored: bool,
        anchor_pos: i64,
    }
    let mut clusters: Vec<Cluster> = Vec::new();
    for members in groups.values() {
        if members.len() < 2 {
            continue;
        }
        // Unreasonable spread (ids/hashes): leave the members to the date phase.
        let mut nums: Vec<i64> = members.iter().map(|&i| num(i)).collect();
        nums.sort_unstable();
        nums.dedup();
        if nums.len() >= 2 && nums.windows(2).map(|w| w[1] - w[0]).min().unwrap() > threshold {
            continue;
        }
        // Nothing to do if every member is already placed.
        if members.iter().all(|&i| placed[i].is_some()) {
            continue;
        }
        let placed_members: Vec<usize> =
            members.iter().copied().filter(|&i| placed[i].is_some()).collect();
        let (anchor, anchored) = if let Some(&a) = placed_members.iter().min_by_key(|&&i| num(i)) {
            (a, true) // a member positioned earlier pins the cluster
        } else {
            (*members.iter().min_by_key(|&&i| num(i)).unwrap(), false)
        };
        let anchor_pos = if anchored { placed[anchor].unwrap() } else { 0 };
        clusters.push(Cluster { members: members.clone(), anchor, anchored, anchor_pos });
    }
    // Anchored clusters first (their positions are pinned), by anchor position;
    // then the unanchored ones in skeleton order (stable sort preserves it).
    clusters.sort_by(|a, b| b.anchored.cmp(&a.anchored).then(a.anchor_pos.cmp(&b.anchor_pos)));
    for c in &clusters {
        let (anchor_pos, anchor_num) = if c.anchored {
            (c.anchor_pos, num(c.anchor))
        } else {
            let ap = maxpos + 1;
            placed[c.anchor] = Some(ap);
            maxpos = maxpos.max(ap);
            (ap, num(c.anchor))
        };
        for &i in &c.members {
            if placed[i].is_some() {
                continue;
            }
            let pos = anchor_pos + (num(i) - anchor_num);
            placed[i] = Some(pos);
            maxpos = maxpos.max(pos);
        }
    }

    // ── Phase 3: creation date (leftovers) ───────────────────────────────
    let mut leftovers: Vec<usize> = (0..n).filter(|&i| placed[i].is_none()).collect();
    leftovers.sort_by(|&a, &b| {
        let key = |i: usize| (items[i].btime.is_none(), items[i].btime.unwrap_or(0), &items[i].name);
        key(a).cmp(&key(b))
    });
    for i in leftovers {
        maxpos += 1;
        placed[i] = Some(maxpos);
    }

    // Only items that had no pre-existing position are written back.
    let mut out: Vec<Assignment> = (0..n)
        .filter(|&i| items[i].existing.is_none())
        .map(|i| Assignment { key: items[i].key.clone(), position: placed[i].unwrap() })
        .collect();
    out.sort_by(|a, b| a.position.cmp(&b.position).then_with(|| a.key.cmp(&b.key)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(key: &str, name: &str, meta: Option<i64>, btime: Option<i64>) -> Item {
        Item { key: key.into(), name: name.into(), meta, btime, existing: None }
    }

    /// Collects the result into a name→position map (the key doubles as the name
    /// in these tests) for order-free assertions.
    fn positions(assignments: &[Assignment]) -> std::collections::HashMap<String, i64> {
        assignments.iter().map(|a| (a.key.clone(), a.position)).collect()
    }

    #[test]
    fn test_last_number_extraction() {
        assert_eq!(last_number("song1.avi"), Some(("song".into(), ".avi".into(), 1)));
        assert_eq!(last_number("supersong2"), Some(("supersong".into(), "".into(), 2)));
        assert_eq!(last_number("S01E05.mkv"), Some(("S01E".into(), ".mkv".into(), 5)));
        assert_eq!(last_number("README.md"), None);
    }

    #[test]
    fn test_spec_example() {
        // The example from the feature request (all files).
        let items = vec![
            item("plop.mp4", "plop.mp4", Some(1), None),
            item("arf.mp4", "arf.mp4", Some(1), None),
            item("plopplop.mp4", "plopplop.mp4", Some(2), None),
            item("song0.avi", "song0.avi", Some(3), None),
            item("song1.avi", "song1.avi", None, None),
            item("song3.avi", "song3.avi", None, None),
            item("supersong1.avi", "supersong1.avi", None, None),
            item("supersong2.avi", "supersong2.avi", None, None),
            item("README.md", "README.md", None, None),
        ];
        let pos = positions(&assign_positions(&items, DEFAULT_MAX_GAP));
        assert_eq!(pos["plop.mp4"], 1);
        assert_eq!(pos["arf.mp4"], 1);
        assert_eq!(pos["plopplop.mp4"], 2);
        assert_eq!(pos["song0.avi"], 3);
        assert_eq!(pos["song1.avi"], 4);
        assert_eq!(pos["song3.avi"], 6);
        assert_eq!(pos["supersong1.avi"], 7);
        assert_eq!(pos["supersong2.avi"], 8);
        assert_eq!(pos["README.md"], 9);
    }

    #[test]
    fn test_name_cluster_without_metadata() {
        // Series seasons: numbers drive the order, gaps proportional.
        let items = vec![
            item("Season 1", "Season 1", None, None),
            item("Season 2", "Season 2", None, None),
            item("Season 10", "Season 10", None, None),
        ];
        let pos = positions(&assign_positions(&items, DEFAULT_MAX_GAP));
        assert_eq!(pos["Season 1"], 1);
        assert_eq!(pos["Season 2"], 2);
        assert_eq!(pos["Season 10"], 10);
    }

    #[test]
    fn test_large_gap_is_treated_as_id_not_order() {
        // Numbers far apart (a hash/id): fall through to the date phase, ordered
        // by btime — not by the number.
        let items = vec![
            item("a", "img_4821.jpg", None, Some(200)),
            item("b", "img_995500.jpg", None, Some(100)),
        ];
        let pos = positions(&assign_positions(&items, DEFAULT_MAX_GAP));
        // b (earlier btime) first.
        assert_eq!(pos["b"], 1);
        assert_eq!(pos["a"], 2);
    }

    #[test]
    fn test_existing_positions_are_kept_and_anchor() {
        // song5 already positioned by the user; the name cluster anchors to it
        // and is not rewritten.
        let mut song5 = item("song5", "song5.mp3", None, None);
        song5.existing = Some(50);
        let items = vec![song5, item("song6", "song6.mp3", None, None)];
        let out = assign_positions(&items, DEFAULT_MAX_GAP);
        let pos = positions(&out);
        // song5 is not in the output (kept), song6 anchored to it: 50 + (6−5).
        assert!(!pos.contains_key("song5"), "existing is never rewritten");
        assert_eq!(pos["song6"], 51);
    }

    #[test]
    fn test_date_phase_orders_by_btime() {
        let items = vec![
            item("late", "zeta.bin", None, Some(300)),
            item("early", "alpha.bin", None, Some(100)),
            item("mid", "mu.bin", None, Some(200)),
        ];
        let pos = positions(&assign_positions(&items, DEFAULT_MAX_GAP));
        assert_eq!(pos["early"], 1);
        assert_eq!(pos["mid"], 2);
        assert_eq!(pos["late"], 3);
    }
}
