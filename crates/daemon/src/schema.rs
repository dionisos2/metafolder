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
    "nothing", "string", "int", "float", "bool", "datetime", "ref", "tree_ref", "refbase",
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
                if let Some(bad) = rows.iter().find(|r| {
                    r.value != Value::Nothing && value_type_name(&r.value) != expected
                }) {
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

/// Builds the 400 response carrying the violations array.
pub fn violation_error(violations: Vec<Violation>) -> ApiError {
    let serialized =
        violations.iter().map(|v| serde_json::to_value(v).expect("violation")).collect();
    ApiError::bad_request("schema constraint violation").with_violations(serialized)
}
