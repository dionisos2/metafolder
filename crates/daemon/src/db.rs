//! Low-level SQLite operations: connection setup, schema, row encoding and
//! unlogged read helpers. All writes must go through [`crate::log::Writer`]
//! so that the event log stays consistent with the data tables.

use std::path::Path;

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use uuid::Uuid;

use metafolder_core::metarecord::{Field, MetaRecord, TreeName, Value, ZERO_UUID};
use metafolder_core::sync::MutexExt;

use crate::error::DomainError;

/// One row of the `field` table, decoded.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldRow {
    pub id: i64,
    pub name: String,
    pub value: Value,
}

// ── UUID ↔ BLOB helpers ───────────────────────────────────────────────────────

pub fn uuid_to_bytes(uuid: Uuid) -> Vec<u8> {
    uuid.as_bytes().to_vec()
}

pub fn bytes_to_uuid(bytes: Vec<u8>) -> Result<Uuid> {
    let arr: [u8; 16] =
        bytes.try_into().map_err(|_| anyhow::anyhow!("Invalid UUID blob: expected 16 bytes"))?;
    Ok(Uuid::from_bytes(arr))
}

// ── Connection setup ──────────────────────────────────────────────────────────

fn configure_connection(conn: &Connection) -> Result<()> {
    // An exclusive lock for the whole connection lifetime prevents a second
    // daemon instance from loading the same repository (spec-main invariant).
    conn.pragma_update(None, "locking_mode", "EXCLUSIVE").context("Failed to set locking_mode")?;
    // WAL requires shared-memory files, which network filesystems do not
    // support; fall back to DELETE journal mode there (spec-platform).
    let wal =
        conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| row.get::<_, String>(0));
    if wal.is_err() {
        conn.pragma_update(None, "journal_mode", "DELETE").context("Failed to set journal_mode")?;
    }
    conn.pragma_update(None, "foreign_keys", true).context("Failed to enable foreign keys")?;
    // The write and navigation hot paths go through `prepare_cached`; keep
    // enough room so the recurring statements never evict each other.
    conn.set_prepared_statement_cache_capacity(64);

    // REGEXP user-defined function backing the `Matches` query operator.
    // Compiled patterns are cached: a scan calls the UDF once per row, and
    // recompiling the regex each time dominates the query cost.
    let regex_cache: std::sync::Mutex<std::collections::HashMap<String, regex::Regex>> =
        std::sync::Mutex::new(std::collections::HashMap::new());
    conn.create_scalar_function(
        "REGEXP",
        2,
        rusqlite::functions::FunctionFlags::SQLITE_UTF8
            | rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC,
        move |ctx| {
            // SQLite: X REGEXP Y → regexp(Y, X), so arg 0 is the pattern.
            let pattern: String = ctx.get(0)?;
            let text: String = ctx.get(1)?;
            let mut cache = regex_cache.lock_recover();
            if !cache.contains_key(&pattern) {
                if cache.len() >= 64 {
                    cache.clear();
                }
                let compiled = crate::regexp::compile(&pattern)
                    .map_err(|e| rusqlite::Error::UserFunctionError(Box::new(e)))?;
                cache.insert(pattern.clone(), compiled);
            }
            Ok(cache[&pattern].is_match(&text))
        },
    )?;
    Ok(())
}

/// Opens a file-backed database with all connection-level settings applied.
pub fn open_database(path: &Path) -> Result<Connection> {
    let conn = Connection::open(path)
        .with_context(|| format!("Failed to open SQLite database at {path:?}"))?;
    configure_connection(&conn)?;
    migrate_legacy_table_names(&conn)?;
    ensure_pending_tracker_column(&conn)?;
    ensure_next_version_column(&conn)?;
    ensure_entity_version_after_column(&conn)?;
    ensure_perf_indexes(&conn)?;
    ensure_field_text(&conn)?;
    Ok(conn)
}

/// Adds `metarecord.next_version` (the per-record monotonic version allocator,
/// spec-data-model) to databases created before it existed. Back-fills each
/// existing row to `version + 1`: legacy databases have no rollback gaps
/// encoded, so the current version is the correct allocator high-water mark.
/// Idempotent; a no-op on fresh databases and on ones with no `metarecord` yet.
fn ensure_next_version_column(conn: &Connection) -> Result<()> {
    let has_table: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'metarecord'",
        [],
        |r| r.get(0),
    )?;
    if has_table == 0 {
        return Ok(());
    }
    let has_column: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('metarecord') WHERE name = 'next_version'",
        [],
        |r| r.get(0),
    )?;
    if has_column == 0 {
        conn.execute_batch(
            "ALTER TABLE metarecord ADD COLUMN next_version INTEGER NOT NULL DEFAULT 1;
             UPDATE metarecord SET next_version = version + 1;",
        )
        .context("Failed to add metarecord.next_version column")?;
    }
    Ok(())
}

/// Adds `operation.entity_version_after` to databases created before it existed.
/// Left NULL on existing rows: forward (redo) application falls back to
/// `entity_version_before + 1` for NULL, exactly the pre-migration behaviour.
/// Idempotent; a no-op on fresh databases and on ones with no `operation` yet.
fn ensure_entity_version_after_column(conn: &Connection) -> Result<()> {
    let has_table: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'operation'",
        [],
        |r| r.get(0),
    )?;
    if has_table == 0 {
        return Ok(());
    }
    let has_column: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('operation') WHERE name = 'entity_version_after'",
        [],
        |r| r.get(0),
    )?;
    if has_column == 0 {
        conn.execute("ALTER TABLE operation ADD COLUMN entity_version_after INTEGER", [])
            .context("Failed to add operation.entity_version_after column")?;
    }
    Ok(())
}

/// Adds `pending_operation.tracker` to databases created before it existed, so
/// the executor can correlate split rename From/To events by their inotify
/// cookie. Idempotent; a no-op on fresh databases (`init_schema` already
/// includes the column) and on databases that have no `pending_operation` yet.
fn ensure_pending_tracker_column(conn: &Connection) -> Result<()> {
    let has_table: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'pending_operation'",
        [],
        |r| r.get(0),
    )?;
    if has_table == 0 {
        return Ok(());
    }
    let has_column: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('pending_operation') WHERE name = 'tracker'",
        [],
        |r| r.get(0),
    )?;
    if has_column == 0 {
        conn.execute("ALTER TABLE pending_operation ADD COLUMN tracker INTEGER", [])
            .context("Failed to add pending_operation.tracker column")?;
    }
    Ok(())
}

/// Creates the performance indexes if missing, so repositories created before
/// they were added pick them up on the next load (a no-op on fresh databases,
/// where `init_schema` already created them). Cheap and idempotent.
fn ensure_perf_indexes(conn: &Connection) -> Result<()> {
    // A freshly created file has no tables yet; `init_schema` will run next and
    // create them already carrying the indexes. Only existing databases need
    // this back-fill.
    let tables: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master \
         WHERE type = 'table' AND name IN ('metarecord', 'field')",
        [],
        |r| r.get(0),
    )?;
    if tables < 2 {
        return Ok(());
    }
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_field_name ON field(field_name, metarecord_uuid);
         CREATE INDEX IF NOT EXISTS idx_field_name_type ON field(field_name, value_type);",
    )
    .context("Failed to ensure performance indexes")?;
    // Back-fill the single-mfr_path invariant on existing databases too.
    // Best-effort: a repo that predates it with a duplicate mfr_path keeps
    // opening (the index simply is not created until the duplicate is resolved).
    let _ = conn.execute(
        "CREATE UNIQUE INDEX IF NOT EXISTS idx_mfr_path_single ON field(metarecord_uuid) \
         WHERE field_name = 'mfr_path'",
        [],
    );
    Ok(())
}

/// Back-fills the `field_text` trigram FTS index for databases created before it
/// existed (spec-query "MATCHES via FTS5"). Idempotent; a no-op on fresh
/// databases (where `init_schema` already created it) and on databases that
/// already carry it. When absent, the table is created and bulk-loaded from the
/// existing textual field rows — this same path also serves as a rebuild
/// (drop + call) if the index ever needs compaction.
pub fn ensure_field_text(conn: &Connection) -> Result<()> {
    // Fresh file: no tables yet, `init_schema` runs next and creates it.
    let has_field: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = 'field'",
        [],
        |r| r.get(0),
    )?;
    if has_field == 0 {
        return Ok(());
    }
    let has_fts: i64 =
        conn.query_row("SELECT COUNT(*) FROM sqlite_master WHERE name = 'field_text'", [], |r| {
            r.get(0)
        })?;
    if has_fts != 0 {
        return Ok(());
    }
    conn.execute_batch(
        "CREATE VIRTUAL TABLE field_text USING fts5(
            text, content='', contentless_delete=1, tokenize='trigram'
         );
         INSERT INTO field_text(rowid, text)
            SELECT id, COALESCE(value_text, value_name) FROM field
            WHERE value_type IN ('string', 'tree_ref');",
    )
    .context("Failed to build the field_text FTS index")?;
    Ok(())
}

/// Migrates a database created under an earlier name of the metarecord
/// concept: either the original `metadata`/`metadata_db` tables (with
/// `metadata_uuid` columns and `*_entry` op types) or the short-lived
/// `record`/`record_db` intermediate. Both land on the current schema.
fn migrate_legacy_table_names(conn: &Connection) -> Result<()> {
    let has_table = |name: &str| -> Result<bool> {
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [name],
            |r| r.get(0),
        )?;
        Ok(n != 0)
    };
    for (table, table_db, uuid_col, op_suffix) in [
        ("metadata", "metadata_db", "metadata_uuid", "entry"),
        ("record", "record_db", "record_uuid", "record"),
    ] {
        if !has_table(table)? {
            continue;
        }
        conn.execute_batch(&format!(
            "BEGIN;
             ALTER TABLE {table} RENAME TO metarecord;
             DROP TABLE {table_db};
             ALTER TABLE field RENAME COLUMN {uuid_col} TO metarecord_uuid;
             DROP INDEX IF EXISTS idx_metadata_db;
             DROP INDEX IF EXISTS idx_record_db;
             DROP INDEX IF EXISTS idx_field_entry;
             DROP INDEX IF EXISTS idx_field_record;
             CREATE INDEX IF NOT EXISTS idx_field_metarecord ON field(metarecord_uuid, field_name);
             UPDATE operation SET op_type = 'create_metarecord' WHERE op_type = 'create_{op_suffix}';
             UPDATE operation SET op_type = 'delete_metarecord' WHERE op_type = 'delete_{op_suffix}';
             COMMIT;",
        ))
        .with_context(|| format!("Failed to migrate the legacy {table} schema"))?;
        return Ok(()); // The two legacy states are mutually exclusive.
    }
    Ok(())
}

/// Opens an in-memory database with connection-level settings (for tests).
pub fn open_in_memory() -> Result<Connection> {
    let conn = Connection::open_in_memory()?;
    configure_connection(&conn)?;
    Ok(conn)
}

/// Creates all tables and indexes. Call on a fresh database only.
pub fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        -- ── Data tables (spec-data-model) ───────────────────────────────────
        CREATE TABLE metarecord (
            uuid         BLOB    PRIMARY KEY NOT NULL,  -- 16-byte UUID
            version      INTEGER NOT NULL DEFAULT 0,    -- current version (from next_version)
            next_version INTEGER NOT NULL DEFAULT 1     -- monotonic allocator; never restored
        );

        CREATE TABLE field (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            metarecord_uuid     BLOB    NOT NULL REFERENCES metarecord(uuid) ON DELETE CASCADE,
            field_name      TEXT    NOT NULL,
            value_type      TEXT    NOT NULL,
            value_text      TEXT,    -- string
            value_int       INTEGER, -- int, bool (0/1), datetime (Unix ms)
            value_real      REAL,    -- float
            value_uuid      BLOB,    -- ref/refbase/externalref: metarecord or repo UUID;
                                     -- tree_ref: parent UUID (zero UUID for roots)
            value_ref_repo  BLOB,    -- externalref only: repo UUID
            value_name      TEXT     -- tree_ref: name component
        );
        CREATE INDEX idx_field_metarecord ON field(metarecord_uuid, field_name);
        -- Predicates filter by field_name (IsPresent/Eq/…); seek the field_name
        -- range instead of scanning the whole EAV table. metarecord_uuid second
        -- makes it cover the `DISTINCT metarecord_uuid` projection.
        CREATE INDEX idx_field_name ON field(field_name, metarecord_uuid);
        -- Covering: the value types a field holds are read from the index
        -- itself, instead of one table lookup per row of the field.
        CREATE INDEX idx_field_name_type ON field(field_name, value_type);
        CREATE INDEX idx_field_reverse ON field(field_name, value_uuid, value_ref_repo)
            WHERE value_type IN ('ref', 'externalref');
        CREATE UNIQUE INDEX idx_field_tree ON field(field_name, value_uuid, value_name)
            WHERE value_type = 'tree_ref';
        -- A metarecord tracks at most one filesystem path: `mfr_path` is
        -- single-valued (distinct files get distinct metarecords). Enforced at
        -- the storage layer so every write path is covered at once.
        CREATE UNIQUE INDEX idx_mfr_path_single ON field(metarecord_uuid)
            WHERE field_name = 'mfr_path';

        -- Trigram full-text index over the textual field columns, to pre-filter
        -- MATCHES (regex) before the REGEXP scan (spec-query \"MATCHES via FTS5\").
        -- Contentless (text not stored twice); rowid = field.id. Maintained in
        -- the same transaction as every field write (see insert_field_row /
        -- delete_field_text_*); a superset is always correct because field.id is
        -- AUTOINCREMENT (never reused), so the REGEXP re-filter excludes any
        -- stale rowid.
        CREATE VIRTUAL TABLE field_text USING fts5(
            text, content='', contentless_delete=1, tokenize='trigram'
        );

        -- ── Event log (spec-event-log) ──────────────────────────────────────
        CREATE TABLE revision (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            timestamp  INTEGER NOT NULL,  -- Unix ms
            label      TEXT
        );

        CREATE TABLE operation (
            id                    INTEGER PRIMARY KEY AUTOINCREMENT,
            parent_id             INTEGER REFERENCES operation(id),
            rev_id                INTEGER NOT NULL REFERENCES revision(id) ON DELETE CASCADE,
            seq                   INTEGER NOT NULL,
            op_type               TEXT    NOT NULL,
            entity_uuid           BLOB    NOT NULL,
            entity_version_before INTEGER,
            entity_version_after  INTEGER,
            field_name            TEXT
        );
        CREATE INDEX idx_operation_parent ON operation(parent_id);
        CREATE INDEX idx_operation_rev    ON operation(rev_id, seq);
        CREATE INDEX idx_operation_entity ON operation(entity_uuid, id);

        CREATE TABLE op_snapshot (
            op_id          INTEGER NOT NULL REFERENCES operation(id) ON DELETE CASCADE,
            is_new         INTEGER NOT NULL CHECK (is_new IN (0, 1)),
            field_id       INTEGER NOT NULL,
            field_name     TEXT    NOT NULL,
            value_type     TEXT    NOT NULL,
            value_text     TEXT,
            value_int      INTEGER,
            value_real     REAL,
            value_uuid     BLOB,
            value_ref_repo BLOB,
            value_name     TEXT,
            PRIMARY KEY (op_id, is_new, field_id)
        );

        CREATE TABLE log_head (
            singleton  INTEGER PRIMARY KEY CHECK (singleton = 1),
            op_id      INTEGER REFERENCES operation(id)
        );
        INSERT INTO log_head (singleton, op_id) VALUES (1, NULL);

        CREATE TABLE pending_operation (
            id             INTEGER PRIMARY KEY AUTOINCREMENT,
            op_type        TEXT NOT NULL,
            entity_uuid    BLOB,
            path           TEXT,
            from_path      TEXT,
            to_path        TEXT,
            field_name     TEXT,
            value_type     TEXT,
            value_text     TEXT,
            value_int      INTEGER,
            value_real     REAL,
            value_uuid     BLOB,
            value_ref_repo BLOB,
            value_name     TEXT,
            tracker        INTEGER
        );
        ",
    )
    .context("Failed to initialize the database schema")
}

// ── Read helpers ──────────────────────────────────────────────────────────────

/// Retrieves a metarecord with all its fields, or None if it does not exist.
pub fn get_metarecord(conn: &Connection, uuid: Uuid) -> Result<Option<MetaRecord>> {
    let Some(version) = get_version(conn, uuid)? else {
        return Ok(None);
    };
    let fields = get_field_rows(conn, uuid)?
        .into_iter()
        .map(|r| Field { id: Some(r.id), name: r.name, value: r.value })
        .collect();

    Ok(Some(MetaRecord { uuid, version, fields }))
}

/// Returns the version counter of a metarecord, or None if it does not exist.
pub fn get_version(conn: &Connection, uuid: Uuid) -> Result<Option<u64>> {
    let v: Option<i64> = conn
        .prepare_cached("SELECT version FROM metarecord WHERE uuid = ?1")?
        .query_row(params![uuid_to_bytes(uuid)], |r| r.get(0))
        .optional()?;
    Ok(v.map(|v| v as u64))
}

const FIELD_COLUMNS: &str =
    "id, field_name, value_type, value_text, value_int, value_real, value_uuid, value_ref_repo, value_name";

fn row_to_field_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<(i64, String, Result<Value>)> {
    let id: i64 = row.get(0)?;
    let name: String = row.get(1)?;
    let value = decode_value(
        &row.get::<_, String>(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
    );
    Ok((id, name, value))
}

/// All field rows of a metarecord, with their row ids.
pub fn get_field_rows(conn: &Connection, uuid: Uuid) -> Result<Vec<FieldRow>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {FIELD_COLUMNS} FROM field WHERE metarecord_uuid = ?1 ORDER BY id"
    ))?;
    let rows = stmt.query_map(params![uuid_to_bytes(uuid)], row_to_field_row)?;
    collect_field_rows(rows)
}

/// Max metarecords per `IN (…)` batch — well under SQLite's variable limit, so
/// the readers below never build an oversized statement.
const IN_CHUNK: usize = 500;

/// Field rows of several metarecords in a handful of `WHERE metarecord_uuid IN
/// (…)` scans (chunked), grouped by owner with each metarecord's rows in id
/// order — the batched form of [`get_field_rows`], so assembling a query page no
/// longer costs one query per metarecord. A uuid with no rows (or unknown) is
/// simply absent from the map.
pub fn field_rows_for(
    conn: &Connection,
    uuids: &[Uuid],
) -> Result<std::collections::HashMap<Uuid, Vec<FieldRow>>> {
    let mut out: std::collections::HashMap<Uuid, Vec<FieldRow>> = std::collections::HashMap::new();
    for chunk in uuids.chunks(IN_CHUNK) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT {FIELD_COLUMNS}, metarecord_uuid FROM field \
             WHERE metarecord_uuid IN ({placeholders}) ORDER BY id"
        ))?;
        let params: Vec<Vec<u8>> = chunk.iter().map(|u| uuid_to_bytes(*u)).collect();
        let mut rows = stmt.query(rusqlite::params_from_iter(params.iter()))?;
        while let Some(row) = rows.next()? {
            let (id, name, value) = row_to_field_row(row)?;
            let uuid = bytes_to_uuid(row.get::<_, Vec<u8>>(9)?)?;
            out.entry(uuid).or_default().push(FieldRow { id, name, value: value? });
        }
    }
    Ok(out)
}

/// Versions of several metarecords in chunked `IN (…)` queries — the batched
/// form of [`get_version`]. An unknown uuid is absent from the map.
pub fn versions_for(
    conn: &Connection,
    uuids: &[Uuid],
) -> Result<std::collections::HashMap<Uuid, u64>> {
    let mut out = std::collections::HashMap::new();
    for chunk in uuids.chunks(IN_CHUNK) {
        let placeholders = vec!["?"; chunk.len()].join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT uuid, version FROM metarecord WHERE uuid IN ({placeholders})"
        ))?;
        let params: Vec<Vec<u8>> = chunk.iter().map(|u| uuid_to_bytes(*u)).collect();
        let mut rows = stmt.query(rusqlite::params_from_iter(params.iter()))?;
        while let Some(row) = rows.next()? {
            let uuid = bytes_to_uuid(row.get::<_, Vec<u8>>(0)?)?;
            out.insert(uuid, row.get::<_, i64>(1)? as u64);
        }
    }
    Ok(out)
}

/// Streams every field row of the whole repository — all metarecords — in a
/// single sequential table scan, invoking `f(owner_uuid, row)` per row. This
/// replaces the per-metarecord `get_field_rows` walk in the bulk index build
/// (`RepoIndex::build`): one scan instead of one query per metarecord, which on
/// a large repository turns ~N seeks into a single pass. Row order is
/// unspecified (the caller routes each row by its owner uuid), and the owner
/// uuid is selected *after* the shared `FIELD_COLUMNS` so `row_to_field_row`
/// keeps its column indices.
pub fn for_each_field_row(
    conn: &Connection,
    mut f: impl FnMut(Uuid, FieldRow) -> Result<()>,
) -> Result<()> {
    let mut stmt = conn.prepare(&format!("SELECT {FIELD_COLUMNS}, metarecord_uuid FROM field"))?;
    let mut rows = stmt.query([])?;
    while let Some(row) = rows.next()? {
        let (id, name, value) = row_to_field_row(row)?;
        let uuid = bytes_to_uuid(row.get::<_, Vec<u8>>(9)?)?;
        f(uuid, FieldRow { id, name, value: value? })?;
    }
    Ok(())
}

/// A single field row by its (repository-unique) id, or `None` if absent.
pub fn get_field_row_by_id(conn: &Connection, id: i64) -> Result<Option<FieldRow>> {
    let mut stmt =
        conn.prepare_cached(&format!("SELECT {FIELD_COLUMNS} FROM field WHERE id = ?1"))?;
    let rows = stmt.query_map(params![id], row_to_field_row)?;
    Ok(collect_field_rows(rows)?.into_iter().next())
}

/// The metarecord that owns a field row id, or `None` if the id is unknown.
pub fn metarecord_of_field(conn: &Connection, id: i64) -> Result<Option<Uuid>> {
    conn.prepare_cached("SELECT metarecord_uuid FROM field WHERE id = ?1")?
        .query_row(params![id], |r| r.get::<_, Vec<u8>>(0))
        .optional()?
        .map(bytes_to_uuid)
        .transpose()
}

/// Field rows of a metarecord restricted to one field name.
pub fn get_field_rows_named(conn: &Connection, uuid: Uuid, name: &str) -> Result<Vec<FieldRow>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {FIELD_COLUMNS} FROM field
         WHERE metarecord_uuid = ?1 AND field_name = ?2 ORDER BY id"
    ))?;
    let rows = stmt.query_map(params![uuid_to_bytes(uuid), name], row_to_field_row)?;
    collect_field_rows(rows)
}

/// The first `String` value of the `(uuid, name)` field, or `None`. The single
/// typed reader shared by reconcile / executor / eligibility (they used to
/// re-derive it over `get_field_rows_named` each).
pub fn string_field(conn: &Connection, uuid: Uuid, name: &str) -> Result<Option<String>> {
    Ok(get_field_rows_named(conn, uuid, name)?.into_iter().find_map(|r| match r.value {
        Value::String(s) => Some(s),
        _ => None,
    }))
}

/// The first `Int` value of the `(uuid, name)` field, or `None`.
pub fn int_field(conn: &Connection, uuid: Uuid, name: &str) -> Result<Option<i64>> {
    Ok(get_field_rows_named(conn, uuid, name)?.into_iter().find_map(|r| match r.value {
        Value::Int(n) => Some(n),
        _ => None,
    }))
}

/// Every `String` value of the `(uuid, name)` multi-map field, in row order.
pub fn string_fields(conn: &Connection, uuid: Uuid, name: &str) -> Result<Vec<String>> {
    Ok(get_field_rows_named(conn, uuid, name)?
        .into_iter()
        .filter_map(|r| match r.value {
            Value::String(s) => Some(s),
            _ => None,
        })
        .collect())
}

/// The first `Bool` value of the `(uuid, name)` field, or `None` (`Nothing`
/// rows do not count).
pub fn bool_field(conn: &Connection, uuid: Uuid, name: &str) -> Result<Option<bool>> {
    Ok(get_field_rows_named(conn, uuid, name)?.into_iter().find_map(|r| match r.value {
        Value::Bool(b) => Some(b),
        _ => None,
    }))
}

/// The established non-`Nothing` value type of a field name, file-wide, or
/// `None` when the name has no non-`Nothing` rows (type not yet established).
/// The "one value type per field name" invariant guarantees at most one such
/// type, so any one non-`Nothing` row is representative — `LIMIT 1` over the
/// existing `idx_field_name` (it seeks the `field_name` range and stops at the
/// first non-`Nothing` row, almost always the first), cheap enough to run per
/// write and amortised further by the [`crate::log::Writer`] per-revision cache.
pub fn established_value_type(conn: &Connection, name: &str) -> Result<Option<String>> {
    Ok(conn
        .prepare_cached(
            "SELECT value_type FROM field \
             WHERE field_name = ?1 AND value_type != 'nothing' LIMIT 1",
        )?
        .query_row(params![name], |r| r.get::<_, String>(0))
        .optional()?)
}

/// The current log HEAD operation id (`log_head.op_id`), or `None` before any
/// operation. Used as the freshness marker for the in-memory query index.
pub fn current_head(conn: &Connection) -> Result<Option<i64>> {
    Ok(conn
        .query_row("SELECT op_id FROM log_head WHERE singleton = 1", [], |r| {
            r.get::<_, Option<i64>>(0)
        })
        .optional()?
        .flatten())
}

/// Distinct metarecord UUIDs carrying at least one row of `name` (served by
/// `idx_field_name`). Used by the retype operation to walk a field's holders.
pub fn metarecords_with_field(conn: &Connection, name: &str) -> Result<Vec<Uuid>> {
    let mut stmt =
        conn.prepare_cached("SELECT DISTINCT metarecord_uuid FROM field WHERE field_name = ?1")?;
    let uuids = stmt
        .query_map(params![name], |r| r.get::<_, Vec<u8>>(0))?
        .map(|r| r.map_err(Into::into).and_then(bytes_to_uuid))
        .collect::<Result<Vec<Uuid>>>()?;
    Ok(uuids)
}

/// Collects a query yielding one UUID-blob column into `Uuid`s.
fn collect_uuid_col<'a>(
    rows: impl Iterator<Item = rusqlite::Result<Vec<u8>>> + 'a,
) -> Result<Vec<Uuid>> {
    rows.map(|r| r.map_err(Into::into).and_then(bytes_to_uuid)).collect()
}

// ── Schema-check candidate queries ──────────────────────────────────────────
//
// A whole-repository schema check only needs the metarecords that *could*
// violate a constraint (schema.rs `violation_candidates`); these index-served
// set queries find them without a per-record scan. Each takes a `limit` (rows
// to return — `i64::MAX` for "all") so the capped heads-up stays bounded.

/// Metarecords with ≥1 non-`Nothing` row of `field` whose value type is neither
/// `allowed` — the exact holders a `type` constraint on `field` can reject.
pub fn uuids_field_wrong_type(
    conn: &Connection,
    field: &str,
    allowed: &str,
    limit: i64,
) -> Result<Vec<Uuid>> {
    let mut stmt = conn.prepare_cached(
        "SELECT DISTINCT metarecord_uuid FROM field \
         WHERE field_name = ?1 AND value_type != 'nothing' AND value_type != ?2 LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![field, allowed, limit], |r| r.get::<_, Vec<u8>>(0))?;
    collect_uuid_col(rows)
}

/// Metarecords whose row-count for `field` exceeds `max` (max-cardinality
/// candidates). Counts every row, `Nothing` included, matching validation.
pub fn uuids_field_count_over(
    conn: &Connection,
    field: &str,
    max: i64,
    limit: i64,
) -> Result<Vec<Uuid>> {
    let mut stmt = conn.prepare_cached(
        "SELECT metarecord_uuid FROM field WHERE field_name = ?1 \
         GROUP BY metarecord_uuid HAVING COUNT(*) > ?2 LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![field, max, limit], |r| r.get::<_, Vec<u8>>(0))?;
    collect_uuid_col(rows)
}

/// Metarecords that hold `field` but with fewer than `min` rows (the
/// present-but-under-minimum candidates; the absent ones are found separately).
pub fn uuids_field_count_under(
    conn: &Connection,
    field: &str,
    min: i64,
    limit: i64,
) -> Result<Vec<Uuid>> {
    let mut stmt = conn.prepare_cached(
        "SELECT metarecord_uuid FROM field WHERE field_name = ?1 \
         GROUP BY metarecord_uuid HAVING COUNT(*) < ?2 LIMIT ?3",
    )?;
    let rows = stmt.query_map(params![field, min, limit], |r| r.get::<_, Vec<u8>>(0))?;
    collect_uuid_col(rows)
}

/// Metarecords with no `field` row at all (min-cardinality candidates for a
/// global constraint, where absence violates a `min ≥ 1`).
pub fn uuids_missing_field(conn: &Connection, field: &str, limit: i64) -> Result<Vec<Uuid>> {
    let mut stmt = conn.prepare_cached(
        "SELECT uuid FROM metarecord \
         WHERE uuid NOT IN (SELECT metarecord_uuid FROM field WHERE field_name = ?1) LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![field, limit], |r| r.get::<_, Vec<u8>>(0))?;
    collect_uuid_col(rows)
}

/// Metarecords declared one of `types` (via `mf_schema`) that hold no `field`
/// row — min-cardinality candidates for a *targeted* constraint, restricted to
/// the target population so unrelated records are never candidates.
pub fn uuids_typed_missing_field(
    conn: &Connection,
    types: &[String],
    field: &str,
    limit: i64,
) -> Result<Vec<Uuid>> {
    if types.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders = (1..=types.len()).map(|i| format!("?{i}")).collect::<Vec<_>>().join(",");
    let field_idx = types.len() + 1;
    let limit_idx = types.len() + 2;
    let sql = format!(
        "SELECT DISTINCT metarecord_uuid FROM field \
         WHERE field_name = 'mf_schema' AND value_type = 'string' AND value_text IN ({placeholders}) \
           AND metarecord_uuid NOT IN (SELECT metarecord_uuid FROM field WHERE field_name = ?{field_idx}) \
         LIMIT ?{limit_idx}"
    );
    let mut params: Vec<rusqlite::types::Value> =
        types.iter().map(|t| rusqlite::types::Value::Text(t.clone())).collect();
    params.push(rusqlite::types::Value::Text(field.to_string()));
    params.push(rusqlite::types::Value::Integer(limit));
    let mut stmt = conn.prepare(&sql)?;
    let rows =
        stmt.query_map(rusqlite::params_from_iter(params.iter()), |r| r.get::<_, Vec<u8>>(0))?;
    collect_uuid_col(rows)
}

/// Total number of metarecords in the repository (one cheap `COUNT`).
pub fn count_metarecords(conn: &Connection) -> Result<usize> {
    Ok(conn.query_row("SELECT COUNT(*) FROM metarecord", [], |r| r.get::<_, i64>(0))? as usize)
}

/// The largest `field.id` (0 when empty). `id` is the AUTOINCREMENT rowid, so
/// this is an O(1) rightmost-btree read — a cheap upper bound for a determinate
/// progress bar over a sequential (rowid-ordered) scan of the `field` table.
pub fn max_field_id(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT COALESCE(MAX(id), 0) FROM field", [], |r| r.get(0))?)
}

fn collect_field_rows<'a>(
    rows: impl Iterator<Item = rusqlite::Result<(i64, String, Result<Value>)>> + 'a,
) -> Result<Vec<FieldRow>> {
    rows.map(|r| {
        let (id, name, value) = r?;
        Ok(FieldRow { id, name, value: value? })
    })
    .collect()
}

/// All metarecord UUIDs of this repository, sorted by UUID byte order.
pub fn list_entries(conn: &Connection) -> Result<Vec<Uuid>> {
    let mut stmt = conn.prepare("SELECT uuid FROM metarecord ORDER BY uuid")?;
    let uuids = stmt
        .query_map([], |r| r.get::<_, Vec<u8>>(0))?
        .map(|r| r.map_err(Into::into).and_then(bytes_to_uuid))
        .collect::<Result<Vec<Uuid>>>()?;
    Ok(uuids)
}

/// All metarecords of this repository holding an `mfr_path` TreeRef (i.e. with
/// a known tree position, stale or not).
/// Every metarecord carrying a `String` value for `field_name`, with that
/// value — the multi-map's first row per metarecord wins. Used for the handful
/// of daemon-read marker fields (`mfr_mount`), never for user queries.
pub fn string_field_owners(conn: &Connection, field_name: &str) -> Result<Vec<(Uuid, String)>> {
    let mut stmt = conn.prepare(
        "SELECT metarecord_uuid, value_text FROM field
         WHERE field_name = ?1 AND value_type = 'string' AND value_text IS NOT NULL
         ORDER BY id",
    )?;
    let rows = stmt
        .query_map([field_name], |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, String>(1)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for (bytes, value) in rows {
        let uuid = bytes_to_uuid(bytes)?;
        if seen.insert(uuid) {
            out.push((uuid, value));
        }
    }
    Ok(out)
}

pub fn all_tracked_metarecords(conn: &Connection) -> Result<Vec<Uuid>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT metarecord_uuid FROM field
         WHERE field_name = 'mfr_path' AND value_type = 'tree_ref'",
    )?;
    let uuids = stmt
        .query_map([], |r| r.get::<_, Vec<u8>>(0))?
        .map(|r| r.map_err(Into::into).and_then(bytes_to_uuid))
        .collect::<Result<Vec<Uuid>>>()?;
    Ok(uuids)
}

/// Orphaned metarecords of this repository (`mfr_path` = Nothing) whose stored
/// `mfr_size` matches. First step of the fingerprint cascade.
pub fn find_orphans_by_size(conn: &Connection, size: i64) -> Result<Vec<Uuid>> {
    let mut stmt = conn.prepare(
        "SELECT p.metarecord_uuid FROM field p
         JOIN field s ON s.metarecord_uuid = p.metarecord_uuid
              AND s.field_name = 'mfr_size' AND s.value_type = 'int' AND s.value_int = ?1
         WHERE p.field_name = 'mfr_path' AND p.value_type = 'nothing'",
    )?;
    let uuids = stmt
        .query_map(params![size], |r| r.get::<_, Vec<u8>>(0))?
        .map(|r| r.map_err(Into::into).and_then(bytes_to_uuid))
        .collect::<Result<Vec<Uuid>>>()?;
    Ok(uuids)
}

/// One page of [`list_entries`]: metarecords after `after` (exclusive), at most
/// `limit` rows, sorted by UUID byte order (keyset pagination).
pub fn list_entries_page(
    conn: &Connection,
    after: Option<Uuid>,
    limit: usize,
) -> Result<Vec<Uuid>> {
    // Conditional keyset: the cursor predicate is omitted on the first page so
    // the primary-key index seeks directly. Folding it into a single
    // `(?2 IS NULL OR uuid > ?2)` would defeat the seek (the OR forces a full
    // scan on every page).
    let after_clause = if after.is_some() { "WHERE uuid > ?2" } else { "" };
    let sql = format!("SELECT uuid FROM metarecord {after_clause} ORDER BY uuid LIMIT ?1");
    let mut stmt = conn.prepare(&sql)?;
    let mut sql_params: Vec<rusqlite::types::Value> = vec![(limit as i64).into()];
    if let Some(after) = after {
        sql_params.push(uuid_to_bytes(after).into());
    }
    let uuids = stmt
        .query_map(rusqlite::params_from_iter(sql_params.iter()), |r| r.get::<_, Vec<u8>>(0))?
        .map(|r| r.map_err(Into::into).and_then(bytes_to_uuid))
        .collect::<Result<Vec<Uuid>>>()?;
    Ok(uuids)
}

/// Resolves one tree step: the child of `parent` named `name` in the tree of
/// `field_name`. `parent = None` looks up root nodes.
pub fn find_tree_child(
    conn: &Connection,
    field_name: &str,
    parent: Option<Uuid>,
    name: &str,
) -> Result<Option<Uuid>> {
    find_tree_child_opts(conn, field_name, parent, name, false)
}

/// Like [`find_tree_child`], optionally matching `name` case-insensitively
/// (SQLite NOCASE — ASCII only; spec-platform leaves Unicode folding open).
pub fn find_tree_child_opts(
    conn: &Connection,
    field_name: &str,
    parent: Option<Uuid>,
    name: &str,
    case_insensitive: bool,
) -> Result<Option<Uuid>> {
    let collate = if case_insensitive { " COLLATE NOCASE" } else { "" };
    let parent_blob = uuid_to_bytes(parent.unwrap_or(ZERO_UUID));
    let uuid: Option<Vec<u8>> = conn
        .query_row(
            &format!(
                "SELECT metarecord_uuid FROM field
                 WHERE field_name = ?1 AND value_type = 'tree_ref'
                   AND value_uuid = ?2 AND value_name = ?3{collate}"
            ),
            params![field_name, parent_blob, name],
            |r| r.get(0),
        )
        .optional()?;
    uuid.map(bytes_to_uuid).transpose()
}

/// All direct children of `parent` in the tree of `field_name`, with the
/// name component each child contributes.
pub fn tree_children(
    conn: &Connection,
    field_name: &str,
    parent: Uuid,
) -> Result<Vec<(Uuid, String)>> {
    // `prepare_cached`: this is the per-node hot path of the (fallback) tree
    // walk, so the statement must not be re-compiled for every node.
    let mut stmt = conn.prepare_cached(
        "SELECT metarecord_uuid, value_name FROM field
         WHERE field_name = ?1 AND value_type = 'tree_ref' AND value_uuid = ?2",
    )?;
    let children = stmt
        .query_map(params![field_name, uuid_to_bytes(parent)], |r| {
            Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, String>(1)?))
        })?
        .map(|r| {
            let (uuid, name) = r?;
            Ok((bytes_to_uuid(uuid)?, name))
        })
        .collect::<Result<Vec<(Uuid, String)>>>()?;
    Ok(children)
}

/// The distinct `(field_name, value_type)` pairs present in this repository,
/// optionally restricted to a single value type (e.g. `"tree_ref"`, `"ref"`).
/// `Nothing` rows are excluded — they record an explicit absence, not a usable
/// field value. A field name has a single non-`Nothing` value type
/// repository-wide, so it appears at most once. Ordered by name then type for a
/// stable response.
///
/// `GET /repos/:repo/fields` is served from the in-memory index
/// (`RepoIndex::field_catalog`) instead — this SQL form is the equivalence
/// oracle (`tests/index_oracle.rs`) and the scan-based fallback definition.
pub fn distinct_field_names(
    conn: &Connection,
    type_filter: Option<&str>,
) -> Result<Vec<(String, String)>> {
    let type_clause = if type_filter.is_some() { "AND value_type = ?1" } else { "" };
    let sql = format!(
        "SELECT DISTINCT field_name, value_type FROM field
         WHERE value_type != 'nothing' {type_clause}
         ORDER BY field_name, value_type"
    );
    let mut stmt = conn.prepare(&sql)?;
    let map = |r: &rusqlite::Row| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?));
    let rows = match type_filter {
        Some(t) => stmt.query_map(params![t], map)?.collect::<rusqlite::Result<Vec<_>>>(),
        None => stmt.query_map([], map)?.collect::<rusqlite::Result<Vec<_>>>(),
    }?;
    Ok(rows)
}

/// One TreeRef position: the metarecord, the field name whose forest it belongs
/// to, its parent (`None` = a forest root) and the name component it contributes.
pub struct TreeRow {
    pub field_name: String,
    pub uuid: Uuid,
    pub parent: Option<Uuid>,
    pub name: String,
}

/// Every TreeRef position in the database, across all field names, ordered so
/// that a metarecord's positions are grouped and stable (`metarecord_uuid`,
/// then `id`). Used to populate the tree cache in a single scan at load time
/// instead of walking the forest node by node.
pub fn load_tree_forest(conn: &Connection) -> Result<Vec<TreeRow>> {
    let mut stmt = conn.prepare(
        "SELECT field_name, metarecord_uuid, value_uuid, value_name FROM field
         WHERE value_type = 'tree_ref'
         ORDER BY field_name, metarecord_uuid, id",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Vec<u8>>(1)?,
            r.get::<_, Vec<u8>>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (field_name, uuid, parent, name) = row?;
        let parent = bytes_to_uuid(parent)?;
        out.push(TreeRow {
            field_name,
            uuid: bytes_to_uuid(uuid)?,
            parent: if parent == ZERO_UUID { None } else { Some(parent) },
            name,
        });
    }
    Ok(out)
}

/// The first tree position `(parent, name)` of a metarecord for `field_name`,
/// or None when the metarecord has no such TreeRef field.
pub fn tree_position(
    conn: &Connection,
    field_name: &str,
    uuid: Uuid,
) -> Result<Option<(Option<Uuid>, String)>> {
    let row: Option<(Vec<u8>, String)> = conn
        .query_row(
            "SELECT value_uuid, value_name FROM field
             WHERE metarecord_uuid = ?1 AND field_name = ?2 AND value_type = 'tree_ref'
             ORDER BY id LIMIT 1",
            params![uuid_to_bytes(uuid), field_name],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    row.map(|(parent, name)| {
        let parent = bytes_to_uuid(parent)?;
        Ok((if parent == ZERO_UUID { None } else { Some(parent) }, name))
    })
    .transpose()
}

/// All tree positions `(parent, name)` of a metarecord for `field_name`, in id
/// order. Fields are a multi-map, so a metarecord may sit at several positions.
pub fn tree_positions(
    conn: &Connection,
    field_name: &str,
    uuid: Uuid,
) -> Result<Vec<(Option<Uuid>, String)>> {
    let mut stmt = conn.prepare(
        "SELECT value_uuid, value_name FROM field
         WHERE metarecord_uuid = ?1 AND field_name = ?2 AND value_type = 'tree_ref'
         ORDER BY id",
    )?;
    let rows = stmt.query_map(params![uuid_to_bytes(uuid), field_name], |r| {
        Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, String>(1)?))
    })?;
    let mut positions = Vec::new();
    for row in rows {
        let (parent, name) = row?;
        let parent = bytes_to_uuid(parent)?;
        positions.push((if parent == ZERO_UUID { None } else { Some(parent) }, name));
    }
    Ok(positions)
}

/// The TreeRef parents of a metarecord for `field_name` (multi-map: one metarecord can
/// have several positions). `None` in the result means "root".
pub fn get_tree_parents(
    conn: &Connection,
    field_name: &str,
    uuid: Uuid,
) -> Result<Vec<Option<Uuid>>> {
    let mut stmt = conn.prepare(
        "SELECT value_uuid FROM field
         WHERE metarecord_uuid = ?1 AND field_name = ?2 AND value_type = 'tree_ref'",
    )?;
    let parents = stmt
        .query_map(params![uuid_to_bytes(uuid), field_name], |r| r.get::<_, Vec<u8>>(0))?
        .map(|r| {
            let parent = bytes_to_uuid(r?)?;
            Ok(if parent == ZERO_UUID { None } else { Some(parent) })
        })
        .collect::<Result<Vec<Option<Uuid>>>>()?;
    Ok(parents)
}

// ── Internal row encoding (shared with the log module) ───────────────────────

/// Column values for one `field` (or `op_snapshot`) row.
pub(crate) struct EncodedValue {
    pub value_type: &'static str,
    pub text: Option<String>,
    pub int: Option<i64>,
    pub real: Option<f64>,
    pub uuid: Option<Vec<u8>>,
    pub ref_repo: Option<Vec<u8>>,
    pub name: Option<String>,
}

impl EncodedValue {
    fn new(value_type: &'static str) -> Self {
        Self {
            value_type,
            text: None,
            int: None,
            real: None,
            uuid: None,
            ref_repo: None,
            name: None,
        }
    }
}

pub(crate) fn encode_value(value: &Value) -> EncodedValue {
    let mut e;
    match value {
        Value::Nothing => e = EncodedValue::new("nothing"),
        Value::String(s) => {
            e = EncodedValue::new("string");
            e.text = Some(s.clone());
        }
        Value::Int(n) => {
            e = EncodedValue::new("int");
            e.int = Some(*n);
        }
        Value::Float(f) => {
            e = EncodedValue::new("float");
            e.real = Some(*f);
        }
        Value::Bool(b) => {
            e = EncodedValue::new("bool");
            e.int = Some(*b as i64);
        }
        Value::DateTime(ms) => {
            e = EncodedValue::new("datetime");
            e.int = Some(*ms);
        }
        Value::Ref(id) => {
            e = EncodedValue::new("ref");
            e.uuid = Some(uuid_to_bytes(*id));
        }
        Value::TreeRef { parent, name } => {
            e = EncodedValue::new("tree_ref");
            e.uuid = Some(uuid_to_bytes(parent.unwrap_or(ZERO_UUID)));
            e.name = Some(name.display().into_owned());
        }
        Value::RefBase(id) => {
            e = EncodedValue::new("refbase");
            e.uuid = Some(uuid_to_bytes(*id));
        }
        Value::ExternalRef { repo, metarecord } => {
            e = EncodedValue::new("externalref");
            e.uuid = Some(uuid_to_bytes(*metarecord));
            e.ref_repo = Some(uuid_to_bytes(*repo));
        }
    }
    e
}

pub(crate) fn decode_value(
    value_type: &str,
    text: Option<String>,
    int: Option<i64>,
    real: Option<f64>,
    uuid: Option<Vec<u8>>,
    ref_repo: Option<Vec<u8>>,
    name: Option<String>,
) -> Result<Value> {
    match value_type {
        "nothing" => Ok(Value::Nothing),
        "string" => Ok(Value::String(text.context("value_text missing")?)),
        "int" => Ok(Value::Int(int.context("value_int missing")?)),
        "float" => Ok(Value::Float(real.context("value_real missing")?)),
        "bool" => Ok(Value::Bool(int.context("value_int missing")? != 0)),
        "datetime" => Ok(Value::DateTime(int.context("value_int missing")?)),
        "ref" => Ok(Value::Ref(bytes_to_uuid(uuid.context("value_uuid missing")?)?)),
        "tree_ref" => {
            let parent = bytes_to_uuid(uuid.context("value_uuid missing")?)?;
            Ok(Value::TreeRef {
                parent: if parent == ZERO_UUID { None } else { Some(parent) },
                name: TreeName::from(name.context("value_name missing")?),
            })
        }
        "refbase" => Ok(Value::RefBase(bytes_to_uuid(uuid.context("value_uuid missing")?)?)),
        "externalref" => Ok(Value::ExternalRef {
            repo: bytes_to_uuid(ref_repo.context("value_ref_repo missing")?)?,
            metarecord: bytes_to_uuid(uuid.context("value_uuid missing")?)?,
        }),
        other => bail!("Unknown value type: '{other}'"),
    }
}

/// Inserts one row in `field`. `explicit_id` restores a row with its original
/// primary key (used by log navigation); None lets AUTOINCREMENT assign one.
pub(crate) fn insert_field_row(
    conn: &Connection,
    metarecord_uuid: Uuid,
    name: &str,
    value: &Value,
    explicit_id: Option<i64>,
) -> Result<i64> {
    let map_unique = |err: rusqlite::Error| -> anyhow::Error {
        // SQLite names the *columns* (not the index) in a UNIQUE-constraint
        // error, so key off the distinguishing column: `value_name` is the tree
        // index (`idx_field_tree`), `metarecord_uuid` the single-mfr_path index.
        let message = err.to_string();
        if !message.contains("UNIQUE constraint failed") {
            return err.into();
        }
        if message.contains("value_name") {
            DomainError::BadRequest(format!("tree position already occupied for field '{name}'"))
                .into()
        } else if message.contains("metarecord_uuid") {
            DomainError::BadRequest(
                "mfr_path is single-valued: a metarecord tracks at most one path".into(),
            )
            .into()
        } else {
            err.into()
        }
    };
    let e = encode_value(value);
    match explicit_id {
        None => {
            conn.prepare_cached(
                "INSERT INTO field (metarecord_uuid, field_name, value_type, value_text,
                                    value_int, value_real, value_uuid, value_ref_repo, value_name)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            )?
            .execute(params![
                uuid_to_bytes(metarecord_uuid),
                name,
                e.value_type,
                e.text,
                e.int,
                e.real,
                e.uuid,
                e.ref_repo,
                e.name
            ])
            .map_err(map_unique)?;
        }
        Some(id) => {
            conn.prepare_cached(
                "INSERT INTO field (id, metarecord_uuid, field_name, value_type, value_text,
                                    value_int, value_real, value_uuid, value_ref_repo, value_name)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            )?
            .execute(params![
                id,
                uuid_to_bytes(metarecord_uuid),
                name,
                e.value_type,
                e.text,
                e.int,
                e.real,
                e.uuid,
                e.ref_repo,
                e.name
            ])
            .map_err(map_unique)?;
        }
    }
    let id = conn.last_insert_rowid();
    // Maintain the trigram FTS pre-filter over the columns MATCHES scans
    // (string `value_text`, tree_ref `value_name`). Same transaction as the
    // field write; see `field_text` in `init_schema`. The write is an
    // *upsert* (delete-by-rowid, then insert): log navigation restores rows
    // with their original id, which may still carry a stale `field_text` entry
    // — replacing it keeps the rowid unique and the insert idempotent.
    if let Some(text) = fts_indexable_text(&e) {
        conn.prepare_cached("DELETE FROM field_text WHERE rowid = ?1")?.execute(params![id])?;
        conn.prepare_cached("INSERT INTO field_text(rowid, text) VALUES (?1, ?2)")?
            .execute(params![id, text])?;
    }
    Ok(id)
}

/// The text MATCHES indexes for a field value: the string itself, or a
/// tree_ref's name component. Other types are not searched by MATCHES, so they
/// are not indexed.
fn fts_indexable_text(e: &EncodedValue) -> Option<&str> {
    match e.value_type {
        "string" => e.text.as_deref(),
        "tree_ref" => e.name.as_deref(),
        _ => None,
    }
}

/// Removes the `field_text` entry for one field row (by its id). Called just
/// before deleting the row from `field`, so the FTS index stays in sync.
pub(crate) fn delete_field_text_by_id(conn: &Connection, id: i64) -> Result<()> {
    conn.prepare_cached("DELETE FROM field_text WHERE rowid = ?1")?.execute(params![id])?;
    Ok(())
}

/// Removes the `field_text` entries for every row of `(metarecord_uuid, name)`.
/// Resolves the ids through `field` itself, so it must run *before* the rows are
/// deleted from `field`.
pub(crate) fn delete_field_text_by_name(
    conn: &Connection,
    metarecord_uuid: Uuid,
    name: &str,
) -> Result<()> {
    conn.prepare_cached(
        "DELETE FROM field_text WHERE rowid IN \
         (SELECT id FROM field WHERE metarecord_uuid = ?1 AND field_name = ?2)",
    )?
    .execute(params![uuid_to_bytes(metarecord_uuid), name])?;
    Ok(())
}

/// Removes the `field_text` entries for every field row of a metarecord. Run
/// *before* deleting the metarecord (whose `field` rows cascade away).
pub(crate) fn delete_field_text_by_metarecord(
    conn: &Connection,
    metarecord_uuid: Uuid,
) -> Result<()> {
    conn.prepare_cached(
        "DELETE FROM field_text WHERE rowid IN \
         (SELECT id FROM field WHERE metarecord_uuid = ?1)",
    )?
    .execute(params![uuid_to_bytes(metarecord_uuid)])?;
    Ok(())
}
