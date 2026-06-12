//! Watch/ignore eligibility (spec-file-tracking "Watch and Ignore"): decides
//! whether a repo-root-relative path should be tracked, from the `mf_watch`
//! and `mf_ignore` fields inherited along the `mfr_path` ancestor chain.

use anyhow::{Context, Result};
use rusqlite::Connection;
use uuid::Uuid;

use metafolder_core::metarecord::Value;

use crate::db;
use crate::tree_cache::TreeCache;

/// Evaluates eligibility for `rel_path` (repo-root-relative, `/`-separated,
/// leading slash; `""` is the root itself).
pub fn is_eligible(conn: &Connection, cache: &mut TreeCache, rel_path: &str) -> Result<bool> {
    let comps: Vec<&str> = rel_path.split('/').collect();
    // Prefixes from the root down: "" for the root, then "/a", "/a/b", …
    let prefixes: Vec<String> = (0..comps.len()).map(|i| comps[..=i].join("/")).collect();

    // Metarecords existing along the path. A TreeRef child requires its parent
    // metarecord, so the chain stops at the first unresolved prefix.
    let mut chain: Vec<(usize, Uuid)> = Vec::new();
    for (i, prefix) in prefixes.iter().enumerate() {
        match cache.resolve_path(conn, "mfr_path", prefix)? {
            Some(uuid) => chain.push((i, uuid)),
            None => break,
        }
    }

    let full_idx = prefixes.len() - 1;
    // The path's own metarecord, when it already exists.
    let own_entry: Option<Uuid> =
        chain.last().and_then(|(i, u)| (*i == full_idx).then_some(*u));

    // Steps 1–2: nearest metarecord (including the path itself) defining mf_watch.
    let mut watch: Option<(Uuid, bool)> = None;
    for (_, uuid) in chain.iter().rev() {
        if let Some(value) = bool_field(conn, *uuid, "mf_watch")? {
            watch = Some((*uuid, value));
            break;
        }
    }
    let Some((watch_entry, watch_value)) = watch else {
        return Ok(false); // No mf_watch anywhere: opt-in default.
    };
    if !watch_value {
        return Ok(false);
    }
    // Step 3: mf_watch set directly on the metarecord → tracked unconditionally.
    if own_entry == Some(watch_entry) {
        return Ok(true);
    }

    // Steps 4–5: nearest strict ancestor with mf_ignore rows provides the
    // effective pattern set (sets are replaced, never merged).
    for (i, uuid) in chain.iter().rev() {
        if *i == full_idx && own_entry == Some(*uuid) {
            continue; // The entry itself is excluded from the ignore search.
        }
        let patterns = string_fields(conn, *uuid, "mf_ignore")?;
        if patterns.is_empty() {
            continue;
        }
        for pattern in &patterns {
            let re = regex::Regex::new(pattern)
                .with_context(|| format!("invalid mf_ignore pattern '{pattern}'"))?;
            if re.is_match(rel_path) {
                return Ok(false);
            }
        }
        return Ok(true);
    }
    Ok(true)
}

/// First Bool value of a field on a metarecord (Nothing rows do not count).
fn bool_field(conn: &Connection, uuid: Uuid, name: &str) -> Result<Option<bool>> {
    Ok(db::get_field_rows_named(conn, uuid, name)?
        .into_iter()
        .find_map(|r| match r.value {
            Value::Bool(b) => Some(b),
            _ => None,
        }))
}

/// All String values of a field on a metarecord.
fn string_fields(conn: &Connection, uuid: Uuid, name: &str) -> Result<Vec<String>> {
    Ok(db::get_field_rows_named(conn, uuid, name)?
        .into_iter()
        .filter_map(|r| match r.value {
            Value::String(s) => Some(s),
            _ => None,
        })
        .collect())
}
