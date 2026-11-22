//! The repository-wide lookups of the daemon's workers (orphans, duplicates,
//! hashing, mounts, the watch flags), answered off the indexes: the records
//! holding a value come from its posting, those holding a field from its
//! counters, and only those records are read (doc "Store tables").

use std::collections::HashMap;

use anyhow::Result;
use metafolder_core::metarecord::Value;
use uuid::Uuid;

use crate::rows::{FieldRow, OrphanCandidate, StoredHashes};
use crate::store::Rows;

/// The metarecords holding `value` in `name` (`Nothing`: a `Nothing` row), in
/// id order: the value's posting. Not for a position (`tree_ref`,
/// `hierarchy_ref`), which has no posting.
pub fn holding(conn: &(impl Rows + ?Sized), name: &str, value: &Value) -> Result<Vec<Uuid>> {
    debug_assert!(value.position().is_none(), "a position has no posting");
    conn.uuids_of_ids(&conn.value_holder_ids(name, value)?)
}

/// Each metarecord holding `name`, with its first string value of it: the
/// field's holders, each read.
pub fn string_owners(conn: &(impl Rows + ?Sized), name: &str) -> Result<Vec<(Uuid, String)>> {
    let mut out = Vec::new();
    for uuid in conn.uuids_of_ids(&conn.holder_ids(name)?)? {
        if let Some(text) = strings(&conn.rows_named(uuid, name)?).into_iter().next() {
            out.push((uuid, text));
        }
    }
    Ok(out)
}

/// Every tracked file (`mfr_type = "file"`, placed) with its `mfr_size`: the
/// posting of `file`, each of its records read.
pub fn tracked_files_with_size(conn: &(impl Rows + ?Sized)) -> Result<Vec<(Uuid, i64)>> {
    let files = conn.value_holder_ids("mfr_type", &Value::String("file".into()))?;
    let mut out = Vec::new();
    for uuid in conn.uuids_of_ids(&files)? {
        let rows = conn.rows(uuid)?;
        let placed = rows
            .iter()
            .filter(|r| r.name == "mfr_path" && matches!(r.value, Value::TreeRef { .. }))
            .count();
        for size in ints(&rows, "mfr_size") {
            for _ in 0..placed {
                out.push((uuid, size));
            }
        }
    }
    Ok(out)
}

/// The orphans (`mfr_path` = `Nothing`) that carry a size and both hashes:
/// the holders of `nothing` in `mfr_path`, each of its records read.
pub fn hashed_orphans(conn: &(impl Rows + ?Sized)) -> Result<Vec<OrphanCandidate>> {
    let mut out = Vec::new();
    for uuid in holding(conn, "mfr_path", &Value::Nothing)? {
        let rows = conn.rows(uuid)?;
        let named = |name: &str| -> Vec<FieldRow> {
            rows.iter().filter(|r| r.name == name).cloned().collect()
        };
        for size in ints(&rows, "mfr_size") {
            for partial in strings(&named("mfr_partial_hash")) {
                for full in strings(&named("mfr_full_hash")) {
                    out.push(OrphanCandidate {
                        uuid,
                        size,
                        partial_hash: partial.clone(),
                        full_hash: full,
                    });
                }
            }
        }
    }
    Ok(out)
}

/// The stored hashes of `uuids`, those that have any.
pub fn stored_hashes(
    conn: &(impl Rows + ?Sized),
    uuids: impl IntoIterator<Item = Uuid>,
) -> Result<HashMap<Uuid, StoredHashes>> {
    let mut out = HashMap::new();
    for uuid in uuids {
        if let Some(hashes) = StoredHashes::from_rows(&conn.rows(uuid)?) {
            out.insert(uuid, hashes);
        }
    }
    Ok(out)
}

/// The `Int` values of `name` among `rows`, in row order.
pub fn ints(rows: &[FieldRow], name: &str) -> Vec<i64> {
    rows.iter()
        .filter(|r| r.name == name)
        .filter_map(|r| match r.value {
            Value::Int(n) => Some(n),
            _ => None,
        })
        .collect()
}

/// The string values among `rows`, in row order.
fn strings(rows: &[FieldRow]) -> Vec<String> {
    rows.iter()
        .filter_map(|r| match &r.value {
            Value::String(s) => Some(s.clone()),
            _ => None,
        })
        .collect()
}
