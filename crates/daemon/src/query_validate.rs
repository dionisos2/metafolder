//! Query rejections that depend on the *stored type* of a field.
//!
//! The engine-independent checks live in [`crate::query_exec::validate_query`]:
//! they read the IR alone. These ones need to know what a field holds — whether
//! it is a `tree_ref` forest — which is why they used to be made by the SQL
//! compiler, the only place that asked the database. It is not the only place
//! that knows any more: the bitmap index carries the same map, so the rejection
//! is made once, before any engine runs, and no longer depends on which one
//! would have run (spec-query "Field aspects", spec-indexing "No operand runs in
//! SQL").
//!
//! `type_of` answers "what does this field hold", as one of the `value_type`
//! spellings (`"tree_ref"`, `"string"`, …), or `None` for a field with no data —
//! which stays vacuously valid, as it always was: the query simply matches
//! nothing, like every other predicate on an empty field.

use metafolder_core::metarecord::Value;
use metafolder_core::query::{Aspect, FollowTarget, OsmMode, Query};

use crate::error::ApiError;

/// How a predicate reads the component its aspect names. The tree-only aspects
/// and a bare `tree_ref` accept only equality: a `(parent, name)` couple and a
/// uuid are neither ordered nor regex-matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadKind {
    Equality,
    Ordered,
    Regex,
}

impl ReadKind {
    fn describe(self) -> &'static str {
        match self {
            ReadKind::Equality => "equality",
            ReadKind::Ordered => "ordered comparison",
            ReadKind::Regex => "MATCHES",
        }
    }
}

fn aspect_name(aspect: Aspect) -> &'static str {
    match aspect {
        Aspect::Raw => "raw",
        Aspect::Value => "value",
        Aspect::Parent => "parent",
        Aspect::Path => "path",
    }
}

/// Rejects every shape whose validity depends on the field's type, anywhere in
/// `q`. Runs before any engine, so the answer is the same whichever one would
/// have served the query.
pub fn validate_query_types(
    q: &Query,
    type_of: &dyn Fn(&str) -> Option<String>,
) -> Result<(), ApiError> {
    match q {
        Query::IsPresent { field, aspect } | Query::IsAbsent { field, aspect } => {
            check_aspect(field, *aspect, ReadKind::Equality, type_of)
        }
        Query::Eq { field, value, aspect } | Query::Neq { field, value, aspect } => {
            check_aspect(field, *aspect, ReadKind::Equality, type_of)?;
            check_operand(field, *aspect, value)
        }
        Query::Lt { field, value, aspect }
        | Query::Lte { field, value, aspect }
        | Query::Gt { field, value, aspect }
        | Query::Gte { field, value, aspect } => {
            check_aspect(field, *aspect, ReadKind::Ordered, type_of)?;
            check_operand(field, *aspect, value)
        }
        Query::Matches { field, aspect, .. } => {
            check_aspect(field, *aspect, ReadKind::Regex, type_of)
        }
        // `osm` path mode reads assembled paths, so it is `tree_ref`-only — and
        // the hint names the operator that does what the user meant.
        Query::Osm { field, mode: OsmMode::Path, .. } => match other_type(field, type_of) {
            Some(other) => Err(ApiError::bad_request(format!(
                "osm path mode requires a tree_ref field, but '{field}' holds {other} values; \
                 use osmd for direct string matching"
            ))),
            None => Ok(()),
        },
        Query::Osm { .. } | Query::IsUnknown { .. } | Query::UuidIn { .. } => Ok(()),

        Query::And { operands } | Query::Or { operands } => {
            operands.iter().try_for_each(|o| validate_query_types(o, type_of))
        }
        Query::Not { operand } => validate_query_types(operand, type_of),
        Query::SameAs { target, .. } => validate_query_types(target, type_of),
        Query::Follows { target, .. } | Query::FollowsTransitive { target, .. } => match target {
            FollowTarget::Condition(c) => validate_query_types(c, type_of),
            FollowTarget::Path(_) => Ok(()),
        },
    }
}

/// The type a field holds when it is *not* a `tree_ref` forest — `None` when it
/// is one, or when the field has no data at all.
fn other_type(field: &str, type_of: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    match type_of(field) {
        Some(t) if t != "tree_ref" => Some(t),
        _ => None,
    }
}

fn check_aspect(
    field: &str,
    aspect: Aspect,
    kind: ReadKind,
    type_of: &dyn Fn(&str) -> Option<String>,
) -> Result<(), ApiError> {
    let is_tree = type_of(field).as_deref() == Some("tree_ref");
    match aspect {
        // The tree-only aspects on a field that holds something else. A field
        // with no data at all is vacuously fine.
        Aspect::Parent | Aspect::Path if !is_tree => match other_type(field, type_of) {
            Some(other) => Err(ApiError::bad_request(format!(
                "the ':{}' aspect needs a tree_ref field, but '{field}' holds {other} values",
                aspect_name(aspect)
            ))),
            None => Ok(()),
        },
        // `parent` reads the parent's uuid: a regex over it, or an ordering of
        // it, has no meaning. Refusing here is what keeps either from being
        // answered silently — as `value_name` for the regex, as plain equality
        // for an ordered operator.
        Aspect::Parent if kind != ReadKind::Equality => Err(ApiError::bad_request(format!(
            "{} on the ':parent' aspect of '{field}' reads a uuid: use ':value' for \
             the name component, ':path' for the assembled path",
            kind.describe()
        ))),
        // Ordering or regex-matching a `(parent, name)` couple means nothing,
        // and silently reading the leaf name instead is the kind of implicit
        // rule the aspect vocabulary exists to remove.
        Aspect::Raw if is_tree && kind != ReadKind::Equality => {
            Err(ApiError::bad_request(format!(
                "{} on the tree_ref field '{field}' needs an explicit aspect: \
                 ':value' reads the name component, ':path' the assembled path",
                kind.describe()
            )))
        }
        _ => Ok(()),
    }
}

/// The `:path` aspect assembles a string, so it compares against one.
fn check_operand(field: &str, aspect: Aspect, value: &Value) -> Result<(), ApiError> {
    if aspect == Aspect::Path && !matches!(value, Value::String(_)) {
        let _ = field;
        return Err(ApiError::bad_request(format!(
            "the ':path' aspect compares against a string, got {}",
            crate::db::encode_value(value).value_type
        )));
    }
    Ok(())
}
