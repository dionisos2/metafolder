//! Keyset (cursor-based) pagination (spec-data-model "Pagination"). The
//! cursor is an opaque base64(JSON) token carrying the sort-key values and
//! UUID of the last returned item, plus a hash of the query/sort context so
//! that a cursor is only accepted by the request shape that produced it.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::ApiError;

#[derive(Debug, Serialize, Deserialize)]
pub struct Cursor {
    /// Sort-key values of the last returned entry (empty for UUID-only order).
    #[serde(default)]
    pub keys: Vec<serde_json::Value>,
    /// UUID of the last returned entry (32-char hex).
    pub uuid: String,
    /// Hash of the (query, sort) context.
    pub h: u64,
}

/// Hashes the request context a cursor is bound to.
pub fn context_hash(parts: &[&str]) -> u64 {
    xxhash_rust::xxh3::xxh3_64(parts.join("\u{1f}").as_bytes())
}

pub fn encode(cursor: &Cursor) -> String {
    URL_SAFE_NO_PAD.encode(serde_json::to_vec(cursor).expect("cursor serialization"))
}

/// Decodes a cursor and verifies it matches the current request context.
pub fn decode(token: &str, expected_hash: u64) -> Result<Cursor, ApiError> {
    let bytes = URL_SAFE_NO_PAD
        .decode(token)
        .map_err(|_| ApiError::bad_request("invalid cursor"))?;
    let cursor: Cursor =
        serde_json::from_slice(&bytes).map_err(|_| ApiError::bad_request("invalid cursor"))?;
    if cursor.h != expected_hash {
        return Err(ApiError::bad_request(
            "invalid cursor: it was issued for a different query or sort",
        ));
    }
    Ok(cursor)
}

impl Cursor {
    pub fn last_uuid(&self) -> Result<Uuid, ApiError> {
        Uuid::parse_str(&self.uuid).map_err(|_| ApiError::bad_request("invalid cursor"))
    }
}

/// The wrapped response shape used when `limit` is present.
#[derive(Debug, Serialize)]
pub struct Page<T> {
    pub results: Vec<T>,
    pub next_cursor: Option<String>,
}
