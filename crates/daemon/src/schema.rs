//! User schema system (spec-schema): metarecord types declared via `mf_schema`,
//! constraints loaded from an external JSON file, delta validation of user
//! writes, and the check endpoint.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use metafolder_core::metarecord::Value;

use crate::config::RepoConfig;
use crate::db;
use crate::error::ApiError;

const VALUE_TYPES: &[&str] = &[
    "nothing",
    "string",
    "int",
    "float",
    "bool",
    "datetime",
    "ref",
    "tree_ref",
    "refbase",
    "externalref",
];

// ── File format ───────────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSchema {
    #[allow(dead_code)]
    version: u32,
    groups: Vec<RawGroup>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGroup {
    targets: RawTargets,
    constraints: Vec<RawConstraint>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawTargets {
    Star(String),
    List(Vec<String>),
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConstraint {
    field: String,
    #[serde(rename = "type")]
    value_type: Option<String>,
    #[serde(default)]
    min: u64,
    #[serde(default)]
    max: Option<u64>,
    #[serde(default)]
    #[allow(dead_code)]
    description: Option<String>,
    /// Optional default value for client templates: a bare JSON value
    /// interpreted via this constraint's `type`. Never read from this parsed
    /// struct (hence `dead_code`) and not kept in the compiled form — it exists
    /// only so `deny_unknown_fields` accepts it in the file. Clients read it
    /// from the *raw* schema JSON that `GET /schema` serves verbatim, not from
    /// here.
    #[serde(default)]
    #[allow(dead_code)]
    default: Option<serde_json::Value>,
}

/// A parsed and validated schema, indexed by field name so that validation
/// adds no database query beyond reading the metarecord itself.
#[derive(Debug)]
pub struct CompiledSchema {
    /// The schema as found in the file (returned by `GET /schema`).
    raw: serde_json::Value,
    by_field: HashMap<String, Vec<IndexedConstraint>>,
}

#[derive(Debug)]
struct IndexedConstraint {
    /// None for global (`"*"`) groups; otherwise the target type names.
    targets: Option<Vec<String>>,
    value_type: Option<String>,
    min: u64,
    max: Option<u64>,
}

impl CompiledSchema {
    pub fn raw(&self) -> &serde_json::Value {
        &self.raw
    }

    pub fn empty_raw() -> serde_json::Value {
        serde_json::json!({"version": 1, "groups": []})
    }

    /// Every field name carrying at least one constraint (for full checks).
    pub fn constrained_fields(&self) -> Vec<String> {
        self.by_field.keys().cloned().collect()
    }

    /// Field names the schema declares with an explicit value type (the first
    /// such constraint per field), as `(field, value_type)`. Feeds the field
    /// catalog so a schema-declared field (e.g. `path: tree_ref`) is listed and
    /// given priority even before any data carries it. `targets` are ignored:
    /// the declared type is a repo-wide property of the field name.
    pub fn declared_types(&self) -> Vec<(String, String)> {
        self.by_field
            .iter()
            .filter_map(|(field, constraints)| {
                constraints.iter().find_map(|c| c.value_type.clone()).map(|t| (field.clone(), t))
            })
            .collect()
    }
}

/// Merges the data-derived field catalog (the index's `field_catalog`) with the
/// schema's declared field types. The schema takes priority on a type conflict
/// (a field whose existing data type differs from its declared type — possible
/// for data predating the constraint, which the daemon never blocks; spec-schema
/// "delta validation"), and schema-only fields (declared but not yet present in
/// data) are added. The `type_filter` is applied *after* the merge, so a
/// schema-only field of that type is included. Result ordered by name.
pub fn merge_field_catalog(
    data: Vec<(String, String)>,
    schema_decls: Vec<(String, String)>,
    type_filter: Option<&str>,
) -> Vec<(String, String)> {
    let mut map: std::collections::BTreeMap<String, String> = data.into_iter().collect();
    for (field, ty) in schema_decls {
        map.insert(field, ty); // schema wins on conflict
    }
    map.into_iter().filter(|(_, ty)| type_filter.is_none_or(|w| w == ty)).collect()
}

/// Parses and validates a schema document. Error messages identify the
/// offending constraint (spec-schema "Schema file location and loading").
pub fn parse(content: &str) -> Result<CompiledSchema, String> {
    let raw_value: serde_json::Value =
        serde_json::from_str(content).map_err(|e| format!("invalid schema file: {e}"))?;
    let schema: RawSchema = serde_json::from_value(raw_value.clone())
        .map_err(|e| format!("invalid schema file: {e}"))?;

    let mut by_field: HashMap<String, Vec<IndexedConstraint>> = HashMap::new();
    for (gi, group) in schema.groups.iter().enumerate() {
        let targets = match &group.targets {
            RawTargets::Star(s) if s == "*" => None,
            RawTargets::Star(s) => {
                return Err(format!(
                    "group {gi}: invalid targets '{s}' (expected \"*\" or a non-empty list)"
                ))
            }
            RawTargets::List(list) if list.is_empty() => {
                return Err(format!("group {gi}: targets must not be an empty list"));
            }
            RawTargets::List(list) => Some(list.clone()),
        };
        for constraint in &group.constraints {
            let at = format!("group {gi}, field '{}'", constraint.field);
            if constraint.field.starts_with("mfr_") || constraint.field.starts_with("mf_") {
                return Err(format!("{at}: reserved fields cannot be constrained"));
            }
            if let Some(t) = &constraint.value_type {
                if !VALUE_TYPES.contains(&t.as_str()) {
                    return Err(format!("{at}: unknown value type '{t}'"));
                }
            }
            // A default is a bare JSON value interpreted via the declared type;
            // the type is therefore required and the value's kind must match it.
            if let Some(default) = &constraint.default {
                match &constraint.value_type {
                    None => return Err(format!("{at}: a default value requires an explicit type")),
                    Some(t) => {
                        if let Err(msg) = check_default_kind(t, default) {
                            return Err(format!("{at}: {msg}"));
                        }
                    }
                }
            }
            if let Some(max) = constraint.max {
                if constraint.min > max {
                    return Err(format!(
                        "{at}: min ({}) is greater than max ({max})",
                        constraint.min
                    ));
                }
            }
            by_field.entry(constraint.field.clone()).or_default().push(IndexedConstraint {
                targets: targets.clone(),
                value_type: constraint.value_type.clone(),
                min: constraint.min,
                max: constraint.max,
            });
        }
    }
    Ok(CompiledSchema { raw: raw_value, by_field })
}

/// Loads the repository's schema file: the `schema` config key (relative to
/// `.metafolder/` or absolute), defaulting to `.metafolder/schema.json`.
/// A missing default file means "no schema"; a missing explicit file or an
/// invalid document is an error (the load must fail with 400).
pub fn load_for_repo(
    metafolder_dir: &Path,
    config: &RepoConfig,
) -> Result<Option<CompiledSchema>, String> {
    let (path, explicit) = match &config.schema {
        Some(p) if p.is_absolute() => (p.clone(), true),
        Some(p) => (metafolder_dir.join(p), true),
        None => (metafolder_dir.join("schema.json"), false),
    };
    let content = match std::fs::read_to_string(&path) {
        Ok(content) => content,
        Err(_) if !explicit => return Ok(None),
        Err(e) => return Err(format!("cannot read schema file {path:?}: {e}")),
    };
    parse(&content).map(Some)
}

// ── Validation ────────────────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct Violation {
    #[serde(with = "metafolder_core::metarecord::hex_uuid")]
    pub metarecord_uuid: Uuid,
    /// The type name that activated the constraint; null for global ones.
    #[serde(rename = "type")]
    pub origin: Option<String>,
    pub field: String,
    /// `type`, `min_cardinality` or `max_cardinality`.
    pub kind: &'static str,
    pub message: String,
}

/// Checks that a bare `default` JSON value matches a constraint's declared
/// type (the value's JSON kind, not a full `{type, value}` form).
fn check_default_kind(value_type: &str, v: &serde_json::Value) -> Result<(), String> {
    let ok = match value_type {
        "string" | "datetime" | "ref" | "refbase" => v.is_string(),
        "int" => v.is_i64() || v.is_u64(),
        "float" => v.is_number(),
        "bool" => v.is_boolean(),
        "tree_ref" => v.get("name").is_some_and(serde_json::Value::is_string),
        "externalref" => v.is_object(),
        // `nothing` is an absence, not a value: omit `default` to template it.
        _ => false,
    };
    if ok {
        Ok(())
    } else {
        Err(format!("default value {v} is not a valid {value_type}"))
    }
}

fn value_type_name(v: &Value) -> &'static str {
    match v {
        Value::Nothing => "nothing",
        Value::String(_) => "string",
        Value::Int(_) => "int",
        Value::Float(_) => "float",
        Value::Bool(_) => "bool",
        Value::DateTime(_) => "datetime",
        Value::Ref(_) => "ref",
        Value::TreeRef { .. } => "tree_ref",
        Value::RefBase(_) => "refbase",
        Value::ExternalRef { .. } => "externalref",
    }
}

/// Evaluates the applicable constraints for the given fields of one metarecord,
/// against its current state (delta validation: callers pass the touched
/// field names after applying the write).
pub fn validate_entry_fields(
    schema: &CompiledSchema,
    conn: &Connection,
    uuid: Uuid,
    touched: &[String],
) -> Result<Vec<Violation>> {
    // The metarecord's declared types (its mf_schema values).
    let types: Vec<String> = db::get_field_rows_named(conn, uuid, "mf_schema")?
        .into_iter()
        .filter_map(|r| match r.value {
            Value::String(s) => Some(s),
            _ => None,
        })
        .collect();

    let mut violations = Vec::new();
    for field in touched {
        // Reserved fields are covered by structural checks, not the schema.
        if field.starts_with("mfr_") || field.starts_with("mf_") {
            continue;
        }
        let Some(constraints) = schema.by_field.get(field) else {
            continue;
        };
        let rows = db::get_field_rows_named(conn, uuid, field)?;
        for constraint in constraints {
            let origin = match &constraint.targets {
                None => None,
                Some(targets) => match targets.iter().find(|t| types.contains(t)) {
                    Some(t) => Some(t.clone()),
                    None => continue, // The entry has none of the target types.
                },
            };
            if let Some(expected) = &constraint.value_type {
                if let Some(bad) = rows
                    .iter()
                    .find(|r| r.value != Value::Nothing && value_type_name(&r.value) != expected)
                {
                    violations.push(Violation {
                        metarecord_uuid: uuid,
                        origin: origin.clone(),
                        field: field.clone(),
                        kind: "type",
                        message: format!(
                            "value of type {} not allowed (expected: {expected})",
                            value_type_name(&bad.value)
                        ),
                    });
                }
            }
            let n = rows.len() as u64;
            if n < constraint.min {
                violations.push(Violation {
                    metarecord_uuid: uuid,
                    origin: origin.clone(),
                    field: field.clone(),
                    kind: "min_cardinality",
                    message: format!("{n} rows, minimum is {}", constraint.min),
                });
            }
            if let Some(max) = constraint.max {
                if n > max {
                    violations.push(Violation {
                        metarecord_uuid: uuid,
                        origin,
                        field: field.clone(),
                        kind: "max_cardinality",
                        message: format!("{n} rows, maximum is {max}"),
                    });
                }
            }
        }
    }
    Ok(violations)
}

/// A superset of the metarecords that could violate any constraint, gathered
/// with a handful of index-served set queries (a small group per constraint)
/// instead of a per-record walk of the whole repository. Running
/// [`validate_entry_fields`] on exactly these reproduces the full-repo check: a
/// metarecord not in this set holds every constrained field within its type and
/// cardinality bounds, so it cannot violate. On a healthy repository the set is
/// nearly empty, so the whole-repo check no longer scans every metarecord.
///
/// `cap` bounds how many rows each query contributes (`Some` for the capped
/// heads-up, `None` for a full audit). Order is unspecified (the caller applies
/// the precise per-record validator anyway). The set is a *superset*: the
/// validator re-applies each constraint's target-type filter, so a candidate
/// that is not actually of a targeted type simply yields no violation.
pub fn violation_candidates(
    schema: &CompiledSchema,
    conn: &Connection,
    cap: Option<usize>,
) -> Result<Vec<Uuid>> {
    let limit = cap.map(|c| c as i64).unwrap_or(i64::MAX);
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    let mut add = |uuids: Vec<Uuid>| {
        for u in uuids {
            if seen.insert(u) {
                out.push(u);
            }
        }
    };
    for (field, constraints) in &schema.by_field {
        for c in constraints {
            // A row of the wrong type trips the `type` constraint.
            if let Some(expected) = &c.value_type {
                add(db::uuids_field_wrong_type(conn, field, expected, limit)?);
            }
            // Too many rows trips `max`.
            if let Some(max) = c.max {
                add(db::uuids_field_count_over(conn, field, max as i64, limit)?);
            }
            // Too few rows trips `min`: present-but-under, plus records that lack
            // the field entirely (restricted to the target population when the
            // constraint is targeted, so unrelated records never become
            // candidates; unrestricted for a global constraint).
            if c.min > 0 {
                add(db::uuids_field_count_under(conn, field, c.min as i64, limit)?);
                match &c.targets {
                    Some(types) => add(db::uuids_typed_missing_field(conn, types, field, limit)?),
                    None => add(db::uuids_missing_field(conn, field, limit)?),
                }
            }
        }
    }
    Ok(out)
}

/// Builds the 400 response carrying the violations array.
pub fn violation_error(violations: Vec<Violation>) -> ApiError {
    let serialized =
        violations.iter().map(|v| serde_json::to_value(v).expect("violation")).collect();
    ApiError::bad_request("schema constraint violation").with_violations(serialized)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pairs(items: &[(&str, &str)]) -> Vec<(String, String)> {
        items.iter().map(|(a, b)| (a.to_string(), b.to_string())).collect()
    }

    #[test]
    fn violation_candidates_gathers_only_records_that_can_violate() {
        use crate::log::Writer;
        use metafolder_core::metarecord::{Field, Value};

        // Global: rating is int, at most one; tag is int. Films must have a name.
        let schema = parse(
            r#"{"version":1,"groups":[
                {"targets":"*","constraints":[
                    {"field":"rating","type":"int","max":1},
                    {"field":"tag","type":"int"}
                ]},
                {"targets":["film"],"constraints":[{"field":"name","type":"string","min":1}]}
            ]}"#,
        )
        .unwrap();

        let mut conn = crate::db::open_in_memory().unwrap();
        crate::db::init_schema(&conn).unwrap();
        let mk = |conn: &mut rusqlite::Connection, fields: Vec<Field>| -> Uuid {
            let mut w = Writer::begin(conn, None).unwrap();
            let m = w.create_metarecord(fields).unwrap();
            w.commit().unwrap();
            m.uuid
        };

        // Clean: satisfies every constraint — not a candidate.
        let clean = mk(&mut conn, vec![Field::new("rating", Value::Int(5))]);
        // Two ratings → max_cardinality candidate.
        let over_max = mk(
            &mut conn,
            vec![Field::new("rating", Value::Int(1)), Field::new("rating", Value::Int(2))],
        );
        // A film with no name → min_cardinality (missing) candidate.
        let film_no_name = mk(
            &mut conn,
            vec![
                Field::new("mf_schema", Value::String("film".into())),
                Field::new("rating", Value::Int(3)),
            ],
        );
        // A film with a name → not a candidate.
        let clean_film = mk(
            &mut conn,
            vec![
                Field::new("mf_schema", Value::String("film".into())),
                Field::new("rating", Value::Int(4)),
                Field::new("name", Value::String("x".into())),
            ],
        );
        // tag declared int, stored string → type candidate.
        let tag_wrong = mk(&mut conn, vec![Field::new("tag", Value::String("hello".into()))]);

        let got: std::collections::HashSet<Uuid> =
            violation_candidates(&schema, &conn, None).unwrap().into_iter().collect();
        let want: std::collections::HashSet<Uuid> =
            [over_max, film_no_name, tag_wrong].into_iter().collect();
        assert_eq!(got, want, "clean={clean} clean_film={clean_film}");
    }

    #[test]
    fn merge_adds_schema_only_fields_and_gives_schema_priority() {
        let data = pairs(&[("a", "string"), ("shared", "string")]);
        let schema = pairs(&[("shared", "int"), ("b", "tree_ref")]);
        // `b` is added (schema-only), `shared` takes the schema type (priority),
        // `a` is kept; ordered by name.
        assert_eq!(
            merge_field_catalog(data, schema, None),
            pairs(&[("a", "string"), ("b", "tree_ref"), ("shared", "int")])
        );
    }

    #[test]
    fn merge_filters_by_type_after_the_merge() {
        // A schema-only field of the wanted type survives the filter; a data
        // field whose type the schema overrode is filtered on the *schema* type.
        let data = pairs(&[("a", "string"), ("shared", "string")]);
        let schema = pairs(&[("shared", "int"), ("b", "int")]);
        assert_eq!(
            merge_field_catalog(data, schema, Some("int")),
            pairs(&[("b", "int"), ("shared", "int")])
        );
        assert_eq!(
            merge_field_catalog(
                pairs(&[("a", "string")]),
                pairs(&[("shared", "int")]),
                Some("string")
            ),
            pairs(&[("a", "string")])
        );
    }

    #[test]
    fn declared_types_picks_the_first_typed_constraint_per_field() {
        let schema = parse(
            &serde_json::json!({
                "version": 1,
                "groups": [
                    {"targets": "*", "constraints": [{"field": "rating", "type": "int"}]},
                    {"targets": ["film"], "constraints": [
                        {"field": "name", "type": "string", "min": 1},
                        {"field": "untyped"}
                    ]}
                ]
            })
            .to_string(),
        )
        .unwrap();
        let mut got = schema.declared_types();
        got.sort();
        // `untyped` (no `type`) contributes nothing.
        assert_eq!(got, pairs(&[("name", "string"), ("rating", "int")]));
    }
}
