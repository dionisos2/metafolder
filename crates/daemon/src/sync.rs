//! Cross-repo synchronisation state (spec-sync.org). Sync state — the links
//! between metarecords of two repositories and the snapshot of their common
//! field state at the last sync — lives *outside the data model*, in a
//! per-pair SQLite file (`sync-<uuid_a>-<uuid_b>.sqlite`) held under one repo's
//! `internal/`. This module is the storage layer over that file; the cross-repo
//! orchestration (status truth table, candidates, records inline) lives in the
//! HTTP handlers, which hold both repositories' connections.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use uuid::Uuid;

use metafolder_core::metarecord::Value;

use crate::db::{self, uuid_to_bytes};

pub const FORMAT_VERSION: &str = "1";

/// Orders a pair of repo UUIDs into canonical `(a, b)` roles: the
/// lexicographically smaller 32-char-hex UUID is repo A. `None` when the two
/// UUIDs are equal (a repo cannot be paired with itself).
pub fn canonical_pair(x: Uuid, y: Uuid) -> Option<(Uuid, Uuid)> {
    match x.as_bytes().cmp(y.as_bytes()) {
        std::cmp::Ordering::Less => Some((x, y)),
        std::cmp::Ordering::Greater => Some((y, x)),
        std::cmp::Ordering::Equal => None,
    }
}

/// The sync-database file name for a canonical pair.
pub fn sync_db_filename(a: Uuid, b: Uuid) -> String {
    format!("sync-{}-{}.sqlite", a.as_simple(), b.as_simple())
}

/// Opens (creating the schema if new) a pair's sync database. Never WAL: one
/// copy may live on a network filesystem (spec-sync "The sync database").
pub fn open(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)
        .with_context(|| format!("Failed to open sync database at {path:?}"))?;
    conn.pragma_update(None, "journal_mode", "DELETE")?;
    conn.pragma_update(None, "synchronous", "FULL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS link (
             uuid       BLOB PRIMARY KEY,
             record_a   BLOB NOT NULL UNIQUE,
             record_b   BLOB NOT NULL UNIQUE,
             version_a  INTEGER,
             version_b  INTEGER
         );
         CREATE TABLE IF NOT EXISTS snapshot_field (
             link_uuid   BLOB NOT NULL REFERENCES link(uuid) ON DELETE CASCADE,
             field_name  TEXT NOT NULL,
             value_type  TEXT NOT NULL,
             value_text  TEXT,
             value_int   INTEGER,
             value_real  REAL,
             value_uuid  BLOB,
             value_uuid_b BLOB,
             value_ref_repo BLOB,
             value_name  TEXT
         );
         CREATE INDEX IF NOT EXISTS idx_snapshot_link ON snapshot_field(link_uuid);",
    )
    .context("Failed to initialise sync database schema")?;
    Ok(conn)
}

/// Writes the identification `meta` rows into a freshly created sync database.
pub fn write_meta(conn: &Connection, a: Uuid, b: Uuid, host: Uuid) -> Result<()> {
    for (k, v) in [
        ("format_version", FORMAT_VERSION.to_string()),
        ("repo_a", a.as_simple().to_string()),
        ("repo_b", b.as_simple().to_string()),
        ("host", host.as_simple().to_string()),
    ] {
        conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![k, v],
        )?;
    }
    Ok(())
}

/// Reads a `meta` value.
pub fn read_meta(conn: &Connection, key: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| r.get(0))
        .optional()?)
}

/// The result of locating a pair's sync database across the two loaded repos'
/// `internal/` directories (spec-sync "Location and discovery").
pub enum Located {
    /// Found in exactly one repo's `internal/`.
    Found(PathBuf),
    /// Present in neither: the pair has no sync state yet.
    Absent,
    /// Present in both — ambiguous; the daemon never merges (409).
    Ambiguous,
}

/// Locates the sync-database file for canonical pair `(a, b)` given the two
/// repos' `internal/` directories.
pub fn locate(a_internal: &Path, b_internal: &Path, a: Uuid, b: Uuid) -> Located {
    let name = sync_db_filename(a, b);
    let in_a = a_internal.join(&name);
    let in_b = b_internal.join(&name);
    match (in_a.exists(), in_b.exists()) {
        (true, true) => Located::Ambiguous,
        (true, false) => Located::Found(in_a),
        (false, true) => Located::Found(in_b),
        (false, false) => Located::Absent,
    }
}

/// One link row (spec-sync `link` table).
#[derive(Debug, Clone)]
pub struct Link {
    pub uuid: Uuid,
    pub record_a: Uuid,
    pub record_b: Uuid,
    pub version_a: Option<u64>,
    pub version_b: Option<u64>,
}

fn row_to_link(r: &rusqlite::Row<'_>) -> rusqlite::Result<Link> {
    Ok(Link {
        uuid: bytes_to_uuid(r.get::<_, Vec<u8>>(0)?),
        record_a: bytes_to_uuid(r.get::<_, Vec<u8>>(1)?),
        record_b: bytes_to_uuid(r.get::<_, Vec<u8>>(2)?),
        version_a: r.get::<_, Option<i64>>(3)?.map(|v| v as u64),
        version_b: r.get::<_, Option<i64>>(4)?.map(|v| v as u64),
    })
}

fn bytes_to_uuid(b: Vec<u8>) -> Uuid {
    Uuid::from_slice(&b).unwrap_or(Uuid::nil())
}

/// All links, ordered by UUID.
pub fn list_links(conn: &Connection) -> Result<Vec<Link>> {
    let mut stmt = conn
        .prepare("SELECT uuid, record_a, record_b, version_a, version_b FROM link ORDER BY uuid")?;
    let links = stmt.query_map([], row_to_link)?.collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(links)
}

/// One link by its UUID.
pub fn get_link(conn: &Connection, uuid: Uuid) -> Result<Option<Link>> {
    Ok(conn
        .query_row(
            "SELECT uuid, record_a, record_b, version_a, version_b FROM link WHERE uuid = ?1",
            params![uuid_to_bytes(uuid)],
            row_to_link,
        )
        .optional()?)
}

/// The link (if any) whose given-side record is `record`.
pub fn link_for_record(conn: &Connection, side: Side, record: Uuid) -> Result<Option<Link>> {
    let col = match side {
        Side::A => "record_a",
        Side::B => "record_b",
    };
    Ok(conn
        .query_row(
            &format!(
                "SELECT uuid, record_a, record_b, version_a, version_b FROM link WHERE {col} = ?1"
            ),
            params![uuid_to_bytes(record)],
            row_to_link,
        )
        .optional()?)
}

/// Canonical role of a repo within a pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    A,
    B,
}

/// Creates a link with no versions and no snapshot (state `never_synced`).
/// The two `UNIQUE` constraints reject a record already linked in this pair.
pub fn create_link(conn: &Connection, record_a: Uuid, record_b: Uuid) -> Result<Link> {
    let uuid = Uuid::new_v4();
    conn.execute(
        "INSERT INTO link (uuid, record_a, record_b) VALUES (?1, ?2, ?3)",
        params![uuid_to_bytes(uuid), uuid_to_bytes(record_a), uuid_to_bytes(record_b)],
    )?;
    Ok(Link { uuid, record_a, record_b, version_a: None, version_b: None })
}

/// Deletes a link and (by cascade) its snapshot rows.
pub fn delete_link(conn: &Connection, uuid: Uuid) -> Result<bool> {
    let n = conn.execute("DELETE FROM link WHERE uuid = ?1", params![uuid_to_bytes(uuid)])?;
    Ok(n > 0)
}

/// One snapshot field: the common value at the last sync, in dual perspective
/// (spec-sync "Ref and TreeRef fields in the snapshot"). `value` holds repo A's
/// perspective; `value_uuid_b` the B-perspective UUID for `ref`/`tree_ref`.
#[derive(Debug, Clone)]
pub struct SnapshotField {
    pub name: String,
    pub value: Value,
    pub value_uuid_b: Option<Uuid>,
}

/// The snapshot rows of a link.
pub fn read_snapshot(conn: &Connection, link: Uuid) -> Result<Vec<SnapshotField>> {
    let mut stmt = conn.prepare(
        "SELECT field_name, value_type, value_text, value_int, value_real,
                value_uuid, value_ref_repo, value_name, value_uuid_b
         FROM snapshot_field WHERE link_uuid = ?1",
    )?;
    let rows = stmt
        .query_map(params![uuid_to_bytes(link)], |r| {
            let value = db::decode_value(
                &r.get::<_, String>(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
            )
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(e.into()))?;
            let value_uuid_b: Option<Vec<u8>> = r.get(8)?;
            Ok(SnapshotField {
                name: r.get(0)?,
                value,
                value_uuid_b: value_uuid_b.map(bytes_to_uuid),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// One entry of a sync-commit batch: set a link's recorded versions and replace
/// its snapshot.
pub struct Commit {
    pub link: Uuid,
    pub version_a: u64,
    pub version_b: u64,
    pub snapshot: Vec<SnapshotField>,
}

/// Applies a batch of sync-commits in a single transaction (spec-sync
/// `POST …/links/commit`): per commit, update the link's versions and replace
/// its `snapshot_field` rows.
pub fn commit_batch(conn: &mut Connection, commits: &[Commit]) -> Result<()> {
    let tx = conn.transaction()?;
    for c in commits {
        tx.execute(
            "UPDATE link SET version_a = ?2, version_b = ?3 WHERE uuid = ?1",
            params![uuid_to_bytes(c.link), c.version_a as i64, c.version_b as i64],
        )?;
        tx.execute(
            "DELETE FROM snapshot_field WHERE link_uuid = ?1",
            params![uuid_to_bytes(c.link)],
        )?;
        for f in &c.snapshot {
            let e = db::encode_value(&f.value);
            tx.execute(
                "INSERT INTO snapshot_field
                     (link_uuid, field_name, value_type, value_text, value_int,
                      value_real, value_uuid, value_uuid_b, value_ref_repo, value_name)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
                params![
                    uuid_to_bytes(c.link),
                    f.name,
                    e.value_type,
                    e.text,
                    e.int,
                    e.real,
                    e.uuid,
                    f.value_uuid_b.map(uuid_to_bytes),
                    e.ref_repo,
                    e.name,
                ],
            )?;
        }
    }
    tx.commit()?;
    Ok(())
}
