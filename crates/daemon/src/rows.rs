//! A repository's rows as the storage layer hands them over, and the column
//! form a value is stored in: `value_type` and the one or two columns its type
//! uses. The key-value store keeps its rows in that form, and the log's
//! snapshots are shown in it (spec-event-log), so it is a persisted format:
//! changing it is a migration.

use anyhow::{bail, Context, Result};
use uuid::Uuid;

use metafolder_core::metarecord::{TreeName, Value, ZERO_UUID};

/// One row of the `field` table, decoded.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldRow {
    pub id: i64,
    pub name: String,
    pub value: Value,
}

pub fn uuid_to_bytes(uuid: Uuid) -> Vec<u8> {
    uuid.as_bytes().to_vec()
}

pub fn bytes_to_uuid(bytes: Vec<u8>) -> Result<Uuid> {
    let arr: [u8; 16] =
        bytes.try_into().map_err(|_| anyhow::anyhow!("Invalid UUID blob: expected 16 bytes"))?;
    Ok(Uuid::from_bytes(arr))
}

/// The stored content hashes of one metarecord, with the `stat` stamp they were
/// computed under (doc "The duplicate hash cache").
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct StoredHashes {
    pub partial: Option<String>,
    pub full: Option<String>,
    /// `(mtime_ms, size)`. `None` when either half is missing, which makes the
    /// entry unusable — an unstamped hash is one no scan may trust.
    pub stamp: Option<(i64, i64)>,
}

/// A `duplicate_group` metarecord as stored: its uuid and the counters the last
/// scan wrote, so a re-scan can tell an unchanged counter from a changed one
/// without asking the database again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateGroup {
    pub uuid: Uuid,
    pub count: Option<i64>,
    pub reclaimable: Option<i64>,
}

/// An orphaned metarecord a re-appearing file can be matched against: its
/// size and the two stored hashes the fingerprint cascade compares.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanCandidate {
    pub uuid: Uuid,
    pub size: i64,
    pub partial_hash: String,
    pub full_hash: String,
}

/// One TreeRef position: the metarecord, the field name whose forest it belongs
/// to, its parent (`None` = a forest root) and the name component it contributes.
pub struct TreeRow {
    /// The `field` row that holds this position; a metarecord's positions are
    /// listed in its order.
    pub id: i64,
    pub field_name: String,
    pub uuid: Uuid,
    pub parent: Option<Uuid>,
    /// The name's exact bytes — what identifies the node. Reading back the
    /// *displayed* name here would give a node named with an undecodable byte
    /// the bytes of its own escape (`caf%E9.mp4`), and it would then answer to
    /// neither reading (spec-data-model "Tree names").
    pub name: TreeName,
}

/// Column values for one `field` (or `op_snapshot`) row.
pub(crate) struct EncodedValue {
    pub value_type: &'static str,
    pub text: Option<String>,
    pub int: Option<i64>,
    pub real: Option<f64>,
    pub uuid: Option<Vec<u8>>,
    pub ref_repo: Option<Vec<u8>>,
    pub name: Option<String>,
    /// tree_ref only: the name's exact bytes — what identifies the node.
    pub name_bytes: Option<Vec<u8>>,
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
            name_bytes: None,
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
            // Both: the text is what queries and displays read, the bytes are
            // what identifies the node (spec-data-model "Tree names").
            e.name = Some(name.display().into_owned());
            e.name_bytes = Some(name.as_bytes().to_vec());
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

/// The value columns of one row, from any table that stores a value (`field`,
/// `op_snapshot`, `snapshot_field`, …).
///
/// Read *by column name* rather than by position: the set grows over time —
/// `value_name_bytes` was the latest — and every positional reader silently
/// shifted when it did, which no compiler catches.
pub struct RawValue {
    pub value_type: String,
    pub text: Option<String>,
    pub int: Option<i64>,
    pub real: Option<f64>,
    pub uuid: Option<Vec<u8>>,
    pub ref_repo: Option<Vec<u8>>,
    pub name: Option<String>,
    pub name_bytes: Option<Vec<u8>>,
}

pub(crate) fn decode_value(raw: RawValue) -> Result<Value> {
    let RawValue { value_type, text, int, real, uuid, ref_repo, name, name_bytes } = raw;
    match value_type.as_str() {
        "nothing" => Ok(Value::Nothing),
        "string" => Ok(Value::String(text.context("value_text missing")?)),
        "int" => Ok(Value::Int(int.context("value_int missing")?)),
        "float" => Ok(Value::Float(real.context("value_real missing")?)),
        "bool" => Ok(Value::Bool(int.context("value_int missing")? != 0)),
        "datetime" => Ok(Value::DateTime(int.context("value_int missing")?)),
        "ref" => Ok(Value::Ref(bytes_to_uuid(uuid.context("value_uuid missing")?)?)),
        "tree_ref" => {
            let parent = bytes_to_uuid(uuid.context("value_uuid missing")?)?;
            // The bytes are authoritative; the text is only a fallback for a
            // row written before the column existed (the migration back-fills
            // them, so this is belt and braces).
            let name = match name_bytes {
                Some(bytes) => TreeName::from_bytes(bytes),
                None => TreeName::from(name.context("value_name missing")?),
            };
            Ok(Value::TreeRef {
                parent: if parent == ZERO_UUID { None } else { Some(parent) },
                name,
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
