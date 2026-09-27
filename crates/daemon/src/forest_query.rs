//! The forest as a query provider (spec-indexing "No operand runs in SQL").
//!
//! The `:path` aspect reads a component no bitmap holds — the path assembled
//! from the forest root — so the bitmap index declines it. It did not follow
//! that SQL had to run the query: the paths live in the resident tree cache,
//! and SQLite only ever received the *result* of walking it, as a `VALUES`
//! list.
//!
//! This module cuts out that detour. Each such leaf — a `:path` predicate, and
//! an order-sensitive `osm` path — is resolved against the forest and rewritten
//! into the `uuid_in` set it matches, which the index then combines with every
//! other operand like any other bitmap. Where no forest is resident (a
//! key-value repository), an `osm` path is instead rewritten into the subtrees
//! of its verified anchors (see [`osm_path_seeded`]), which the index expands
//! from its descendant bitmaps: walking the stored forest read every node.

use std::collections::HashMap;

use metafolder_core::metarecord::Value;
use metafolder_core::query::{osm_advance, Aspect, FollowTarget, OsmMode, OsmProgress, Query};
use uuid::Uuid;

use crate::error::ApiError;
use crate::index::{Eval, QueryRoots};
use crate::log::MAX_TREE_DEPTH;
use crate::store::Rows;
use crate::tree_cache::TreeCache;

/// Rewrites every forest-served leaf of `q` into the `uuid_in` set it matches.
///
/// A leaf is left untouched whenever the forest cannot answer authoritatively:
/// an incomplete cache, an operand of the wrong type, or a pattern that does
/// not compile. Those last two are `400`s raised upstream by
/// `query_validate`, before this runs; an incomplete cache cannot happen on the
/// serving path (a repository serves nothing until it is warm), and the index
/// declining the untouched leaf then reports the daemon bug it is. A field
/// with no forest at all is the one benign case: the leaf matches nothing and
/// the walk says so.
///
/// `names` is the index the query will run on: without a resident forest, it
/// supplies the name candidates an `osm` path is narrowed to. `None` walks.
pub fn resolve_path_leaves(
    cache: &TreeCache,
    store: &dyn Rows,
    names: Option<&Eval<'_>>,
    q: &Query,
) -> Result<Query, ApiError> {
    if let Some(rewritten) = path_leaf_matches(cache, store, names, q)? {
        return Ok(rewritten);
    }
    Ok(match q {
        Query::And { operands } => {
            Query::And { operands: rewrite_all(cache, store, names, operands)? }
        }
        Query::Or { operands } => {
            Query::Or { operands: rewrite_all(cache, store, names, operands)? }
        }
        Query::Not { operand } => {
            Query::Not { operand: Box::new(resolve_path_leaves(cache, store, names, operand)?) }
        }
        Query::SameAs { field, target } => Query::SameAs {
            field: field.clone(),
            target: Box::new(resolve_path_leaves(cache, store, names, target)?),
        },
        Query::Follows { field, target } => Query::Follows {
            field: field.clone(),
            target: rewrite_target(cache, store, names, target)?,
        },
        Query::FollowsTransitive { field, target, inclusive } => Query::FollowsTransitive {
            field: field.clone(),
            target: rewrite_target(cache, store, names, target)?,
            inclusive: *inclusive,
        },
        other => other.clone(),
    })
}

fn rewrite_all(
    cache: &TreeCache,
    store: &dyn Rows,
    names: Option<&Eval<'_>>,
    operands: &[Query],
) -> Result<Vec<Query>, ApiError> {
    operands.iter().map(|o| resolve_path_leaves(cache, store, names, o)).collect()
}

fn rewrite_target(
    cache: &TreeCache,
    store: &dyn Rows,
    names: Option<&Eval<'_>>,
    target: &FollowTarget,
) -> Result<FollowTarget, ApiError> {
    Ok(match target {
        FollowTarget::Condition(c) => {
            FollowTarget::Condition(Box::new(resolve_path_leaves(cache, store, names, c)?))
        }
        FollowTarget::Path(p) => FollowTarget::Path(p.clone()),
    })
}

/// The rewrite of a single forest leaf, or `None` when `q` is not one or the
/// forest cannot answer it.
fn path_leaf_matches(
    cache: &TreeCache,
    store: &dyn Rows,
    names: Option<&Eval<'_>>,
    q: &Query,
) -> Result<Option<Query>, ApiError> {
    // An `osm` path the index cannot serve natively — several terms, or one
    // containing the separator, both order-sensitive.
    if let Query::Osm { field, terms, mode: OsmMode::Path } = q {
        if terms.is_empty() || crate::index::osm_path_indexable(terms).is_some() {
            return Ok(None);
        }
        // The resident forest: one walk carrying the match position down each
        // branch, taking a whole subtree at once when a branch has consumed
        // every term.
        let resident = cache.osm_path_matches(field, terms).map_err(ApiError::from)?;
        let seeded = match (resident, names) {
            (Some(matched), _) => Some(Query::UuidIn { uuids: matched }),
            (None, Some(names)) => osm_path_seeded(store, names, field, terms)?,
            (None, None) => None,
        };
        let mut rewritten = match seeded {
            Some(rewritten) => rewritten,
            None => Query::UuidIn {
                uuids: cache.osm_path_matches_with(store, field, terms).map_err(ApiError::from)?,
            },
        };
        // Sorted for the same reason the `:path` walk sorts: the cursor is
        // bound to a hash of the rewritten query.
        if let Query::UuidIn { uuids } = &mut rewritten {
            uuids.sort_unstable();
        }
        return Ok(Some(rewritten));
    }
    let Some((field, pred)) = path_predicate(q) else { return Ok(None) };
    let matched = cache.path_matches_with(store, field, pred.as_ref()).map_err(ApiError::from)?;
    Ok(Some(Query::UuidIn { uuids: matched }))
}

/// An order-sensitive `osm` path answered from the store without walking the
/// forest (a key-value repository keeps none resident, and the walk reads
/// every node of it).
///
/// Along any path that matches, the shortest matching prefix ends at a node
/// whose own *name* holds the end of the last term: a term free of the
/// separator cannot straddle one, and of a term containing it, what follows
/// its last `/` opens that node's name. So the matches are the subtrees rooted
/// at those "anchor" nodes whose path matches — the candidates come from the
/// index's name scan, each is verified on its assembled paths, and the
/// subtrees are left to the index, as a single-term search leaves them.
///
/// `None` when no anchor narrows the search (a last term ending with the
/// separator), or the index declines the name scan: the caller walks instead.
fn osm_path_seeded(
    store: &dyn Rows,
    names: &Eval<'_>,
    field: &str,
    terms: &[String],
) -> Result<Option<Query>, ApiError> {
    let Some(anchor) = terms.last().and_then(|t| t.rsplit('/').next()).filter(|a| !a.is_empty())
    else {
        return Ok(None);
    };
    // The regex the single-term search seeds with, over the same names.
    let probe = Query::Matches {
        field: field.to_string(),
        pattern: format!("(?i){}", regex::escape(anchor)),
        // On a tree_ref, the `value` aspect is the node's name.
        aspect: Aspect::Value,
    };
    let Ok((candidates, _)) =
        names.evaluate_page_with_roots(&probe, &[], None, None, &QueryRoots::new())
    else {
        return Ok(None);
    };
    let terms_lower: Vec<String> = terms.iter().map(|t| lower(t)).collect();
    let mut paths = AncestorPaths { store, field, known: HashMap::new() };
    // Descendants hang from a node's first position only: an anchor matching
    // through another one matches alone.
    let (mut subtrees, mut alone) = (Vec::new(), Vec::new());
    for uuid in candidates {
        let positions = store.positions(field, uuid).map_err(ApiError::from)?;
        for (i, (parent, name)) in positions.into_iter().enumerate() {
            // A root's path is its bare name; every other node joins with '/'.
            let path = match parent {
                None => lower(&name),
                Some(parent) => match paths.first_path(parent)? {
                    Some(above) => format!("{above}/{}", lower(&name)),
                    None => continue, // a stale position: no path through it
                },
            };
            if osm_advance(&path, &terms_lower, OsmProgress::default()).matched == terms_lower.len()
            {
                if i == 0 {
                    subtrees.push(uuid)
                } else {
                    alone.push(uuid)
                }
                break;
            }
        }
    }
    subtrees.sort_unstable();
    alone.sort_unstable();
    let whole = Query::FollowsTransitive {
        field: field.to_string(),
        target: FollowTarget::Condition(Box::new(Query::UuidIn { uuids: subtrees })),
        inclusive: true,
    };
    Ok(Some(if alone.is_empty() {
        whole
    } else {
        Query::Or { operands: vec![whole, Query::UuidIn { uuids: alone }] }
    }))
}

/// Lower-cases like the resident walk: char by char, so a word-final sigma
/// folds as every other one does.
fn lower(s: &str) -> String {
    s.chars().flat_map(char::to_lowercase).collect()
}

/// The lower-cased path of each node through its first position — the one its
/// descendants hang from, as `TreeCache::path_of` assembles it — remembered,
/// so candidates sharing a folder read its ancestry once.
/// A node's `(parent, name)` in a forest — `None` for a root's parent.
type Position = (Option<Uuid>, String);

struct AncestorPaths<'a> {
    store: &'a dyn Rows,
    field: &'a str,
    known: HashMap<Uuid, Option<String>>,
}

impl AncestorPaths<'_> {
    fn first_path(&mut self, uuid: Uuid) -> Result<Option<String>, ApiError> {
        // Climb to a node already known (or a root), then settle the chain
        // back down.
        let mut chain: Vec<(Uuid, Option<Position>)> = Vec::new();
        let mut cur = uuid;
        let mut above = loop {
            if let Some(known) = self.known.get(&cur) {
                break known.clone();
            }
            if chain.len() >= MAX_TREE_DEPTH {
                return Err(ApiError::internal(format!(
                    "TreeRef chain deeper than {MAX_TREE_DEPTH} for entry {uuid}"
                )));
            }
            let first =
                self.store.positions(self.field, cur).map_err(ApiError::from)?.into_iter().next();
            let parent = first.as_ref().and_then(|(p, _)| *p);
            chain.push((cur, first));
            match parent {
                Some(p) => cur = p,
                None => break None,
            }
        };
        for (node, first) in chain.into_iter().rev() {
            let path = match first {
                None => None,
                Some((None, name)) => Some(lower(&name)),
                Some((Some(_), name)) => above.map(|a| format!("{a}/{}", lower(&name))),
            };
            self.known.insert(node, path.clone());
            above = path;
        }
        Ok(above)
    }
}

/// A `:path` leaf's field and the test its assembled path must pass.
type PathPredicate<'a> = (&'a str, Box<dyn Fn(&str) -> bool + 'a>);

/// The `(field, predicate on the assembled path)` a `:path` leaf reads, mirror
/// for mirror of what the oracle's SQL compiler builds for the same node.
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
