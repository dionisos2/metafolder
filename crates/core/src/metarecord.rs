use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub type MetaRecordId = Uuid;
pub type DatabaseId = Uuid;

/// The zero UUID is reserved as the sentinel parent of `TreeRef` root nodes
/// in the database; it must never be assigned to a real metarecord.
pub const ZERO_UUID: Uuid = Uuid::nil();

/// Serde helpers encoding UUIDs as 32-char lowercase hex strings without
/// hyphens (the API encoding mandated by spec-data-model). Deserialization
/// also accepts the hyphenated form.
pub mod hex_uuid {
    use serde::{Deserialize, Deserializer, Serializer};
    use uuid::Uuid;

    pub fn serialize<S: Serializer>(u: &Uuid, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&u.as_simple().to_string())
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Uuid, D::Error> {
        let s = String::deserialize(d)?;
        Uuid::parse_str(&s).map_err(serde::de::Error::custom)
    }
}

/// Like [`hex_uuid`] for `Option<Uuid>`; `None` maps to JSON `null`.
pub mod hex_uuid_opt {
    use serde::{Deserialize, Deserializer, Serializer};
    use uuid::Uuid;

    pub fn serialize<S: Serializer>(u: &Option<Uuid>, s: S) -> Result<S::Ok, S::Error> {
        match u {
            Some(u) => s.serialize_str(&u.as_simple().to_string()),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Uuid>, D::Error> {
        let s = Option::<String>::deserialize(d)?;
        s.map(|s| Uuid::parse_str(&s).map_err(serde::de::Error::custom))
            .transpose()
    }
}

/// Like [`hex_uuid`] for `Vec<Uuid>`.
pub mod hex_uuid_vec {
    use serde::ser::SerializeSeq;
    use serde::{Deserialize, Deserializer, Serializer};
    use uuid::Uuid;

    pub fn serialize<S: Serializer>(v: &[Uuid], s: S) -> Result<S::Ok, S::Error> {
        let mut seq = s.serialize_seq(Some(v.len()))?;
        for u in v {
            seq.serialize_element(&u.as_simple().to_string())?;
        }
        seq.end()
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<Uuid>, D::Error> {
        let strings = Vec::<String>::deserialize(d)?;
        strings
            .into_iter()
            .map(|s| Uuid::parse_str(&s).map_err(serde::de::Error::custom))
            .collect()
    }
}

/// A field value. `Nothing` represents an explicit absence ("this field does
/// not apply"), distinct from the absence of the field itself ("unknown").
///
/// JSON form: `{"type": "<variant>", "value": <json_value>}`. UUIDs are
/// encoded as 32-char lowercase hex strings without hyphens.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "lowercase")]
pub enum Value {
    Nothing,
    String(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    /// ISO-8601 datetime: "2024-03-15T10:30:00Z"
    DateTime(String),
    /// Reference to another metarecord's UUID (same repo).
    Ref(#[serde(with = "hex_uuid")] MetaRecordId),
    /// Position in a named tree: parent metarecord (None for a root) + the name
    /// component contributed by this metarecord.
    #[serde(rename = "tree_ref")]
    TreeRef {
        #[serde(with = "hex_uuid_opt")]
        parent: Option<Uuid>,
        name: String,
    },
    /// Reference to a repository UUID.
    RefBase(#[serde(with = "hex_uuid")] DatabaseId),
    /// Cross-repo reference: (repo UUID, metarecord UUID).
    ExternalRef {
        #[serde(with = "hex_uuid")]
        repo: DatabaseId,
        #[serde(with = "hex_uuid")]
        metarecord: MetaRecordId,
    },
}

/// A field: name + value. Multiple fields can share the same name (multi-map).
/// `id` is the database row id; present in API responses, absent in requests.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Field {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<i64>,
    pub name: String,
    pub value: Value,
}

impl Field {
    pub fn new(name: impl Into<String>, value: Value) -> Self {
        Self { id: None, name: name.into(), value }
    }
}

/// The fundamental unit of the system. Files, tags, relations — everything is
/// a metarecord.
///
/// `db_ids` normally contains the single owning repository UUID; two UUIDs
/// mean a link metarecord shared between repositories. `version` is a monotonic
/// write counter managed exclusively by the daemon.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MetaRecord {
    #[serde(with = "hex_uuid")]
    pub uuid: MetaRecordId,
    #[serde(with = "hex_uuid_vec")]
    pub db_ids: Vec<DatabaseId>,
    pub version: u64,
    pub fields: Vec<Field>,
}

impl MetaRecord {
    pub fn new(db_id: DatabaseId) -> Self {
        Self {
            uuid: Uuid::new_v4(),
            db_ids: vec![db_id],
            version: 0,
            fields: Vec::new(),
        }
    }

    /// Returns all values for the field with this name (multi-map).
    pub fn get_all(&self, name: &str) -> Vec<&Value> {
        self.fields
            .iter()
            .filter(|f| f.name == name)
            .map(|f| &f.value)
            .collect()
    }

    /// Returns the first value for the field with this name, or None.
    pub fn get(&self, name: &str) -> Option<&Value> {
        self.fields.iter().find(|f| f.name == name).map(|f| &f.value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(v: &Value) -> Value {
        let json = serde_json::to_string(v).expect("serialization failed");
        serde_json::from_str(&json).expect("deserialization failed")
    }

    // ── Value: JSON format (spec-data-model) ─────────────────────────────────

    #[test]
    fn test_value_json_format_int() {
        let json = serde_json::to_string(&Value::Int(5)).unwrap();
        assert_eq!(json, r#"{"type":"int","value":5}"#);
    }

    #[test]
    fn test_value_json_format_nothing() {
        let json = serde_json::to_string(&Value::Nothing).unwrap();
        assert_eq!(json, r#"{"type":"nothing"}"#);
    }

    #[test]
    fn test_value_json_format_datetime() {
        let json = serde_json::to_string(&Value::DateTime("2024-03-15T10:30:00Z".into())).unwrap();
        assert_eq!(json, r#"{"type":"datetime","value":"2024-03-15T10:30:00Z"}"#);
    }

    #[test]
    fn test_value_json_format_ref_hex_without_hyphens() {
        let id = Uuid::parse_str("8f3a2b1c-4d5e-6f70-8192-a3b4c5d6e7f8").unwrap();
        let json = serde_json::to_string(&Value::Ref(id)).unwrap();
        assert_eq!(json, r#"{"type":"ref","value":"8f3a2b1c4d5e6f708192a3b4c5d6e7f8"}"#);
    }

    #[test]
    fn test_value_json_format_tree_ref() {
        let parent = Uuid::parse_str("8f3a2b1c-4d5e-6f70-8192-a3b4c5d6e7f8").unwrap();
        let v = Value::TreeRef { parent: Some(parent), name: "bar.mp3".into() };
        let json = serde_json::to_string(&v).unwrap();
        assert_eq!(
            json,
            r#"{"type":"tree_ref","value":{"parent":"8f3a2b1c4d5e6f708192a3b4c5d6e7f8","name":"bar.mp3"}}"#
        );
    }

    #[test]
    fn test_value_json_format_tree_ref_root() {
        let v = Value::TreeRef { parent: None, name: "".into() };
        let json = serde_json::to_string(&v).unwrap();
        assert_eq!(json, r#"{"type":"tree_ref","value":{"parent":null,"name":""}}"#);
    }

    #[test]
    fn test_value_json_format_refbase() {
        let id = Uuid::parse_str("47ab0000-0000-0000-0000-000000000001").unwrap();
        let json = serde_json::to_string(&Value::RefBase(id)).unwrap();
        assert_eq!(json, r#"{"type":"refbase","value":"47ab0000000000000000000000000001"}"#);
    }

    #[test]
    fn test_value_json_format_externalref() {
        let repo = Uuid::parse_str("47ab0000-0000-0000-0000-000000000001").unwrap();
        let metarecord = Uuid::parse_str("8f3a2b1c-4d5e-6f70-8192-a3b4c5d6e7f8").unwrap();
        let v = Value::ExternalRef { repo, metarecord };
        let json = serde_json::to_string(&v).unwrap();
        assert_eq!(
            json,
            r#"{"type":"externalref","value":{"repo":"47ab0000000000000000000000000001","metarecord":"8f3a2b1c4d5e6f708192a3b4c5d6e7f8"}}"#
        );
    }

    #[test]
    fn test_value_deserialize_accepts_hyphenated_uuid() {
        let v: Value =
            serde_json::from_str(r#"{"type":"ref","value":"8f3a2b1c-4d5e-6f70-8192-a3b4c5d6e7f8"}"#)
                .unwrap();
        let expected = Uuid::parse_str("8f3a2b1c4d5e6f708192a3b4c5d6e7f8").unwrap();
        assert_eq!(v, Value::Ref(expected));
    }

    // ── Value: roundtrips ────────────────────────────────────────────────────

    #[test]
    fn test_value_roundtrips() {
        let id = Uuid::new_v4();
        let repo = Uuid::new_v4();
        let cases = vec![
            Value::Nothing,
            Value::String("hello".into()),
            Value::Int(-42),
            Value::Float(3.25),
            Value::Bool(true),
            Value::Bool(false),
            Value::DateTime("2024-03-15T10:30:00Z".into()),
            Value::Ref(id),
            Value::TreeRef { parent: Some(id), name: "félins".into() },
            Value::TreeRef { parent: None, name: "tag1".into() },
            Value::RefBase(repo),
            Value::ExternalRef { repo, metarecord: id },
        ];
        for v in cases {
            assert_eq!(roundtrip(&v), v, "roundtrip failed for {v:?}");
        }
    }

    // ── Field: id handling ───────────────────────────────────────────────────

    #[test]
    fn test_field_id_omitted_when_none() {
        let f = Field::new("rating", Value::Int(5));
        let json = serde_json::to_string(&f).unwrap();
        assert_eq!(json, r#"{"name":"rating","value":{"type":"int","value":5}}"#);
    }

    #[test]
    fn test_field_id_present_when_some() {
        let f = Field { id: Some(42), name: "rating".into(), value: Value::Int(5) };
        let json = serde_json::to_string(&f).unwrap();
        assert_eq!(json, r#"{"id":42,"name":"rating","value":{"type":"int","value":5}}"#);
    }

    #[test]
    fn test_field_deserialize_without_id() {
        let f: Field =
            serde_json::from_str(r#"{"name":"rating","value":{"type":"int","value":5}}"#).unwrap();
        assert_eq!(f.id, None);
        assert_eq!(f.name, "rating");
    }

    // ── MetaRecord: JSON format ────────────────────────────────────────────────

    #[test]
    fn test_record_json_format() {
        let uuid = Uuid::parse_str("8f3a2b1c-4d5e-6f70-8192-a3b4c5d6e7f8").unwrap();
        let db_id = Uuid::parse_str("47ab0000-0000-0000-0000-000000000001").unwrap();
        let m = MetaRecord {
            uuid,
            db_ids: vec![db_id],
            version: 3,
            fields: vec![Field { id: Some(43), name: "rating".into(), value: Value::Int(5) }],
        };
        let json = serde_json::to_string(&m).unwrap();
        assert_eq!(
            json,
            r#"{"uuid":"8f3a2b1c4d5e6f708192a3b4c5d6e7f8","db_ids":["47ab0000000000000000000000000001"],"version":3,"fields":[{"id":43,"name":"rating","value":{"type":"int","value":5}}]}"#
        );
    }

    #[test]
    fn test_record_roundtrip() {
        let mut m = MetaRecord::new(Uuid::new_v4());
        m.version = 7;
        m.fields.push(Field::new("tag", Value::String("jazz".into())));
        m.fields.push(Field::new("tag", Value::String("live".into())));
        let json = serde_json::to_string(&m).unwrap();
        let back: MetaRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back, m);
    }

    // ── MetaRecord: accessors ──────────────────────────────────────────────────

    fn make_metarecord() -> MetaRecord {
        let mut e = MetaRecord::new(Uuid::new_v4());
        e.fields.push(Field::new("path", Value::String("/music/a.mp3".into())));
        e.fields.push(Field::new("tag", Value::String("jazz".into())));
        e.fields.push(Field::new("tag", Value::String("live".into())));
        e
    }

    #[test]
    fn test_get_returns_first_value() {
        let e = make_metarecord();
        assert_eq!(e.get("path"), Some(&Value::String("/music/a.mp3".into())));
    }

    #[test]
    fn test_get_unknown_field_returns_none() {
        let e = make_metarecord();
        assert_eq!(e.get("rating"), None);
    }

    #[test]
    fn test_get_all_multimap() {
        let e = make_metarecord();
        let tags = e.get_all("tag");
        assert_eq!(tags.len(), 2);
        assert!(tags.contains(&&Value::String("jazz".into())));
        assert!(tags.contains(&&Value::String("live".into())));
    }

    #[test]
    fn test_new_record_defaults() {
        let db_id = Uuid::new_v4();
        let e1 = MetaRecord::new(db_id);
        let e2 = MetaRecord::new(db_id);
        assert_ne!(e1.uuid, e2.uuid);
        assert_eq!(e1.db_ids, vec![db_id]);
        assert_eq!(e1.version, 0);
    }
}
