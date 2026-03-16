#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        conn
    }

    fn sample_fields() -> Vec<Field> {
        vec![
            Field { name: "path".to_string(),   value: Value::String("/music/a.mp3".to_string()) },
            Field { name: "rating".to_string(),  value: Value::Int(4) },
            Field { name: "active".to_string(),  value: Value::Bool(true) },
            Field { name: "score".to_string(),   value: Value::Float(8.5) },
            Field { name: "created".to_string(), value: Value::Date("2024-01-15".to_string()) },
            Field { name: "duration".to_string(),value: Value::Duration(210_000) },
            Field { name: "note".to_string(),    value: Value::Nothing },
        ]
    }

    // ── Existing functions ────────────────────────────────────────────────────

    #[test]
    fn test_create_and_get_roundtrip() {
        let conn = test_db();
        let db_id = Uuid::new_v4();
        let fields = sample_fields();

        let created = create_entry(&conn, db_id, fields).unwrap();
        let retrieved = get_entry(&conn, created.uuid).unwrap();

        assert_eq!(created.uuid, retrieved.uuid);
        assert_eq!(created.db_id, retrieved.db_id);
        assert_eq!(created.fields.len(), retrieved.fields.len());
    }

    #[test]
    fn test_all_value_types_roundtrip() {
        let conn = test_db();
        let db_id = Uuid::new_v4();

        // Insert a target entry so that the value_ref FK is valid
        let target = create_entry(&conn, db_id, vec![
            Field { name: "label".to_string(), value: Value::String("target".to_string()) }
        ]).unwrap();

        let fields = vec![
            Field { name: "a".to_string(), value: Value::Nothing },
            Field { name: "b".to_string(), value: Value::String("hello".to_string()) },
            Field { name: "c".to_string(), value: Value::Int(-99) },
            Field { name: "d".to_string(), value: Value::Float(1.23) },
            Field { name: "e".to_string(), value: Value::Bool(false) },
            Field { name: "f".to_string(), value: Value::Date("2023-06-01".to_string()) },
            Field { name: "g".to_string(), value: Value::DateTime("2023-06-01T12:00:00Z".to_string()) },
            Field { name: "h".to_string(), value: Value::Duration(5000) },
            Field { name: "i".to_string(), value: Value::Ref(target.uuid) },
        ];

        let created = create_entry(&conn, db_id, fields.clone()).unwrap();
        let retrieved = get_entry(&conn, created.uuid).unwrap();

        for (orig, ret) in fields.iter().zip(retrieved.fields.iter()) {
            assert_eq!(orig.name, ret.name, "field name mismatch");
            assert_eq!(orig.value, ret.value, "value mismatch for field '{}'", orig.name);
        }
    }

    #[test]
    fn test_multimap_fields() {
        let conn = test_db();
        let db_id = Uuid::new_v4();
        let fields = vec![
            Field { name: "tag".to_string(), value: Value::String("jazz".to_string()) },
            Field { name: "tag".to_string(), value: Value::String("live".to_string()) },
            Field { name: "tag".to_string(), value: Value::String("piano".to_string()) },
        ];

        let created = create_entry(&conn, db_id, fields).unwrap();
        let retrieved = get_entry(&conn, created.uuid).unwrap();

        assert_eq!(retrieved.fields.len(), 3);
        let tags: Vec<_> = retrieved.fields.iter()
            .filter(|f| f.name == "tag")
            .map(|f| &f.value)
            .collect();
        assert_eq!(tags.len(), 3);
    }

    #[test]
    fn test_get_nonexistent_entry_returns_error() {
        let conn = test_db();
        let result = get_entry(&conn, Uuid::nil());
        assert!(result.is_err(), "should return an error for a nonexistent UUID");
    }

    // ── TDD : find_entry_by_path ──────────────────────────────────────────────

    #[test]
    fn test_find_entry_by_path_found() {
        let conn = test_db();
        let created = create_entry(&conn, Uuid::new_v4(), vec![
            Field { name: "path".to_string(), value: Value::String("/tmp/foo.txt".to_string()) },
        ]).unwrap();
        let result = find_entry_by_path(&conn, "/tmp/foo.txt").unwrap();
        assert_eq!(result, Some(created.uuid));
    }

    #[test]
    fn test_find_entry_by_path_not_found() {
        let conn = test_db();
        let result = find_entry_by_path(&conn, "/nonexistent.txt").unwrap();
        assert_eq!(result, None);
    }

    // ── TDD : clear_path ─────────────────────────────────────────────────────

    #[test]
    fn test_clear_path_sets_value_to_nothing() {
        let conn = test_db();
        let created = create_entry(&conn, Uuid::new_v4(), vec![
            Field { name: "path".to_string(),   value: Value::String("/tmp/foo.txt".to_string()) },
            Field { name: "rating".to_string(), value: Value::Int(5) },
        ]).unwrap();

        clear_path(&conn, created.uuid).unwrap();

        let retrieved = get_entry(&conn, created.uuid).unwrap();
        let path_field = retrieved.fields.iter().find(|f| f.name == "path").unwrap();
        assert_eq!(path_field.value, Value::Nothing, "path should be Nothing after clear");

        let rating_field = retrieved.fields.iter().find(|f| f.name == "rating").unwrap();
        assert_eq!(rating_field.value, Value::Int(5), "other fields should be unchanged");
    }

    // ── TDD : delete_entry ────────────────────────────────────────────────────

    #[test]
    fn test_delete_entry_removes_entry_and_fields() {
        let conn = test_db();
        let db_id = Uuid::new_v4();
        let created = create_entry(&conn, db_id, sample_fields()).unwrap();

        // Before: the entry exists
        assert!(get_entry(&conn, created.uuid).is_ok());

        delete_entry(&conn, created.uuid).unwrap();

        // After: the entry no longer exists
        assert!(get_entry(&conn, created.uuid).is_err(), "the entry should have been deleted");

        // Fields too (CASCADE)
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM field WHERE metadata_uuid = ?1", // field FK refs metadata
            rusqlite::params![created.uuid.as_bytes().to_vec()],
            |row| row.get(0),
        ).unwrap();
        assert_eq!(count, 0, "fields should have been deleted");
    }
}

use anyhow::{bail, Context};
use rusqlite::{params, Connection};
use uuid::Uuid;

use metafolder_core::entry::{DatabaseId, EntryId, Field, Metadata, Value};

// ── UUID ↔ BLOB helpers ───────────────────────────────────────────────────────

fn uuid_to_bytes(uuid: Uuid) -> Vec<u8> {
    uuid.as_bytes().to_vec()
}

fn bytes_to_uuid(bytes: Vec<u8>) -> anyhow::Result<Uuid> {
    let arr: [u8; 16] = bytes
        .try_into()
        .map_err(|_| anyhow::anyhow!("Invalid UUID blob: expected 16 bytes"))?;
    Ok(Uuid::from_bytes(arr))
}

// ── Schema initialization ─────────────────────────────────────────────────────

pub fn init_schema(conn: &Connection) -> anyhow::Result<()> {
    conn.execute_batch(
        "
        PRAGMA journal_mode = WAL;
        PRAGMA foreign_keys = ON;

        CREATE TABLE IF NOT EXISTS metadata (
            uuid   BLOB PRIMARY KEY NOT NULL,  -- 16-byte UUID
            db_id  BLOB NOT NULL               -- 16-byte UUID
        );

        -- Each row is one field of a metadata (multi-map: multiple rows
        -- can share the same metadata_uuid + field_name).
        CREATE TABLE IF NOT EXISTS field (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            metadata_uuid  BLOB    NOT NULL REFERENCES metadata(uuid) ON DELETE CASCADE,
            field_name  TEXT    NOT NULL,
            value_type  TEXT    NOT NULL,  -- see Value discriminant
            value_str   TEXT,              -- String, Date, DateTime
            value_int   INTEGER,           -- Int, Bool (0/1), Duration (ms)
            value_real  REAL,              -- Float
            value_ref   BLOB    REFERENCES metadata(uuid)  -- Ref (16-byte UUID)
        );

        CREATE INDEX IF NOT EXISTS idx_field_entry
            ON field(metadata_uuid, field_name);

        CREATE INDEX IF NOT EXISTS idx_field_reverse
            ON field(field_name, value_ref);
        ",
    )
    .context("Failed to initialize the SQLite schema")
}

// ── Value ↔ SQLite conversions ────────────────────────────────────────────────

/// Inserts a field into the `field` table.
pub fn insert_field(conn: &Connection, metadata_uuid: Uuid, field: &Field) -> anyhow::Result<()> {
    let entry_blob = uuid_to_bytes(metadata_uuid);
    match &field.value {
        Value::Nothing => {
            conn.execute(
                "INSERT INTO field (metadata_uuid, field_name, value_type)
                 VALUES (?1, ?2, 'nothing')",
                params![entry_blob, field.name],
            )?;
        }
        Value::String(s) => {
            conn.execute(
                "INSERT INTO field (metadata_uuid, field_name, value_type, value_str)
                 VALUES (?1, ?2, 'string', ?3)",
                params![entry_blob, field.name, s],
            )?;
        }
        Value::Int(n) => {
            conn.execute(
                "INSERT INTO field (metadata_uuid, field_name, value_type, value_int)
                 VALUES (?1, ?2, 'int', ?3)",
                params![entry_blob, field.name, n],
            )?;
        }
        Value::Float(f) => {
            conn.execute(
                "INSERT INTO field (metadata_uuid, field_name, value_type, value_real)
                 VALUES (?1, ?2, 'float', ?3)",
                params![entry_blob, field.name, f],
            )?;
        }
        Value::Bool(b) => {
            conn.execute(
                "INSERT INTO field (metadata_uuid, field_name, value_type, value_int)
                 VALUES (?1, ?2, 'bool', ?3)",
                params![entry_blob, field.name, *b as i64],
            )?;
        }
        Value::Date(s) => {
            conn.execute(
                "INSERT INTO field (metadata_uuid, field_name, value_type, value_str)
                 VALUES (?1, ?2, 'date', ?3)",
                params![entry_blob, field.name, s],
            )?;
        }
        Value::DateTime(s) => {
            conn.execute(
                "INSERT INTO field (metadata_uuid, field_name, value_type, value_str)
                 VALUES (?1, ?2, 'datetime', ?3)",
                params![entry_blob, field.name, s],
            )?;
        }
        Value::Duration(ms) => {
            conn.execute(
                "INSERT INTO field (metadata_uuid, field_name, value_type, value_int)
                 VALUES (?1, ?2, 'duration', ?3)",
                params![entry_blob, field.name, ms],
            )?;
        }
        Value::Ref(id) => {
            conn.execute(
                "INSERT INTO field (metadata_uuid, field_name, value_type, value_ref)
                 VALUES (?1, ?2, 'ref', ?3)",
                params![entry_blob, field.name, uuid_to_bytes(*id)],
            )?;
        }
    }
    Ok(())
}

/// Reconstructs a `Value` from a row in the `field` table.
fn row_to_value(
    value_type: &str,
    value_str: Option<String>,
    value_int: Option<i64>,
    value_real: Option<f64>,
    value_ref: Option<Vec<u8>>,
) -> anyhow::Result<Value> {
    match value_type {
        "nothing" => Ok(Value::Nothing),
        "string" => Ok(Value::String(value_str.context("value_str missing")?)),
        "int" => Ok(Value::Int(value_int.context("value_int missing")?)),
        "float" => Ok(Value::Float(value_real.context("value_real missing")?)),
        "bool" => Ok(Value::Bool(value_int.context("value_int missing")? != 0)),
        "date" => Ok(Value::Date(value_str.context("value_str missing")?)),
        "datetime" => Ok(Value::DateTime(value_str.context("value_str missing")?)),
        "duration" => Ok(Value::Duration(value_int.context("value_int missing")?)),
        "ref" => {
            let bytes = value_ref.context("value_ref missing")?;
            Ok(Value::Ref(bytes_to_uuid(bytes)?))
        }
        other => bail!("Unknown value type: '{other}'"),
    }
}

// ── CRUD ──────────────────────────────────────────────────────────────────────

/// Creates a new entry with its fields. Returns the created entry.
pub fn create_entry(
    conn: &Connection,
    db_id: DatabaseId,
    fields: Vec<Field>,
) -> anyhow::Result<Metadata> {
    let entry = Metadata::new(db_id);

    conn.execute(
        "INSERT INTO metadata (uuid, db_id) VALUES (?1, ?2)",
        params![uuid_to_bytes(entry.uuid), uuid_to_bytes(entry.db_id)],
    )
    .context("Failed to insert entry")?;

    for field in &fields {
        insert_field(conn, entry.uuid, field)
            .with_context(|| format!("Failed to insert field '{}'", field.name))?;
    }

    Ok(Metadata {
        uuid: entry.uuid,
        db_id: entry.db_id,
        fields,
    })
}

/// Deletes an entry and all its fields (CASCADE via FK).
pub fn delete_entry(conn: &Connection, uuid: Uuid) -> anyhow::Result<()> {
    let rows = conn
        .execute("DELETE FROM metadata WHERE uuid = ?1", params![uuid_to_bytes(uuid)])
        .context("Error during deletion")?;

    if rows == 0 {
        anyhow::bail!("Entry not found: {uuid}");
    }
    Ok(())
}

/// Sets the `path` field of an entry to `Nothing`, preserving all other fields.
pub fn clear_path(conn: &Connection, metadata_uuid: Uuid) -> anyhow::Result<()> {
    conn.execute(
        "UPDATE field SET value_type = 'nothing', value_str = NULL
         WHERE metadata_uuid = ?1 AND field_name = 'path'",
        params![uuid_to_bytes(metadata_uuid)],
    )?;
    Ok(())
}

/// Finds the UUID of the entry whose `path` field matches the given string.
pub fn find_entry_by_path(conn: &Connection, path: &str) -> anyhow::Result<Option<Uuid>> {
    let mut stmt = conn.prepare(
        "SELECT metadata_uuid FROM field
         WHERE field_name = 'path' AND value_type = 'string' AND value_str = ?1
         LIMIT 1",
    )?;
    let mut rows = stmt.query(params![path])?;
    rows.next()?
        .map(|r| r.get::<_, Vec<u8>>(0).map_err(Into::into).and_then(bytes_to_uuid))
        .transpose()
}

/// Retrieves an entry and all its fields by UUID.
pub fn get_entry(conn: &Connection, uuid: Uuid) -> anyhow::Result<Metadata> {
    let (uuid_blob, db_id_blob): (Vec<u8>, Vec<u8>) = conn
        .query_row(
            "SELECT uuid, db_id FROM metadata WHERE uuid = ?1",
            params![uuid_to_bytes(uuid)],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .context("Entry not found")?;

    let db_id: DatabaseId = bytes_to_uuid(db_id_blob)?;
    let entry_id: EntryId = bytes_to_uuid(uuid_blob)?;

    let mut stmt = conn
        .prepare(
            "SELECT field_name, value_type, value_str, value_int, value_real, value_ref
             FROM field WHERE metadata_uuid = ?1",
        )
        .context("Failed to prepare query")?;

    let fields: Vec<Field> = stmt
        .query_map(params![uuid_to_bytes(uuid)], |row| {
            Ok((
                row.get::<_, String>(0)?,           // field_name
                row.get::<_, String>(1)?,           // value_type
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<i64>>(3)?,
                row.get::<_, Option<f64>>(4)?,
                row.get::<_, Option<Vec<u8>>>(5)?,  // value_ref (BLOB)
            ))
        })
        .context("Error reading fields")?
        .map(|r| {
            let (name, vtype, vstr, vint, vreal, vref) = r?;
            let value = row_to_value(&vtype, vstr, vint, vreal, vref)?;
            Ok(Field { name, value })
        })
        .collect::<anyhow::Result<Vec<Field>>>()?;

    Ok(Metadata {
        uuid: entry_id,
        db_id,
        fields,
    })
}
