//! The forest as a query provider (spec-indexing "No operand runs in SQL").
//!
//! The `:path` aspect reads a component no bitmap holds — the path assembled
//! from the forest root — so the bitmap index declines it. It does not follow
//! that SQL must run the query: the paths live in the resident tree cache, and
//! SQLite only ever received the *result* of walking it, as a `VALUES` list.
//!
//! This module cuts out that detour. Each `:path` leaf is resolved against the
//! forest and rewritten into the `uuid_in` set it matches, which the index then
//! combines with every other operand like any other bitmap.

use metafolder_core::metarecord::Value;
use metafolder_core::query::{Aspect, FollowTarget, Query};
use uuid::Uuid;

use crate::error::ApiError;
use crate::tree_cache::TreeCache;

/// Rewrites every `:path` leaf of `q` into the `uuid_in` set it matches.
///
/// A leaf is left untouched — and the query then takes its usual course,
/// index-declined and SQL-served — whenever the forest cannot answer
/// authoritatively: an incomplete cache, a field with no forest (where `:path`
/// is a `400` the SQL engine raises), an operand of the wrong type, or a
/// pattern that does not compile (a `400` as well).
pub fn resolve_path_leaves(cache: &TreeCache, q: &Query) -> Result<Query, ApiError> {
    if let Some(matched) = path_leaf_matches(cache, q)? {
        return Ok(Query::UuidIn { uuids: matched });
    }
    Ok(match q {
        Query::And { operands } => Query::And { operands: rewrite_all(cache, operands)? },
        Query::Or { operands } => Query::Or { operands: rewrite_all(cache, operands)? },
        Query::Not { operand } => {
            Query::Not { operand: Box::new(resolve_path_leaves(cache, operand)?) }
        }
        Query::SameAs { field, target } => Query::SameAs {
            field: field.clone(),
            target: Box::new(resolve_path_leaves(cache, target)?),
        },
        Query::Follows { field, target } => {
            Query::Follows { field: field.clone(), target: rewrite_target(cache, target)? }
        }
        Query::FollowsTransitive { field, target, inclusive } => Query::FollowsTransitive {
            field: field.clone(),
            target: rewrite_target(cache, target)?,
            inclusive: *inclusive,
        },
        other => other.clone(),
    })
}

fn rewrite_all(cache: &TreeCache, operands: &[Query]) -> Result<Vec<Query>, ApiError> {
    operands.iter().map(|o| resolve_path_leaves(cache, o)).collect()
}

fn rewrite_target(cache: &TreeCache, target: &FollowTarget) -> Result<FollowTarget, ApiError> {
    Ok(match target {
        FollowTarget::Condition(c) => {
            FollowTarget::Condition(Box::new(resolve_path_leaves(cache, c)?))
        }
        FollowTarget::Path(p) => FollowTarget::Path(p.clone()),
    })
}

/// The uuids a single `:path` leaf matches, or `None` when `q` is not such a
/// leaf or the forest cannot answer it.
fn path_leaf_matches(cache: &TreeCache, q: &Query) -> Result<Option<Vec<Uuid>>, ApiError> {
    let Some((field, pred)) = path_predicate(q) else { return Ok(None) };
    cache.path_matches(field, pred.as_ref()).map_err(ApiError::from)
}

/// A `:path` leaf's field and the test its assembled path must pass.
type PathPredicate<'a> = (&'a str, Box<dyn Fn(&str) -> bool + 'a>);

/// The `(field, predicate on the assembled path)` a `:path` leaf reads, mirror
/// for mirror of what the SQL compiler builds for the same node.
fn path_predicate(q: &Query) -> Option<PathPredicate<'_>> {
    use metafolder_core::query::Query as Q;
    let (field, value, op): (&String, &Value, Op) = match q {
        Q::Eq { field, value, aspect: Aspect::Path } => (field, value, Op::Eq),
        Q::Neq { field, value, aspect: Aspect::Path } => (field, value, Op::Neq),
        Q::Lt { field, value, aspect: Aspect::Path } => (field, value, Op::Lt),
        Q::Lte { field, value, aspect: Aspect::Path } => (field, value, Op::Lte),
        Q::Gt { field, value, aspect: Aspect::Path } => (field, value, Op::Gt),
        Q::Gte { field, value, aspect: Aspect::Path } => (field, value, Op::Gte),
        Q::Matches { field, pattern, aspect: Aspect::Path } => {
            // An invalid pattern is a 400, raised where every other one is.
            let re = crate::regexp::compile(pattern).ok()?;
            return Some((field, Box::new(move |path: &str| re.is_match(path))));
        }
        _ => return None,
    };
    // `:path` compares against a string; any other operand is a 400.
    let Value::String(operand) = value else { return None };
    let operand = operand.clone();
    Some((
        field,
        Box::new(move |path: &str| match op {
            Op::Eq => path == operand,
            // Not the complement: `!=` asks for one *differing* path, so a node
            // holding both a matching and a differing one satisfies both.
            Op::Neq => path != operand,
            Op::Lt => path < operand.as_str(),
            Op::Lte => path <= operand.as_str(),
            Op::Gt => path > operand.as_str(),
            Op::Gte => path >= operand.as_str(),
        }),
    ))
}

#[derive(Clone, Copy)]
enum Op {
    Eq,
    Neq,
    Lt,
    Lte,
    Gt,
    Gte,
}
