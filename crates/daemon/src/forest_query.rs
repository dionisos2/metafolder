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
//! from its descendant bitmaps, and a `:path` comparison seeks the paths it
//! can match (`PathSeek`): walking the stored forest read every node.

use roaring::RoaringBitmap;
use std::collections::HashMap;

use metafolder_core::metarecord::Value;
use metafolder_core::query::{osm_advance, Aspect, FollowTarget, OsmMode, OsmProgress, Query};
use uuid::Uuid;

use crate::error::ApiError;
use crate::index::{Eval, QueryRoots};
use crate::log::MAX_TREE_DEPTH;
use crate::store::Rows;
use crate::tree_cache::{PathSeek, TreeCache};

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
        let seeded = match names {
            Some(names) => osm_path_seeded(store, names, field, terms)?,
            None => None,
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
    let Some((field, pred, narrow)) = path_predicate(q) else { return Ok(None) };
    // Seek in the store what can match, rather than walk every node of it
    // (spec-storage "The forest").
    let below = |operand: &str, op: Op| -> Box<dyn Fn(&str) -> bool + '_> {
        let operand = operand.to_string();
        Box::new(move |start: &str| match op {
            // Every path below starts with `start`, and is greater than it.
            Op::Lt | Op::Lte => start < operand.as_str(),
            _ => start > operand.as_str() || operand.starts_with(start),
        })
    };
    let seek_all = |seek: PathSeek<'_>| {
        cache.path_matches_seeking(store, field, seek, pred.as_ref()).map_err(ApiError::from)
    };
    let uuids = match &narrow {
        // A path assembled from a name the store does not hold as text is
        // not one a seek can spell: walk.
        Narrow::Exact(p) | Narrow::Differs(p) | Narrow::Prefix(p) | Narrow::Range(_, p)
            if p.contains(char::REPLACEMENT_CHARACTER) =>
        {
            cache.path_matches_with(store, field, pred.as_ref()).map_err(ApiError::from)?
        }
        Narrow::Exact(path) => seek_all(PathSeek::Exact(path))?,
        Narrow::Prefix(prefix) => seek_all(PathSeek::Prefix(prefix))?,
        Narrow::Range(op, operand) => seek_all(PathSeek::Below(below(operand, *op).as_ref()))?,
        // Every node has one path: all but the one at the operand.
        Narrow::Differs(path) => {
            let present = Query::IsPresent { field: field.to_string(), aspect: Aspect::Path };
            let at = cache
                .path_matches_seeking(store, field, PathSeek::Exact(path), &|_| true)
                .map_err(ApiError::from)?;
            if at.is_empty() {
                return Ok(Some(present));
            }
            let not_it = Query::Not { operand: Box::new(Query::UuidIn { uuids: at }) };
            return Ok(Some(Query::And { operands: vec![present, not_it] }));
        }
        Narrow::Walk => {
            cache.path_matches_with(store, field, pred.as_ref()).map_err(ApiError::from)?
        }
    };
    Ok(Some(Query::UuidIn { uuids }))
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
    let pattern = format!("(?i){}", regex::escape(anchor));
    let literals = crate::regexp::required_literals(&pattern);
    let within = anchors_within(names, field, &terms[..terms.len() - 1], &literals);
    // The candidates with their positions, from one read of their rows where
    // the source can; else the names scanned, then the positions read.
    let found = match crate::regexp::compile(&pattern) {
        Ok(re) => {
            names.src.named_positions(field, &|name| re.is_match(name), &literals, within.as_ref())
        }
        Err(_) => None,
    };
    let found = match found {
        Some(found) => found,
        None => {
            let probe = Query::Matches {
                field: field.to_string(),
                pattern,
                // On a tree_ref, the `value` aspect is the node's name.
                aspect: Aspect::Value,
            };
            let Ok((candidates, _)) =
                names.evaluate_page_with_roots(&probe, &[], None, None, &QueryRoots::new())
            else {
                return Ok(None);
            };
            let mut found = Vec::with_capacity(candidates.len());
            for uuid in candidates {
                found.push((uuid, store.positions(field, uuid).map_err(ApiError::from)?));
            }
            found
        }
    };
    let terms_lower: Vec<String> = terms.iter().map(|t| lower(t)).collect();
    let mut paths = AncestorPaths { store, field, known: HashMap::new() };
    let mut subtrees = Vec::new();
    for (uuid, positions) in found {
        // One position per forest (spec-data-model).
        let Some((parent, name)) = positions.into_iter().next() else { continue };
        // A root's path is its bare name; every other node joins with '/'.
        let path = match parent {
            None => lower(&name),
            Some(parent) => match paths.path(parent)? {
                Some(above) => format!("{above}/{}", lower(&name)),
                None => continue, // a stale position: no path through it
            },
        };
        if osm_advance(&path, &terms_lower, OsmProgress::default()).matched == terms_lower.len() {
            subtrees.push(uuid);
        }
    }
    subtrees.sort_unstable();
    Ok(Some(Query::FollowsTransitive {
        field: field.to_string(),
        target: FollowTarget::Condition(Box::new(Query::UuidIn { uuids: subtrees })),
        inclusive: true,
    }))
}

/// Where the anchors of an `osm` path can be, when an earlier term says so
/// for less than reading them would cost — `None` to read them all.
///
/// A term free of the separator lies within one name of the path, so every
/// match sits at or below a node whose name holds it: among the nodes the
/// text index gives for the term (a superset), and their descendants. That
/// costs a read per node *with children* among them, and saves the rows of
/// every anchor outside (two reads each, before its ancestry): worth it when
/// the term is rare — a folder's name — and the anchors many — a file
/// extension, a common word. The rarest such term is the one taken.
fn anchors_within(
    names: &Eval<'_>,
    field: &str,
    earlier: &[String],
    anchor_literals: &[String],
) -> Option<RoaringBitmap> {
    let src = names.src;
    let anchors = src.text_superset(field, anchor_literals, None)?;
    let parents = src.parents(field);
    let rarest = earlier
        .iter()
        .filter(|t| !t.contains('/'))
        .filter_map(|t| {
            let pattern = format!("(?i){}", regex::escape(t));
            let holders =
                src.text_superset(field, &crate::regexp::required_literals(&pattern), None)?;
            let folders = holders.intersection_len(&parents);
            Some((folders, holders))
        })
        .min_by_key(|(folders, _)| *folders)?;
    let (folders, holders) = rarest;
    if folders.saturating_mul(2) >= anchors.len() {
        return None;
    }
    let mut within = src.descendants(field, &holders)?;
    within |= holders;
    Some(within)
}

/// Lower-cases like the resident walk: char by char, so a word-final sigma
/// folds as every other one does.
fn lower(s: &str) -> String {
    s.chars().flat_map(char::to_lowercase).collect()
}

/// A node's `(parent, name)` in a forest — `None` for a root's parent.
type Position = (Option<Uuid>, String);

/// The lower-cased path of each node, as `TreeCache::path_of` assembles it,
/// remembered, so candidates sharing a folder read its ancestry once.
struct AncestorPaths<'a> {
    store: &'a dyn Rows,
    field: &'a str,
    known: HashMap<Uuid, Option<String>>,
}

impl AncestorPaths<'_> {
    fn path(&mut self, uuid: Uuid) -> Result<Option<String>, ApiError> {
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

/// A `:path` leaf's field, the test its assembled path must pass, and what
/// that test lets a walk of the stored forest leave out.
type PathPredicate<'a> = (&'a str, Box<dyn Fn(&str) -> bool + 'a>, Narrow);

/// Where the paths a `:path` leaf matches can be.
enum Narrow {
    /// This path alone.
    Exact(String),
    /// Every path but this one.
    Differs(String),
    /// The paths starting with this text.
    Prefix(String),
    /// The paths on one side of this one.
    Range(Op, String),
    /// Anywhere.
    Walk,
}

/// The `(field, predicate on the assembled path, narrowing)` a `:path` leaf
/// reads, mirror for mirror of what the oracle's SQL compiler builds for the
/// same node.
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
            let narrow =
                crate::regexp::anchored_prefix(pattern).map_or(Narrow::Walk, Narrow::Prefix);
            return Some((field, Box::new(move |path: &str| re.is_match(path)), narrow));
        }
        _ => return None,
    };
    // `:path` compares against a string; any other operand is a 400.
    let Value::String(operand) = value else { return None };
    let narrow = match op {
        Op::Eq => Narrow::Exact(operand.clone()),
        Op::Neq => Narrow::Differs(operand.clone()),
        _ => Narrow::Range(op, operand.clone()),
    };
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
        narrow,
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
