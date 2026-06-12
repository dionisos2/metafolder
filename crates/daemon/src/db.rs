//! Low-level SQLite operations: connection setup, schema, row encoding and
//! unlogged read helpers. All writes must go through [`crate::log::Writer`]
//! so that the event log stays consistent with the data tables.

use std::path::Path;

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use uuid::Uuid;

use metafolder_core::metarecord::{Field, MetaRecord, Value, ZERO_UUID};

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
    let arr: [u8; 16] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("Invalid UUID blob: expected 16 bytes"))?;
    Ok(Uuid::from_bytes(arr))
}

// ── Connection setup ──────────────────────────────────────────────────────────

fn configure_connection(conn: &Connection) -> Result<()> {
    // An exclusive lock for the whole connection lifetime prevents a second
    // daemon instance from loading the same repository (spec-main invariant).
    conn.pragma_update(None, "locking_mode", "EXCLUSIVE")
        .context("Failed to set locking_mode")?;
    // WAL requires shared-memory files, which network filesystems do not
    // support; fall back to DELETE journal mode there (spec-platform).
    let wal = conn.pragma_update_and_check(None, "journal_mode", "WAL", |row| {
        row.get::<_, String>(0)
    });
    if wal.is_err() {
        conn.pragma_update(None, "journal_mode", "DELETE")
            .context("Failed to set journal_mode")?;
    }
    conn.pragma_update(None, "foreign_keys", true)
        .context("Failed to enable foreign keys")?;
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
            let mut cache = regex_cache.lock().unwrap();
            if !cache.contains_key(&pattern) {
                if cache.len() >= 64 {
                    cache.clear();
                }
                let compiled = regex::Regex::new(&pattern)
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
    Ok(conn)
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
             ALTER TABLE {table_db} RENAME TO metarecord_db;
             ALTER TABLE metarecord_db RENAME COLUMN {uuid_col} TO metarecord_uuid;
             ALTER TABLE field RENAME COLUMN {uuid_col} TO metarecord_uuid;
             DROP INDEX IF EXISTS idx_metadata_db;
             DROP INDEX IF EXISTS idx_record_db;
             CREATE INDEX IF NOT EXISTS idx_metarecord_db ON metarecord_db(db_id);
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
            uuid     BLOB    PRIMARY KEY NOT NULL,  -- 16-byte UUID
            version  INTEGER NOT NULL DEFAULT 0     -- bumped on every field write
        );

        -- One row per owning repository (usually one; two for link metarecords).
        CREATE TABLE metarecord_db (
            metarecord_uuid  BLOB NOT NULL REFERENCES metarecord(uuid) ON DELETE CASCADE,
            db_id          BLOB NOT NULL,
            PRIMARY KEY (metarecord_uuid, db_id)
        );
        CREATE INDEX idx_metarecord_db ON metarecord_db(db_id);

        CREATE TABLE field (
            id              INTEGER PRIMARY KEY AUTOINCREMENT,
            metarecord_uuid     BLOB    NOT NULL REFERENCES metarecord(uuid) ON DELETE CASCADE,
            field_name      TEXT    NOT NULL,
            value_type      TEXT    NOT NULL,
            value_text      TEXT,    -- string, datetime
            value_int       INTEGER, -- int, bool (0/1)
            value_real      REAL,    -- float
            value_uuid      BLOB,    -- ref/refbase/externalref: metarecord or repo UUID;
                                     -- tree_ref: parent UUID (zero UUID for roots)
            value_ref_repo  BLOB,    -- externalref only: repo UUID
            value_name      TEXT     -- tree_ref: name component
        );
        CREATE INDEX idx_field_metarecord ON field(metarecord_uuid, field_name);
        CREATE INDEX idx_field_reverse ON field(field_name, value_uuid, value_ref_repo)
            WHERE value_type IN ('ref', 'externalref');
        CREATE UNIQUE INDEX idx_field_tree ON field(field_name, value_uuid, value_name)
            WHERE value_type = 'tree_ref';

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
            value_name     TEXT
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
    let mut stmt =
        conn.prepare("SELECT db_id FROM metarecord_db WHERE metarecord_uuid = ?1 ORDER BY db_id")?;
    let db_ids = stmt
        .query_map(params![uuid_to_bytes(uuid)], |r| r.get::<_, Vec<u8>>(0))?
        .map(|r| r.map_err(Into::into).and_then(bytes_to_uuid))
        .collect::<Result<Vec<Uuid>>>()?;

    let fields = get_field_rows(conn, uuid)?
        .into_iter()
        .map(|r| Field { id: Some(r.id), name: r.name, value: r.value })
        .collect();

    Ok(Some(MetaRecord { uuid, db_ids, version, fields }))
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

/// Field rows of a metarecord restricted to one field name.
pub fn get_field_rows_named(conn: &Connection, uuid: Uuid, name: &str) -> Result<Vec<FieldRow>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {FIELD_COLUMNS} FROM field
         WHERE metarecord_uuid = ?1 AND field_name = ?2 ORDER BY id"
    ))?;
    let rows = stmt.query_map(params![uuid_to_bytes(uuid), name], row_to_field_row)?;
    collect_field_rows(rows)
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

/// All metarecord UUIDs owned exclusively by `db_id`, sorted by UUID byte order.
/// Link metarecords (several owners) are excluded, as mandated by spec-data-model.
pub fn list_entries(conn: &Connection, db_id: Uuid) -> Result<Vec<Uuid>> {
    let mut stmt = conn.prepare(
        "SELECT m1.metarecord_uuid FROM metarecord_db m1
         WHERE m1.db_id = ?1
           AND (SELECT COUNT(*) FROM metarecord_db m2
                WHERE m2.metarecord_uuid = m1.metarecord_uuid) = 1
         ORDER BY m1.metarecord_uuid",
    )?;
    let uuids = stmt
        .query_map(params![uuid_to_bytes(db_id)], |r| r.get::<_, Vec<u8>>(0))?
        .map(|r| r.map_err(Into::into).and_then(bytes_to_uuid))
        .collect::<Result<Vec<Uuid>>>()?;
    Ok(uuids)
}

/// All metarecords of this repository holding an `mfr_path` TreeRef (i.e. with
/// a known tree position, stale or not).
pub fn all_tracked_metarecords(conn: &Connection, db_id: Uuid) -> Result<Vec<Uuid>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT f.metarecord_uuid FROM field f
         JOIN metarecord_db md ON md.metarecord_uuid = f.metarecord_uuid AND md.db_id = ?1
         WHERE f.field_name = 'mfr_path' AND f.value_type = 'tree_ref'",
    )?;
    let uuids = stmt
        .query_map(params![uuid_to_bytes(db_id)], |r| r.get::<_, Vec<u8>>(0))?
        .map(|r| r.map_err(Into::into).and_then(bytes_to_uuid))
        .collect::<Result<Vec<Uuid>>>()?;
    Ok(uuids)
}

/// Orphaned metarecords of this repository (`mfr_path` = Nothing) whose stored
/// `mfr_size` matches. First step of the fingerprint cascade.
pub fn find_orphans_by_size(conn: &Connection, db_id: Uuid, size: i64) -> Result<Vec<Uuid>> {
    let mut stmt = conn.prepare(
        "SELECT p.metarecord_uuid FROM field p
         JOIN metarecord_db md ON md.metarecord_uuid = p.metarecord_uuid AND md.db_id = ?1
         JOIN field s ON s.metarecord_uuid = p.metarecord_uuid
              AND s.field_name = 'mfr_size' AND s.value_type = 'int' AND s.value_int = ?2
         WHERE p.field_name = 'mfr_path' AND p.value_type = 'nothing'",
    )?;
    let uuids = stmt
        .query_map(params![uuid_to_bytes(db_id), size], |r| r.get::<_, Vec<u8>>(0))?
        .map(|r| r.map_err(Into::into).and_then(bytes_to_uuid))
        .collect::<Result<Vec<Uuid>>>()?;
    Ok(uuids)
}

/// One page of [`list_entries`]: metarecords after `after` (exclusive), at most
/// `limit` rows, sorted by UUID byte order (keyset pagination).
pub fn list_entries_page(
    conn: &Connection,
    db_id: Uuid,
    after: Option<Uuid>,
    limit: usize,
) -> Result<Vec<Uuid>> {
    let mut stmt = conn.prepare(
        "SELECT m1.metarecord_uuid FROM metarecord_db m1
         WHERE m1.db_id = ?1
           AND (?2 IS NULL OR m1.metarecord_uuid > ?2)
           AND (SELECT COUNT(*) FROM metarecord_db m2
                WHERE m2.metarecord_uuid = m1.metarecord_uuid) = 1
         ORDER BY m1.metarecord_uuid LIMIT ?3",
    )?;
    let uuids = stmt
        .query_map(
            params![uuid_to_bytes(db_id), after.map(uuid_to_bytes), limit as i64],
            |r| r.get::<_, Vec<u8>>(0),
        )?
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
    let mut stmt = conn.prepare(
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
        Value::DateTime(s) => {
            e = EncodedValue::new("datetime");
            e.text = Some(s.clone());
        }
        Value::Ref(id) => {
            e = EncodedValue::new("ref");
            e.uuid = Some(uuid_to_bytes(*id));
        }
        Value::TreeRef { parent, name } => {
            e = EncodedValue::new("tree_ref");
            e.uuid = Some(uuid_to_bytes(parent.unwrap_or(ZERO_UUID)));
            e.name = Some(name.clone());
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
        "datetime" => Ok(Value::DateTime(text.context("value_text missing")?)),
        "ref" => Ok(Value::Ref(bytes_to_uuid(uuid.context("value_uuid missing")?)?)),
        "tree_ref" => {
            let parent = bytes_to_uuid(uuid.context("value_uuid missing")?)?;
            Ok(Value::TreeRef {
                parent: if parent == ZERO_UUID { None } else { Some(parent) },
                name: name.context("value_name missing")?,
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
        if err.to_string().contains("idx_field_tree") {
            anyhow::anyhow!("tree position already occupied for field '{name}'")
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
    Ok(conn.last_insert_rowid())
}
