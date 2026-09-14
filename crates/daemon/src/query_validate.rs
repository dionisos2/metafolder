//! Query rejections that depend on the *stored type* of a field.
//!
//! The engine-independent checks live next door in [`validate_query`]:
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
            value.type_str()
        )));
    }
    Ok(())
}

/// Upper bound on the number of nodes in a single query. A safety valve
/// against a query that is cheap to send but expensive to *compile* (a wide
/// `And`/`Or`, deep nesting): it would otherwise build a giant CTE chain and
/// tie up a blocking thread before any row is read. Generous on purpose —
/// realistic hand- or UI-built queries are well under it; a membership filter
/// over a very large value list (an `Or` of many `Eq`) should be decomposed
/// (and a future native `In` operator would make it O(1) nodes — see
/// docs/review-followups.md).
pub const MAX_QUERY_NODES: usize = 2000;

/// Maximum number of operands in a single `And`/`Or`. Each operand becomes one
/// term of a SQLite compound `SELECT` (`UNION`/`INTERSECT`), bounded by
/// `SQLITE_MAX_COMPOUND_SELECT` (default 500); beyond it SQLite fails the whole
/// statement with an opaque "too many terms in compound SELECT" error, so we
/// reject early with a clear message. (Nest or decompose, or use a future
/// native `In` operator — see docs/review-followups.md §8.)
pub const MAX_COMBINATOR_OPERANDS: usize = 500;

/// Total number of nodes in a query tree, counting boolean operands and follow
/// sub-conditions. Recursion is bounded: the JSON deserializer caps query
/// nesting depth, so a parsed `Query` is shallow enough to walk safely.
fn node_count(q: &Query) -> usize {
    let children: usize = match q {
        Query::And { operands } | Query::Or { operands } => operands.iter().map(node_count).sum(),
        Query::Not { operand } => node_count(operand),
        Query::Follows { target, .. } | Query::FollowsTransitive { target, .. } => match target {
            FollowTarget::Condition(c) => node_count(c),
            FollowTarget::Path(_) => 0,
        },
        Query::SameAs { target, .. } => node_count(target),
        _ => 0, // leaf predicates
    };
    1 + children
}

/// The widest single `And`/`Or` anywhere in the tree.
fn widest_combinator(q: &Query) -> usize {
    let (here, children): (usize, Vec<&Query>) = match q {
        Query::And { operands } | Query::Or { operands } => {
            (operands.len(), operands.iter().collect())
        }
        Query::Not { operand } => (0, vec![operand]),
        Query::Follows { target, .. } | Query::FollowsTransitive { target, .. } => match target {
            FollowTarget::Condition(c) => (0, vec![c.as_ref()]),
            FollowTarget::Path(_) => (0, Vec::new()),
        },
        Query::SameAs { target, .. } => (0, vec![target.as_ref()]),
        _ => (0, Vec::new()),
    };
    children.into_iter().map(widest_combinator).fold(here, usize::max)
}

/// The message [`MAX_COMBINATOR_OPERANDS`] is rejected with, worded once so the
/// upfront check and the compiler's own guard say the same thing.
pub fn too_wide_message(got: usize) -> String {
    format!(
        "a single 'and'/'or' may have at most {MAX_COMBINATOR_OPERANDS} operands \
         (got {got}); nest or decompose it"
    )
}

/// Rejects an over-large query before compiling it (spec-query "Limits").
///
/// Both limits are checked here, and this must run *before the engine is
/// chosen*: whether a query is too large is a property of the query, not of
/// which engine ends up serving it. The width limit used to live only inside
/// the SQL compiler, so a wide `or` of index-servable leaves was accepted while
/// the same `or` with one `matches` leaf — which forces the SQL fallback — was
/// rejected. A client cannot see that routing decision, so it saw the limit
/// flicker on and off.
pub fn check_query_size(q: &Query) -> Result<(), ApiError> {
    let n = node_count(q);
    if n > MAX_QUERY_NODES {
        return Err(ApiError::bad_request(format!(
            "query too large ({n} nodes, maximum {MAX_QUERY_NODES}); decompose it into smaller queries"
        )));
    }
    let widest = widest_combinator(q);
    if widest > MAX_COMBINATOR_OPERANDS {
        return Err(ApiError::bad_request(too_wide_message(widest)));
    }
    Ok(())
}

/// Validates a query's comparison nodes *upfront* — independent of which engine
/// (bitmap index or SQL) runs it — and rejects the ones with no well-defined,
/// useful meaning (spec-query "Comparison validity"):
///
/// - a comparison against `Nothing` (use `is_absent` / `is_unknown` instead);
/// - an *ordered* comparison (`<` `<=` `>` `>=`) on a value type that has no
///   meaningful order: `bool` and the reference types. Equality (`eq`/`neq`)
///   stays allowed on them, and ordered comparison stays allowed on strings,
///   numbers and datetimes.
///
/// This is the single source of truth: the SQL engine's per-row checks and the
/// index's `Unsupported` branches for these shapes are now defensive backstops.
/// Callers run this before touching either engine so the rejection never has to
/// emerge from an engine-selection fallback.
pub fn validate_query(q: &Query) -> Result<(), ApiError> {
    match q {
        Query::Eq { value, .. } | Query::Neq { value, .. } => validate_comparison(value, false),
        Query::Lt { value, .. }
        | Query::Lte { value, .. }
        | Query::Gt { value, .. }
        | Query::Gte { value, .. } => validate_comparison(value, true),
        Query::And { operands } | Query::Or { operands } => {
            // An empty combinator has no meaning to give — neither "everything"
            // nor "nothing" is more right — and it is a property of the IR, so
            // it is refused here rather than by whichever engine noticed first.
            if operands.is_empty() {
                return Err(ApiError::bad_request("'and'/'or' need at least one operand"));
            }
            operands.iter().try_for_each(validate_query)
        }
        Query::Not { operand } => validate_query(operand),
        Query::Follows { target, .. } | Query::FollowsTransitive { target, .. } => match target {
            FollowTarget::Condition(c) => validate_query(c),
            FollowTarget::Path(_) => Ok(()),
        },
        Query::SameAs { target, .. } => validate_query(target),
        // A pattern that does not compile is a property of the IR, not of an
        // engine: reject it here, so no engine has to be the one that notices.
        Query::Matches { pattern, .. } => crate::regexp::compile(pattern)
            .map(|_| ())
            .map_err(|e| ApiError::bad_request(format!("invalid regex pattern: {e}"))),
        _ => Ok(()),
    }
}

fn validate_comparison(value: &Value, ordered: bool) -> Result<(), ApiError> {
    match value {
        Value::Nothing => Err(ApiError::bad_request(
            "comparisons with 'nothing' are not allowed; use is_absent / is_unknown",
        )),
        Value::Bool(_)
        | Value::Ref(_)
        | Value::RefBase(_)
        | Value::TreeRef { .. }
        | Value::ExternalRef { .. }
            if ordered =>
        {
            Err(ApiError::bad_request(format!(
                "ordered comparison is not supported on {} values",
                value.type_str()
            )))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_node_count_and_size_limit() {
        let leaf = || Query::IsPresent { field: "x".into(), aspect: Aspect::Raw };
        assert_eq!(node_count(&leaf()), 1);

        // 1 (Or) + 5 leaves; nesting and follow conditions also count.
        let nested = Query::And {
            operands: vec![
                leaf(),
                Query::Not { operand: Box::new(leaf()) },
                Query::FollowsTransitive {
                    field: "mfr_path".into(),
                    target: FollowTarget::Condition(Box::new(leaf())),
                    inclusive: false,
                },
            ],
        };
        // And + leaf + (Not + leaf) + (FollowsTransitive + leaf) = 6
        assert_eq!(node_count(&nested), 6);

        // At the node limit passes; one over is rejected. Nested, because the
        // two limits are independent: a flat `Or` of 1999 operands is under the
        // node limit but far over the per-combinator one, so it could not
        // exercise the node limit at all.
        let chunk = |n: usize| Query::Or { operands: (0..n).map(|_| leaf()).collect() };
        let at_limit = Query::Or { operands: vec![chunk(499), chunk(499), chunk(499), chunk(498)] };
        assert_eq!(node_count(&at_limit), MAX_QUERY_NODES);
        assert!(check_query_size(&at_limit).is_ok());

        let over = Query::Or { operands: vec![chunk(499), chunk(499), chunk(499), chunk(499)] };
        assert_eq!(node_count(&over), MAX_QUERY_NODES + 1);
        let err = check_query_size(&over).unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);
        assert!(err.message.contains("too large"), "unexpected error: {}", err.message);
    }

    #[test]
    fn query_size_check_also_bounds_combinator_width() {
        // The width limit is checked upfront, next to the node limit, so it
        // holds whichever engine ends up serving the query — it used to live
        // only inside the SQL compiler, where the index path never reached it.
        let leaf = || Query::IsPresent { field: "x".into(), aspect: Aspect::Raw };
        let wide = |n: usize| Query::Or { operands: (0..n).map(|_| leaf()).collect() };

        assert!(check_query_size(&wide(MAX_COMBINATOR_OPERANDS)).is_ok());
        let err = check_query_size(&wide(MAX_COMBINATOR_OPERANDS + 1)).unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);
        assert!(err.message.contains("at most"), "unexpected error: {}", err.message);

        // Found however deeply it is buried, not just at the root.
        let buried = Query::Not {
            operand: Box::new(Query::And {
                operands: vec![leaf(), wide(MAX_COMBINATOR_OPERANDS + 1)],
            }),
        };
        assert!(check_query_size(&buried).is_err(), "a nested wide combinator slipped through");
    }
}
