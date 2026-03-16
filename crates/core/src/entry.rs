use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub type EntryId = Uuid;
pub type DatabaseId = Uuid;

/// A field value. `Nothing` represents an explicit absence ("I know this field
/// does not apply"), distinct from the absence of the field itself ("unknown").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "lowercase")]
pub enum Value {
    Nothing,
    String(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Date(String),       // ISO 8601 : "2024-03-15"
    DateTime(String),   // ISO 8601 : "2024-03-15T10:30:00Z"
    Duration(i64),      // millisecondes
    Ref(EntryId),       // référence vers une autre entrée
}

/// A field: name + value. Multiple fields can share the same name (multi-map).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Field {
    pub name: String,
    pub value: Value,
}

/// The fundamental unit of the system. Files, tags, relations — everything is a Metadata.
///
/// - A file is an entry with `path` and `hash` fields.
/// - A tag is an entry with a `label` field and optionally a `parent` field.
/// - A preference relation is an entry with named fields pointing to other entries.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Metadata {
    pub uuid: EntryId,
    pub db_id: DatabaseId,
    pub fields: Vec<Field>,
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Value : JSON serialization ────────────────────────────────────────────

    fn roundtrip(v: &Value) -> Value {
        let json = serde_json::to_string(v).expect("serialization failed");
        serde_json::from_str(&json).expect("deserialization failed")
    }

    #[test]
    fn test_value_nothing_roundtrip() {
        assert_eq!(roundtrip(&Value::Nothing), Value::Nothing);
    }

    #[test]
    fn test_value_string_roundtrip() {
        let v = Value::String("hello".to_string());
        assert_eq!(roundtrip(&v), v);
    }

    #[test]
    fn test_value_int_roundtrip() {
        assert_eq!(roundtrip(&Value::Int(-42)), Value::Int(-42));
    }

    #[test]
    fn test_value_float_roundtrip() {
        assert_eq!(roundtrip(&Value::Float(3.14)), Value::Float(3.14));
    }

    #[test]
    fn test_value_bool_roundtrip() {
        assert_eq!(roundtrip(&Value::Bool(true)), Value::Bool(true));
        assert_eq!(roundtrip(&Value::Bool(false)), Value::Bool(false));
    }

    #[test]
    fn test_value_date_roundtrip() {
        let v = Value::Date("2024-03-15".to_string());
        assert_eq!(roundtrip(&v), v);
    }

    #[test]
    fn test_value_ref_roundtrip() {
        let id = Uuid::new_v4();
        let v = Value::Ref(id);
        assert_eq!(roundtrip(&v), v);
    }

    #[test]
    fn test_value_json_format() {
        // Vérifie que le format JSON est bien {"type": "int", "value": 5}
        let json = serde_json::to_string(&Value::Int(5)).unwrap();
        assert_eq!(json, r#"{"type":"int","value":5}"#);
    }

    // ── Metadata : accessors ────────────────────────────────────────────

    fn make_entry() -> Metadata {
        let db_id = Uuid::new_v4();
        let mut e = Metadata::new(db_id);
        e.fields.push(Field { name: "path".to_string(),   value: Value::String("/music/a.mp3".to_string()) });
        e.fields.push(Field { name: "tag".to_string(),    value: Value::String("jazz".to_string()) });
        e.fields.push(Field { name: "tag".to_string(),    value: Value::String("live".to_string()) });
        e
    }

    #[test]
    fn test_get_returns_first_value() {
        let e = make_entry();
        assert_eq!(e.get("path"), Some(&Value::String("/music/a.mp3".to_string())));
    }

    #[test]
    fn test_get_unknown_field_returns_none() {
        let e = make_entry();
        assert_eq!(e.get("rating"), None);
    }

    #[test]
    fn test_get_all_multimap() {
        let e = make_entry();
        let tags = e.get_all("tag");
        assert_eq!(tags.len(), 2);
        assert!(tags.contains(&&Value::String("jazz".to_string())));
        assert!(tags.contains(&&Value::String("live".to_string())));
    }

    #[test]
    fn test_new_entry_has_unique_uuid() {
        let db_id = Uuid::new_v4();
        let e1 = Metadata::new(db_id);
        let e2 = Metadata::new(db_id);
        assert_ne!(e1.uuid, e2.uuid);
    }
}

impl Metadata {
    pub fn new(db_id: DatabaseId) -> Self {
        Self {
            uuid: Uuid::new_v4(),
            db_id,
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
