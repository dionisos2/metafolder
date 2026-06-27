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

/// Serde helper for `DateTime`: stored internally as Unix milliseconds (UTC),
/// but encoded on the JSON wire as an ISO-8601 string (`YYYY-MM-DDTHH:MM:SSZ`)
/// for readability. Deserialization rejects a non-parsable datetime string.
pub mod iso_ms {
    use crate::date;
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(ms: &i64, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&date::iso8601_from_ms(*ms))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<i64, D::Error> {
        let s = String::deserialize(d)?;
        date::iso_to_ms(&s)
            .ok_or_else(|| serde::de::Error::custom(format!("invalid ISO-8601 datetime: {s}")))
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
    /// A datetime as Unix milliseconds (UTC), encoded on the JSON wire as an
    /// ISO-8601 string ("2024-03-15T10:30:00Z"). See [`iso_ms`].
    DateTime(#[serde(with = "iso_ms")] i64),
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

/// The value types a field can be retyped to (spec-data-model "Changing a
/// field's type"). The target may be *any* type, including the reference
/// variants: a mis-typed field must always be escapable. The string names match
/// the JSON `type` tags of [`Value`] and the DB `value_type` column.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldType {
    String,
    Int,
    Float,
    Bool,
    DateTime,
    Ref,
    TreeRef,
    RefBase,
    ExternalRef,
}

impl FieldType {
    pub fn as_str(self) -> &'static str {
        match self {
            FieldType::String => "string",
            FieldType::Int => "int",
            FieldType::Float => "float",
            FieldType::Bool => "bool",
            FieldType::DateTime => "datetime",
            FieldType::Ref => "ref",
            FieldType::TreeRef => "tree_ref",
            FieldType::RefBase => "refbase",
            FieldType::ExternalRef => "externalref",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "string" => Some(FieldType::String),
            "int" => Some(FieldType::Int),
            "float" => Some(FieldType::Float),
            "bool" => Some(FieldType::Bool),
            "datetime" => Some(FieldType::DateTime),
            "ref" => Some(FieldType::Ref),
            "tree_ref" => Some(FieldType::TreeRef),
            "refbase" => Some(FieldType::RefBase),
            "externalref" => Some(FieldType::ExternalRef),
            _ => None,
        }
    }
}

impl Value {
    /// Converts this value to `target`, for the `retype` operation. Returns the
    /// converted value and whether it fell back to the target's *sentinel*
    /// (because the source could not be meaningfully converted) — the sentinels
    /// are chosen to be easily found afterwards: scalars `""`/`0`/`0.0`/`false`/
    /// the Unix epoch, the reference types `Nothing` (findable via IS ABSENT).
    ///
    /// `String` is the universal hub: every type ↔ `String` is lossless and
    /// reversible (display form ⇄ parse). Other lossless scalar coercions are
    /// kept (`Int↔Float`, `Bool→Int` as `0/1`, `DateTime↔Int` as Unix-ms).
    /// Same-type is identity. Every other cross-type pair (a reference to a
    /// non-`String` scalar, a scalar to a reference, or one reference type to a
    /// different one) has no meaningful coercion and falls back to the sentinel;
    /// route through `String` explicitly to reinterpret. `Nothing` is preserved
    /// (the caller skips it; never converted).
    pub fn convert_to(&self, target: FieldType) -> (Value, bool) {
        use FieldType as T;
        // Sentinel for an impossible conversion: the reference types collapse to
        // Nothing (no fabricated uuid), the scalars to their neutral element.
        let sentinel = || match target {
            T::String => Value::String(String::new()),
            T::Int => Value::Int(0),
            T::Float => Value::Float(0.0),
            T::Bool => Value::Bool(false),
            T::DateTime => Value::DateTime(0),
            T::Ref | T::TreeRef | T::RefBase | T::ExternalRef => Value::Nothing,
        };
        let ok = |v: Value| (v, false);
        let fallback = || (sentinel(), true);

        match (self, target) {
            (Value::Nothing, _) => (Value::Nothing, false),

            // → String: every value has a canonical display form (the inverse of
            // the `String →` parses below), so the round trip is lossless.
            (_, T::String) => ok(Value::String(self.display_form())),

            // → Int.
            (Value::Int(n), T::Int) => ok(Value::Int(*n)),
            (Value::Float(f), T::Int) => ok(Value::Int(*f as i64)),
            (Value::Bool(b), T::Int) => ok(Value::Int(*b as i64)),
            (Value::DateTime(ms), T::Int) => ok(Value::Int(*ms)),
            (Value::String(s), T::Int) => s.trim().parse::<i64>().map(Value::Int).map_or_else(|_| fallback(), ok),

            // → Float.
            (Value::Float(f), T::Float) => ok(Value::Float(*f)),
            (Value::Int(n), T::Float) => ok(Value::Float(*n as f64)),
            (Value::Bool(b), T::Float) => ok(Value::Float(*b as i64 as f64)),
            (Value::DateTime(ms), T::Float) => ok(Value::Float(*ms as f64)),
            (Value::String(s), T::Float) => s.trim().parse::<f64>().map(Value::Float).map_or_else(|_| fallback(), ok),

            // → Bool.
            (Value::Bool(b), T::Bool) => ok(Value::Bool(*b)),
            (Value::Int(n), T::Bool) => ok(Value::Bool(*n != 0)),
            (Value::Float(f), T::Bool) => ok(Value::Bool(*f != 0.0)),
            (Value::DateTime(ms), T::Bool) => ok(Value::Bool(*ms != 0)),
            (Value::String(s), T::Bool) => match s.trim() {
                "true" => ok(Value::Bool(true)),
                "false" => ok(Value::Bool(false)),
                _ => fallback(),
            },

            // → DateTime.
            (Value::DateTime(ms), T::DateTime) => ok(Value::DateTime(*ms)),
            (Value::Int(n), T::DateTime) => ok(Value::DateTime(*n)),
            (Value::Float(f), T::DateTime) => ok(Value::DateTime(*f as i64)),
            (Value::String(s), T::DateTime) => {
                crate::date::iso_to_ms(s.trim()).map(Value::DateTime).map_or_else(fallback, ok)
            }

            // → reference types: same type is identity; a String parses from its
            // display form; everything else has no coercion.
            (Value::Ref(u), T::Ref) => ok(Value::Ref(*u)),
            (Value::String(s), T::Ref) => parse_hex_uuid(s).map(Value::Ref).map_or_else(fallback, ok),
            (Value::RefBase(u), T::RefBase) => ok(Value::RefBase(*u)),
            (Value::String(s), T::RefBase) => {
                parse_hex_uuid(s).map(Value::RefBase).map_or_else(fallback, ok)
            }
            (Value::TreeRef { parent, name }, T::TreeRef) => {
                ok(Value::TreeRef { parent: *parent, name: name.clone() })
            }
            (Value::String(s), T::TreeRef) => parse_tree_ref(s).map_or_else(fallback, ok),
            (Value::ExternalRef { repo, metarecord }, T::ExternalRef) => {
                ok(Value::ExternalRef { repo: *repo, metarecord: *metarecord })
            }
            (Value::String(s), T::ExternalRef) => parse_external_ref(s).map_or_else(fallback, ok),

            // Any remaining cross-type pair has no meaningful coercion.
            _ => fallback(),
        }
    }

    /// The canonical, reversible textual form used by `→ String` retype (and
    /// parsed back by `String →`). For UUIDs this is the 32-char hex encoding.
    fn display_form(&self) -> String {
        match self {
            Value::Nothing => String::new(),
            Value::String(s) => s.clone(),
            Value::Int(n) => n.to_string(),
            Value::Float(f) => f.to_string(),
            Value::Bool(b) => b.to_string(),
            Value::DateTime(ms) => crate::date::iso8601_from_ms(*ms),
            Value::Ref(u) => u.as_simple().to_string(),
            Value::RefBase(u) => u.as_simple().to_string(),
            Value::TreeRef { parent, name } => match parent {
                Some(p) => format!("{}/{name}", p.as_simple()),
                None => format!("/{name}"),
            },
            Value::ExternalRef { repo, metarecord } => {
                format!("{}:{}", repo.as_simple(), metarecord.as_simple())
            }
        }
    }
}

/// Parses a 32-char (or hyphenated) hex UUID, trimming surrounding whitespace.
fn parse_hex_uuid(s: &str) -> Option<Uuid> {
    Uuid::parse_str(s.trim()).ok()
}

/// Parses the `String → TreeRef` form (the same grammar the CLI field-spec and
/// the GUI widget use): `<parent_hex>/<name>` for a child, `/<name>` for a root.
/// `name` must be a single non-empty path component.
fn parse_tree_ref(s: &str) -> Option<Value> {
    let (parent, name) = s.trim().split_once('/')?;
    if name.is_empty() || name.contains('/') {
        return None;
    }
    let parent = if parent.is_empty() { None } else { Some(parse_hex_uuid(parent)?) };
    Some(Value::TreeRef { parent, name: name.to_string() })
}

/// Parses the `String → ExternalRef` form `<repo_hex>:<metarecord_hex>`.
fn parse_external_ref(s: &str) -> Option<Value> {
    let (repo, metarecord) = s.trim().split_once(':')?;
    Some(Value::ExternalRef { repo: parse_hex_uuid(repo)?, metarecord: parse_hex_uuid(metarecord)? })
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

    // ── Value: retype conversions ────────────────────────────────────────────

    #[test]
    fn test_convert_lossless_and_display() {
        use FieldType as T;
        assert_eq!(Value::Int(5).convert_to(T::String), (Value::String("5".into()), false));
        assert_eq!(Value::Int(5).convert_to(T::Float), (Value::Float(5.0), false));
        assert_eq!(Value::Bool(true).convert_to(T::Int), (Value::Int(1), false));
        assert_eq!(Value::String("5".into()).convert_to(T::Int), (Value::Int(5), false));
        // Float→Int truncates (a real conversion, not a sentinel fallback).
        assert_eq!(Value::Float(4.5).convert_to(T::Int), (Value::Int(4), false));
    }

    #[test]
    fn test_convert_datetime_roundtrip() {
        use FieldType as T;
        let ms = crate::date::iso_to_ms("2024-03-15T10:30:00Z").unwrap();
        let (as_str, fell) = Value::DateTime(ms).convert_to(T::String);
        assert_eq!((as_str.clone(), fell), (Value::String("2024-03-15T10:30:00Z".into()), false));
        assert_eq!(as_str.convert_to(T::DateTime), (Value::DateTime(ms), false));
        // DateTime ↔ Int as Unix-ms (lossless).
        assert_eq!(Value::DateTime(ms).convert_to(T::Int), (Value::Int(ms), false));
        assert_eq!(Value::Int(ms).convert_to(T::DateTime), (Value::DateTime(ms), false));
    }

    #[test]
    fn test_convert_impossible_falls_back_to_sentinel() {
        use FieldType as T;
        assert_eq!(Value::String("x".into()).convert_to(T::Int), (Value::Int(0), true));
        assert_eq!(Value::String("x".into()).convert_to(T::Float), (Value::Float(0.0), true));
        assert_eq!(Value::String("x".into()).convert_to(T::Bool), (Value::Bool(false), true));
        assert_eq!(Value::String("x".into()).convert_to(T::DateTime), (Value::DateTime(0), true));
        // A reference to a non-String scalar has no coercion → scalar sentinel.
        assert_eq!(Value::Ref(Uuid::nil()).convert_to(T::Int), (Value::Int(0), true));
    }

    #[test]
    fn test_convert_nothing_is_preserved() {
        assert_eq!(Value::Nothing.convert_to(FieldType::Int), (Value::Nothing, false));
    }

    // ── Value: any-to-any retype via the reference types ─────────────────────

    #[test]
    fn test_convert_reference_to_string_is_lossless_display() {
        use FieldType as T;
        let u = Uuid::parse_str("8f3a2b1c4d5e6f708192a3b4c5d6e7f8").unwrap();
        let hex = "8f3a2b1c4d5e6f708192a3b4c5d6e7f8";
        assert_eq!(Value::Ref(u).convert_to(T::String), (Value::String(hex.into()), false));
        assert_eq!(Value::RefBase(u).convert_to(T::String), (Value::String(hex.into()), false));
        assert_eq!(
            Value::TreeRef { parent: Some(u), name: "félins".into() }.convert_to(T::String),
            (Value::String(format!("{hex}/félins")), false)
        );
        assert_eq!(
            Value::TreeRef { parent: None, name: "tags".into() }.convert_to(T::String),
            (Value::String("/tags".into()), false)
        );
        assert_eq!(
            Value::ExternalRef { repo: u, metarecord: u }.convert_to(T::String),
            (Value::String(format!("{hex}:{hex}")), false)
        );
    }

    #[test]
    fn test_convert_string_to_reference_parses_or_falls_back() {
        use FieldType as T;
        let u = Uuid::parse_str("8f3a2b1c4d5e6f708192a3b4c5d6e7f8").unwrap();
        let hex = "8f3a2b1c4d5e6f708192a3b4c5d6e7f8";
        // String → Ref / RefBase: a valid hex uuid parses; junk → Nothing.
        assert_eq!(Value::String(hex.into()).convert_to(T::Ref), (Value::Ref(u), false));
        assert_eq!(Value::String(hex.into()).convert_to(T::RefBase), (Value::RefBase(u), false));
        assert_eq!(Value::String("nope".into()).convert_to(T::Ref), (Value::Nothing, true));
        // String → TreeRef: root and parented grammar.
        assert_eq!(
            Value::String("/tags".into()).convert_to(T::TreeRef),
            (Value::TreeRef { parent: None, name: "tags".into() }, false)
        );
        assert_eq!(
            Value::String(format!("{hex}/félins")).convert_to(T::TreeRef),
            (Value::TreeRef { parent: Some(u), name: "félins".into() }, false)
        );
        // A name with an interior slash or no slash at all is not a TreeRef.
        assert_eq!(Value::String("/a/b".into()).convert_to(T::TreeRef), (Value::Nothing, true));
        assert_eq!(Value::String("plain".into()).convert_to(T::TreeRef), (Value::Nothing, true));
        // String → ExternalRef: "repo:metarecord".
        assert_eq!(
            Value::String(format!("{hex}:{hex}")).convert_to(T::ExternalRef),
            (Value::ExternalRef { repo: u, metarecord: u }, false)
        );
        assert_eq!(Value::String("nope".into()).convert_to(T::ExternalRef), (Value::Nothing, true));
    }

    #[test]
    fn test_convert_reference_identity_and_cross_fallback() {
        use FieldType as T;
        let u = Uuid::parse_str("8f3a2b1c4d5e6f708192a3b4c5d6e7f8").unwrap();
        // Same type → unchanged (no fallback).
        assert_eq!(Value::Ref(u).convert_to(T::Ref), (Value::Ref(u), false));
        // A reference to a *different* reference type, or to a non-String scalar,
        // has no coercion → Nothing sentinel (findable via IS ABSENT).
        assert_eq!(Value::Ref(u).convert_to(T::RefBase), (Value::Nothing, true));
        assert_eq!(Value::Ref(u).convert_to(T::Int), (Value::Int(0), true));
        assert_eq!(
            Value::TreeRef { parent: None, name: "x".into() }.convert_to(T::Ref),
            (Value::Nothing, true)
        );
        // A non-String scalar to a reference type: no coercion.
        assert_eq!(Value::Int(5).convert_to(T::Ref), (Value::Nothing, true));
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
        let ms = crate::date::iso_to_ms("2024-03-15T10:30:00Z").unwrap();
        let json = serde_json::to_string(&Value::DateTime(ms)).unwrap();
        assert_eq!(json, r#"{"type":"datetime","value":"2024-03-15T10:30:00Z"}"#);
    }

    #[test]
    fn test_value_datetime_deserializes_iso_to_ms() {
        let v: Value =
            serde_json::from_str(r#"{"type":"datetime","value":"2024-03-15T10:30:00Z"}"#).unwrap();
        assert_eq!(v, Value::DateTime(crate::date::iso_to_ms("2024-03-15T10:30:00Z").unwrap()));
    }

    #[test]
    fn test_value_datetime_rejects_invalid_iso() {
        let r: Result<Value, _> =
            serde_json::from_str(r#"{"type":"datetime","value":"not-a-date"}"#);
        assert!(r.is_err(), "invalid ISO-8601 datetime must be rejected");
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
            Value::DateTime(crate::date::iso_to_ms("2024-03-15T10:30:00Z").unwrap()),
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
