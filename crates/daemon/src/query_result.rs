//! The shape of a query's *answer*: the sort specification a request carries,
//! and the assembly of a result page's fields.
//!
//! Neither runs a query. What reads the `field` table here reads it *for a list
//! of uuids* — which is all the SQL layer does on the serving path now
//! (spec-indexing "No operand runs in SQL").

use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use metafolder_core::metarecord::{Field, MetaRecord};

use crate::db;
use crate::error::ApiError;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SortOrder {
    Asc,
    Desc,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SortKey {
    pub field: String,
    #[serde(default = "default_order")]
    pub order: SortOrder,
}

fn default_order() -> SortOrder {
    SortOrder::Asc
}

/// Assembles the `select`-projected JSON objects for a page of result UUIDs,
/// polling `cancel` every few hundred rows so a long assembly (the dominant cost
/// of a `select=*` query over many matches) can be stopped (spec-tasks
/// "Cancellation"). `fields_filter = None` keeps every field; `Some(list)` keeps
/// only the named ones. Pass `&|| false` for uncancellable callers.
pub fn assemble_selected(
    conn: &Connection,
    uuids: &[Uuid],
    fields_filter: Option<&[String]>,
    cancel: &dyn Fn() -> bool,
) -> Result<Vec<serde_json::Value>, ApiError> {
    // Batched reads: the whole page's versions and field rows in a couple of
    // `IN (…)` scans, not a query per metarecord.
    let versions = db::versions_for(conn, uuids)?;
    let mut rows = db::field_rows_for(conn, uuids)?;
    let mut objects = Vec::with_capacity(uuids.len());
    for (i, &uuid) in uuids.iter().enumerate() {
        if i % 256 == 0 && cancel() {
            return Err(ApiError::conflict("query cancelled"));
        }
        let version = *versions
            .get(&uuid)
            .ok_or_else(|| ApiError::not_found(format!("Metarecord not found: {uuid}")))?;
        let fields: Vec<Field> = rows
            .remove(&uuid)
            .unwrap_or_default()
            .into_iter()
            .filter(|r| fields_filter.is_none_or(|f| f.contains(&r.name)))
            .map(|r| Field { id: Some(r.id), name: r.name, value: r.value })
            .collect();
        let metarecord = MetaRecord { uuid, version, fields };
        objects.push(serde_json::to_value(metarecord).expect("metarecord serialization"));
    }
    Ok(objects)
}

/// The case-insensitive ordered-substring regex for OSM `Direct` mode:
/// `["con", "def"]` → `(?i)con.*def`. Terms are regex-escaped; empty `terms`
/// yields `(?i)`, which matches any string ("present" semantics). Unanchored
/// (`REGEXP` searches), so this is exactly "con then def, non-overlapping".
pub fn osm_regex(terms: &[String]) -> String {
    let body = terms.iter().map(|t| regex::escape(t)).collect::<Vec<_>>().join(".*");
    format!("(?i){body}")
}
