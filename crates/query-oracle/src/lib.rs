//! The query oracle: every question answered by reading every row — the
//! second implementation the index and the key-value store are held to
//! (doc "The query oracle").
//!
//! It reads the primary data only — the metarecords and their field rows,
//! through the storage traits ([`Rows`]) — and nothing derived: no index, no
//! tree cache, no partition. The forest, the paths and the sort keys are
//! rebuilt from the rows on every call. That makes it slow and obviously
//! independent of whatever it checks, which is the whole point of an oracle:
//! a mistake in a derived structure cannot be reproduced here by sharing it.
//!
//! It replaced a SQL engine (a CTE per query node over the SQLite schema)
//! that had been the daemon's query engine before the bitmap index; the two
//! were held to each other over the whole test battery before the SQL one
//! went, cursors and rejections included. This crate is a dev-dependency of
//! the daemon and of nothing else, so no shipped binary links it.

use std::cmp::Ordering;
use std::collections::{BTreeSet, HashMap};

use uuid::Uuid;

use metafolder_core::metarecord::{escaped_to_bytes, TreeName, Value};
use metafolder_core::query::{osm_ordered_match, Aspect, FollowTarget, OsmMode, Query};

use metafolder_daemon::error::ApiError;
use metafolder_daemon::log::MAX_TREE_DEPTH;
use metafolder_daemon::pagination::{self, Cursor};
use metafolder_daemon::query_result::{osm_regex, SortKey, SortOrder};
use metafolder_daemon::query_validate::{
    check_query_size, too_wide_message, validate_query, validate_query_types,
    MAX_COMBINATOR_OPERANDS,
};
use metafolder_daemon::rows::FieldRow;
use metafolder_daemon::store::Rows;
use metafolder_daemon::tree_cache::PATH_KEY_SEP;

use metafolder_core::hex::encode as hex_encode;

/// The placeholder of an absent numeric sort component, so that a cursor
/// carries a number in every slot. It only ever meets another placeholder:
/// the type group, compared first, keeps it from deciding between two values.
const NUM_SENTINEL: f64 = -9e99;

type Set = BTreeSet<Uuid>;

/// Every metarecord and every row of the repository, read once.
struct Data {
    universe: Set,
    /// Each field's rows, in row-id order.
    by_field: HashMap<String, Vec<(Uuid, FieldRow)>>,
}

impl Data {
    fn read(store: &dyn Rows) -> Result<Self, ApiError> {
        let universe: Set = store.metarecords()?.into_iter().collect();
        let mut by_field: HashMap<String, Vec<(Uuid, FieldRow)>> = HashMap::new();
        store.for_each_row(&mut |uuid, row| {
            by_field.entry(row.name.clone()).or_default().push((uuid, row));
            Ok(())
        })?;
        for rows in by_field.values_mut() {
            rows.sort_by_key(|(_, r)| r.id);
        }
        Ok(Self { universe, by_field })
    }

    fn rows(&self, field: &str) -> &[(Uuid, FieldRow)] {
        self.by_field.get(field).map(Vec::as_slice).unwrap_or(&[])
    }

    /// The owners of the rows of `field` satisfying `pred`.
    fn holders(&self, field: &str, pred: impl Fn(Uuid, &Value) -> bool) -> Set {
        self.rows(field).iter().filter(|(u, r)| pred(*u, &r.value)).map(|(u, _)| *u).collect()
    }

    /// What a field holds, as the type check before evaluation asks it:
    /// `tree_ref` when any row is one, else a type it carries.
    fn stored_type(&self, field: &str) -> Option<String> {
        let mut other = None;
        for (_, row) in self.rows(field) {
            match &row.value {
                Value::TreeRef { .. } => return Some("tree_ref".into()),
                Value::Nothing => {}
                v => other = Some(v.type_str().to_string()),
            }
        }
        other
    }

    fn forest(&self, field: &str) -> Forest<'_> {
        Forest::new(self.rows(field))
    }
}

/// One field's forest, rebuilt from its `tree_ref` rows.
struct Forest<'a> {
    /// Every position, in row-id order: `(node, parent, name)`.
    positions: Vec<(Uuid, Option<Uuid>, &'a TreeName)>,
    /// The first (lowest-id) position of each node.
    first: HashMap<Uuid, (Option<Uuid>, &'a TreeName)>,
    /// Each node's positions, and each parent's children, in row-id order.
    of_node: HashMap<Uuid, Vec<(Option<Uuid>, &'a TreeName)>>,
    below: HashMap<Option<Uuid>, Vec<(Uuid, &'a TreeName)>>,
}

impl<'a> Forest<'a> {
    fn new(rows: &'a [(Uuid, FieldRow)]) -> Self {
        let mut positions = Vec::new();
        let mut first = HashMap::new();
        let mut of_node: HashMap<_, Vec<_>> = HashMap::new();
        let mut below: HashMap<_, Vec<_>> = HashMap::new();
        for (uuid, row) in rows {
            if let Value::TreeRef { parent, name } = &row.value {
                positions.push((*uuid, *parent, name));
                first.entry(*uuid).or_insert((*parent, name));
                of_node.entry(*uuid).or_default().push((*parent, name));
                below.entry(*parent).or_default().push((*uuid, name));
            }
        }
        Self { positions, first, of_node, below }
    }

    fn is_empty(&self) -> bool {
        self.positions.is_empty()
    }

    fn nodes(&self) -> Set {
        self.positions.iter().map(|(u, _, _)| *u).collect()
    }

    fn children(&self, parent: Option<Uuid>) -> impl Iterator<Item = (Uuid, &'a TreeName)> + '_ {
        self.below.get(&parent).into_iter().flatten().copied()
    }

    /// The node a typed path names: the first component is a root's name,
    /// every other one a child's; empty components after the first are
    /// redundant slashes. A component reads as typed or, when it holds an
    /// escape, as the bytes it decodes to — and names nothing when the two
    /// readings land on two different nodes.
    fn resolve(&self, path: &str) -> Option<Uuid> {
        let split: Vec<&str> = path.split('/').collect();
        let mut comps = vec![split[0]];
        comps.extend(split[1..].iter().copied().filter(|c| !c.is_empty()));
        let mut cur: Option<Uuid> = None;
        for comp in comps {
            let mut readings = vec![comp.as_bytes().to_vec()];
            readings.extend(escaped_to_bytes(comp));
            let found: BTreeSet<Uuid> = self
                .children(cur)
                .filter(|(_, name)| readings.iter().any(|r| r.as_slice() == name.as_bytes()))
                .map(|(u, _)| u)
                .collect();
            if found.len() != 1 {
                return None;
            }
            cur = found.into_iter().next();
        }
        cur
    }

    /// The path of a node through its first positions, `None` when the chain
    /// meets a parent outside the forest.
    fn path_of(&self, node: Uuid) -> Option<String> {
        let mut comps = Vec::new();
        let mut cur = node;
        for _ in 0..MAX_TREE_DEPTH {
            let (parent, name) = self.first.get(&cur)?;
            comps.push(name.display().into_owned());
            match parent {
                Some(p) => cur = *p,
                None => {
                    comps.reverse();
                    return Some(comps.join("/"));
                }
            }
        }
        None
    }

    /// Every path of a node, one per position whose parent chain reaches a
    /// root. A filesystem root is named `""`, so its descendants' paths start
    /// with `/`; a named root's do not.
    fn paths_of(&self, node: Uuid) -> Vec<String> {
        self.of_node
            .get(&node)
            .into_iter()
            .flatten()
            .filter_map(|(parent, name)| match parent {
                None => Some(name.display().into_owned()),
                Some(p) => self.path_of(*p).map(|pp| format!("{pp}/{}", name.display())),
            })
            .collect()
    }

    /// The nodes with at least one path satisfying `pred`.
    fn nodes_whose_path(&self, pred: impl Fn(&str) -> bool) -> Set {
        self.nodes().into_iter().filter(|n| self.paths_of(*n).iter().any(|p| pred(p))).collect()
    }

    /// Everything below `root`, at any depth (not `root` itself, unless a
    /// cycle brings it back).
    fn descendants(&self, root: Uuid) -> Set {
        let mut out = Set::new();
        let mut frontier = vec![root];
        while let Some(node) = frontier.pop() {
            for (child, _) in self.children(Some(node)) {
                if out.insert(child) {
                    frontier.push(child);
                }
            }
        }
        out
    }

    /// The sort key of one position: the names from the top of its chain down
    /// to it, joined by [`PATH_KEY_SEP`]. The chain climbs through first
    /// positions and stops at a root, or at a parent outside the forest (a
    /// detached node counts as a root).
    fn sort_key(&self, parent: Option<Uuid>, name: &TreeName) -> String {
        let mut comps = vec![name.display().into_owned()];
        let mut cur = parent;
        let mut depth = 0;
        while let Some(p) = cur {
            if depth >= MAX_TREE_DEPTH {
                break;
            }
            let Some((pp, pname)) = self.first.get(&p) else { break };
            comps.push(pname.display().into_owned());
            cur = *pp;
            depth += 1;
        }
        comps.reverse();
        comps.join(&PATH_KEY_SEP.to_string())
    }
}

// ── Public surface ────────────────────────────────────────────────────────────

/// What a field holds, for the type check a query goes through.
pub fn stored_type(store: &dyn Rows, field: &str) -> Result<Option<String>, ApiError> {
    Ok(Data::read(store)?.stored_type(field))
}

/// Every `(field name, value type)` the repository holds, `Nothing` aside,
/// sorted — only those of `type_filter` when given.
pub fn field_catalog(
    store: &dyn Rows,
    type_filter: Option<&str>,
) -> Result<Vec<(String, String)>, ApiError> {
    let data = Data::read(store)?;
    let mut out: Vec<(String, String)> = data
        .by_field
        .iter()
        .flat_map(|(name, rows)| {
            rows.iter()
                .filter(|(_, r)| !matches!(r.value, Value::Nothing))
                .map(move |(_, r)| (name.clone(), r.value.type_str().to_string()))
        })
        .filter(|(_, ty)| type_filter.is_none_or(|t| t == ty))
        .collect();
    out.sort();
    out.dedup();
    Ok(out)
}

/// The number of metarecords matching `query`.
pub fn count(store: &dyn Rows, query: &Query) -> Result<usize, ApiError> {
    let data = Data::read(store)?;
    validate(&data, query)?;
    Ok(eval(&data, query)?.len())
}

/// One page of the metarecords matching `query`, in `sort` order (uuid order
/// last), and the cursor of the next page when `limit` leaves some out.
pub fn execute(
    store: &dyn Rows,
    query: &Query,
    sort: &[SortKey],
    limit: Option<usize>,
    cursor: Option<&str>,
) -> Result<(Vec<Uuid>, Option<String>), ApiError> {
    if cursor.is_some() && limit.is_none() {
        return Err(ApiError::bad_request("'cursor' requires 'limit'"));
    }
    let data = Data::read(store)?;
    validate(&data, query)?;
    // A cursor is bound to the (query, sort) pair that produced it.
    let hash = pagination::context_hash(&[
        "query",
        &serde_json::to_string(query).map_err(|e| ApiError::internal(e.to_string()))?,
        &serde_json::to_string(sort).map_err(|e| ApiError::internal(e.to_string()))?,
    ]);
    let matched = eval(&data, query)?;

    // Per sort key, each metarecord's values of the key's field.
    let forests: Vec<Forest<'_>> = sort.iter().map(|k| data.forest(&k.field)).collect();
    let values: Vec<HashMap<Uuid, Vec<&Value>>> = sort
        .iter()
        .map(|k| {
            let mut by_owner: HashMap<Uuid, Vec<&Value>> = HashMap::new();
            for (u, r) in data.rows(&k.field) {
                by_owner.entry(*u).or_default().push(&r.value);
            }
            by_owner
        })
        .collect();
    let mut rows: Vec<(Vec<Component>, Uuid)> = matched
        .iter()
        .map(|&uuid| {
            let keys = sort
                .iter()
                .zip(&forests)
                .zip(&values)
                .flat_map(|((key, forest), values)| {
                    let held = values.get(&uuid).map(Vec::as_slice).unwrap_or(&[]);
                    sort_components(forest, key, held)
                })
                .collect();
            (keys, uuid)
        })
        .collect();
    let directions: Vec<bool> = sort
        .iter()
        .flat_map(|k| {
            let asc = k.order == SortOrder::Asc;
            // Missing-last first, whatever the order; then the four value
            // components in the key's direction.
            [true, asc, asc, asc, asc]
        })
        .collect();
    let order = |a: &(Vec<Component>, Uuid), b: &(Vec<Component>, Uuid)| {
        compare_rows(&directions, &a.0, a.1, &b.0, b.1)
    };
    rows.sort_by(order);

    if let Some(token) = cursor {
        let parsed = pagination::decode(token, hash)?;
        let after = (cursor_components(&parsed, sort.len())?, parsed.last_uuid()?);
        rows.retain(|r| order(r, &after) == Ordering::Greater);
    }

    let Some(limit) = limit else {
        return Ok((rows.into_iter().map(|(_, u)| u).collect(), None));
    };
    let has_more = rows.len() > limit;
    rows.truncate(limit);
    let next = match rows.last() {
        Some((keys, uuid)) if has_more => Some(pagination::encode(&Cursor {
            keys: keys.iter().map(Component::to_cursor).collect(),
            uuid: uuid.as_simple().to_string(),
            h: hash,
        })),
        _ => None,
    };
    Ok((rows.into_iter().map(|(_, u)| u).collect(), next))
}

/// The metarecords whose assembled `field` path matches `terms` as ordered,
/// case-insensitive substrings (every path-bearing one for no term), sorted.
pub fn osm_path_matches(
    store: &dyn Rows,
    field: &str,
    terms: &[String],
) -> Result<Vec<Uuid>, ApiError> {
    let data = Data::read(store)?;
    Ok(osm_path(&data, field, terms).into_iter().collect())
}

fn validate(data: &Data, query: &Query) -> Result<(), ApiError> {
    check_query_size(query)?;
    validate_query(query)?;
    validate_query_types(query, &|f| data.stored_type(f))
}

// ── Evaluation ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CmpOp {
    Eq,
    Lt,
    Lte,
    Gt,
    Gte,
}

impl CmpOp {
    fn holds(self, ord: Option<Ordering>) -> bool {
        let Some(ord) = ord else { return false };
        match self {
            CmpOp::Eq => ord == Ordering::Equal,
            CmpOp::Lt => ord == Ordering::Less,
            CmpOp::Lte => ord != Ordering::Greater,
            CmpOp::Gt => ord == Ordering::Greater,
            CmpOp::Gte => ord != Ordering::Less,
        }
    }

    fn is_ordered(self) -> bool {
        self != CmpOp::Eq
    }
}

/// Every operand is evaluated, left to right, even where the answer is
/// already known: a rejection further right must not depend on what came
/// before it.
fn eval(data: &Data, q: &Query) -> Result<Set, ApiError> {
    Ok(match q {
        Query::IsPresent { field, aspect } => presence(data, field, *aspect, true),
        Query::IsAbsent { field, aspect } => presence(data, field, *aspect, false),
        Query::IsUnknown { field } => {
            let known = data.holders(field, |_, _| true);
            data.universe.difference(&known).copied().collect()
        }
        Query::Eq { field, value, aspect } => comparison(data, field, value, CmpOp::Eq, *aspect)?,
        Query::Lt { field, value, aspect } => comparison(data, field, value, CmpOp::Lt, *aspect)?,
        Query::Lte { field, value, aspect } => comparison(data, field, value, CmpOp::Lte, *aspect)?,
        Query::Gt { field, value, aspect } => comparison(data, field, value, CmpOp::Gt, *aspect)?,
        Query::Gte { field, value, aspect } => comparison(data, field, value, CmpOp::Gte, *aspect)?,
        Query::Neq { field, value, aspect } => {
            // At least one occurrence differing from `value` — not the
            // complement of `Eq`: a multi-valued field can do both.
            if *aspect == Aspect::Path {
                let operand = path_operand(value)?;
                return Ok(data.forest(field).nodes_whose_path(|p| p != operand));
            }
            let pred = row_predicate(data, field, value, CmpOp::Eq, *aspect)?;
            data.holders(field, |u, v| !matches!(v, Value::Nothing) && !pred(u, v))
        }
        Query::And { operands } | Query::Or { operands } => {
            if operands.is_empty() {
                return Err(ApiError::bad_request("'and'/'or' need at least one operand"));
            }
            if operands.len() > MAX_COMBINATOR_OPERANDS {
                return Err(ApiError::bad_request(too_wide_message(operands.len())));
            }
            let sets = operands.iter().map(|o| eval(data, o)).collect::<Result<Vec<_>, _>>()?;
            let is_and = matches!(q, Query::And { .. });
            let mut sets = sets.into_iter();
            let first = sets.next().expect("at least one operand");
            sets.fold(first, |acc, s| {
                if is_and {
                    acc.intersection(&s).copied().collect()
                } else {
                    acc.union(&s).copied().collect()
                }
            })
        }
        Query::Not { operand } => {
            let sub = eval(data, operand)?;
            data.universe.difference(&sub).copied().collect()
        }
        Query::Matches { field, pattern, aspect } => {
            let re = metafolder_daemon::regexp::compile(pattern)
                .map_err(|e| ApiError::bad_request(format!("invalid regex pattern: {e}")))?;
            if *aspect == Aspect::Path {
                data.forest(field).nodes_whose_path(|p| re.is_match(p))
            } else {
                data.holders(field, |_, v| text_of(v).is_some_and(|t| re.is_match(&t)))
            }
        }
        Query::Osm { field, terms, mode } => match mode {
            OsmMode::Direct => {
                let re = metafolder_daemon::regexp::compile(&osm_regex(terms))
                    .map_err(|e| ApiError::bad_request(format!("invalid regex pattern: {e}")))?;
                data.holders(field, |_, v| text_of(v).is_some_and(|t| re.is_match(&t)))
            }
            OsmMode::Path => osm_path(data, field, terms),
        },
        Query::SameAs { field, target } => {
            let targets = eval(data, target)?;
            let wanted: Vec<&Value> = data
                .rows(field)
                .iter()
                .filter(|(u, r)| targets.contains(u) && !matches!(r.value, Value::Nothing))
                .map(|(_, r)| &r.value)
                .collect();
            data.holders(field, |_, v| {
                !matches!(v, Value::Nothing) && wanted.iter().any(|w| same_value(v, w))
            })
        }
        Query::Follows { field, target } => match target {
            FollowTarget::Condition(cond) => {
                let targets = eval(data, cond)?;
                data.holders(field, |_, v| match v {
                    Value::Ref(t) => targets.contains(t),
                    Value::TreeRef { parent: Some(p), .. } => targets.contains(p),
                    _ => false,
                })
            }
            FollowTarget::Path(path) => match data.forest(field).resolve(path) {
                None => Set::new(),
                Some(node) => data.holders(
                    field,
                    |_, v| matches!(v, Value::TreeRef { parent: Some(p), .. } if *p == node),
                ),
            },
        },
        Query::FollowsTransitive { field, target, inclusive } => {
            let forest = data.forest(field);
            let roots: Set = match target {
                FollowTarget::Path(path) => forest.resolve(path).into_iter().collect(),
                FollowTarget::Condition(cond) => eval(data, cond)?,
            };
            // Only a forest has descendants; its roots are kept by the
            // inclusive form alone, and only on a forest.
            let mut out = Set::new();
            for root in roots {
                if *inclusive && !forest.is_empty() {
                    out.insert(root);
                }
                out.extend(forest.descendants(root));
            }
            out
        }
        Query::UuidIn { uuids } => {
            uuids.iter().filter(|u| data.universe.contains(u)).copied().collect()
        }
    })
}

/// `IsPresent` / `IsAbsent`. Under `parent`, whether a position has a real
/// parent — so `field:parent IS ABSENT` names the roots. Otherwise a row that
/// is, or is not, the explicit absence `Nothing`.
fn presence(data: &Data, field: &str, aspect: Aspect, present: bool) -> Set {
    if aspect == Aspect::Parent {
        return data.holders(field, |_, v| match v {
            Value::TreeRef { parent, .. } => parent.is_some() == present,
            _ => false,
        });
    }
    data.holders(field, |_, v| matches!(v, Value::Nothing) != present)
}

fn comparison(
    data: &Data,
    field: &str,
    value: &Value,
    op: CmpOp,
    aspect: Aspect,
) -> Result<Set, ApiError> {
    if aspect == Aspect::Path {
        let operand = path_operand(value)?;
        return Ok(data.forest(field).nodes_whose_path(|p| op.holds(Some(p.cmp(operand)))));
    }
    let pred = row_predicate(data, field, value, op, aspect)?;
    Ok(data.holders(field, pred))
}

/// The string a `:path` comparison is made against.
fn path_operand(value: &Value) -> Result<&str, ApiError> {
    match value {
        Value::String(s) => Ok(s),
        _ => Err(ApiError::bad_request(format!(
            "the ':path' aspect compares against a string, got {}",
            value.type_str()
        ))),
    }
}

/// The text a row offers to a pattern or to a `:value` string comparison: a
/// string, or the name of a position.
fn text_of(v: &Value) -> Option<std::borrow::Cow<'_, str>> {
    match v {
        Value::String(s) => Some(std::borrow::Cow::Borrowed(s)),
        Value::TreeRef { name, .. } => Some(name.display()),
        _ => None,
    }
}

/// Whether one row `(owner, value)` holds.
type RowPredicate<'a> = Box<dyn Fn(Uuid, &Value) -> bool + 'a>;

/// Whether one row `(owner, value)` compares to `value` as `op` asks.
fn row_predicate<'a>(
    data: &Data,
    field: &str,
    value: &'a Value,
    op: CmpOp,
    aspect: Aspect,
) -> Result<RowPredicate<'a>, ApiError> {
    let equality_only = |type_name: &str| {
        Err(ApiError::bad_request(format!(
            "ordered comparison is not supported on {type_name} values"
        )))
    };
    Ok(match value {
        Value::Nothing => {
            return Err(ApiError::bad_request(
                "comparisons with 'nothing' are not allowed; use is_absent / is_unknown",
            ))
        }
        // Int and Float compare numerically together.
        Value::Int(_) | Value::Float(_) => {
            let x = match value {
                Value::Int(n) => *n as f64,
                Value::Float(f) => *f,
                _ => unreachable!(),
            };
            Box::new(move |_, v| match v {
                Value::Int(n) => op.holds((*n as f64).partial_cmp(&x)),
                Value::Float(f) => op.holds(f.partial_cmp(&x)),
                _ => false,
            })
        }
        Value::String(text) => {
            // `:parent` names the parent by its path: the positions under it.
            if aspect == Aspect::Parent {
                let node = data.forest(field).resolve(text);
                return Ok(Box::new(move |_, v| match (v, node) {
                    (Value::TreeRef { parent: Some(p), .. }, Some(n)) => *p == n,
                    _ => false,
                }));
            }
            // Raw equality on a forest is the exact node the path names; on a
            // string, the literal.
            if aspect == Aspect::Raw && op == CmpOp::Eq {
                let node = data.forest(field).resolve(text);
                return Ok(Box::new(move |owner, v| match v {
                    Value::String(s) => s == text,
                    Value::TreeRef { .. } => Some(owner) == node,
                    _ => false,
                }));
            }
            // The row's own text: a string, or a position's name.
            Box::new(move |_, v| match v {
                Value::String(s) => op.holds(Some(s.as_str().cmp(text))),
                Value::TreeRef { name, .. } => op.holds(Some(name.display().as_ref().cmp(text))),
                _ => false,
            })
        }
        // A datetime compares only with a datetime.
        Value::DateTime(ms) => Box::new(move |_, v| match v {
            Value::DateTime(x) => op.holds(Some(x.cmp(ms))),
            _ => false,
        }),
        Value::Bool(b) => {
            if op.is_ordered() {
                return equality_only("bool");
            }
            Box::new(move |_, v| matches!(v, Value::Bool(x) if x == b))
        }
        Value::Ref(u) => {
            if op.is_ordered() {
                return equality_only("ref");
            }
            Box::new(move |_, v| matches!(v, Value::Ref(x) if x == u))
        }
        Value::RefBase(u) => {
            if op.is_ordered() {
                return equality_only("refbase");
            }
            Box::new(move |_, v| matches!(v, Value::RefBase(x) if x == u))
        }
        Value::TreeRef { parent, name } => {
            if op.is_ordered() {
                return equality_only("tree_ref");
            }
            Box::new(move |_, v| match v {
                Value::TreeRef { parent: p, name: n } => {
                    p == parent && n.display() == name.display()
                }
                _ => false,
            })
        }
        Value::ExternalRef { repo, metarecord } => {
            if op.is_ordered() {
                return equality_only("externalref");
            }
            Box::new(
                move |_, v| matches!(v, Value::ExternalRef { repo: r, metarecord: m } if r == repo && m == metarecord),
            )
        }
    })
}

/// Two rows holding the same value: the same type and the same stored value
/// (a number numerically, so `0.0` and `-0.0` are one value).
fn same_value(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Float(x), Value::Float(y)) => x == y,
        (Value::TreeRef { parent: p, name: n }, Value::TreeRef { parent: q, name: m }) => {
            p == q && n.as_bytes() == m.as_bytes()
        }
        _ => a == b,
    }
}

/// OSM `Path`: the nodes of `field`'s forest with a path matching `terms`
/// in order, case-insensitively — every node for no term.
fn osm_path(data: &Data, field: &str, terms: &[String]) -> Set {
    let forest = data.forest(field);
    if terms.is_empty() {
        return forest.nodes();
    }
    forest.nodes_whose_path(|p| osm_ordered_match(&p.to_lowercase(), terms))
}

// ── Sorting ───────────────────────────────────────────────────────────────────

/// One component of a row's position in the order. Per sort key there are
/// five — missing-last flag, type group, number, text, bytes — then the uuid.
#[derive(Debug, Clone)]
enum Component {
    Int(i64),
    Num(f64),
    Text(String),
    Bytes(Vec<u8>),
}

impl Component {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Component::Int(a), Component::Int(b)) => a.cmp(b),
            (Component::Num(a), Component::Num(b)) => a.partial_cmp(b).unwrap_or(Ordering::Equal),
            (Component::Text(a), Component::Text(b)) => a.cmp(b),
            (Component::Bytes(a), Component::Bytes(b)) => a.cmp(b),
            _ => Ordering::Equal,
        }
    }

    fn to_cursor(&self) -> serde_json::Value {
        match self {
            Component::Int(n) => serde_json::json!(n),
            Component::Num(f) => serde_json::json!(hex_encode(&f.to_bits().to_be_bytes())),
            Component::Text(s) => serde_json::json!(s),
            Component::Bytes(b) => serde_json::json!(hex_encode(b)),
        }
    }
}

/// A row's value for sorting: type group, number, text, bytes — each absent
/// where the type has none.
type SortValue = (i64, Option<f64>, Option<String>, Option<Vec<u8>>);

fn sort_value(forest: &Forest<'_>, v: &Value) -> Option<SortValue> {
    Some(match v {
        Value::Nothing => return None,
        Value::Bool(b) => (0, Some(*b as i64 as f64), None, None),
        Value::Int(n) => (1, Some(*n as f64), None, None),
        Value::Float(f) => (1, Some(*f), None, None),
        Value::String(s) => (2, None, Some(s.clone()), None),
        Value::DateTime(ms) => (3, Some(*ms as f64), None, None),
        Value::Ref(u) | Value::RefBase(u) => (4, None, None, Some(u.as_bytes().to_vec())),
        Value::ExternalRef { metarecord, .. } => {
            (4, None, None, Some(metarecord.as_bytes().to_vec()))
        }
        Value::TreeRef { parent, name } => (5, None, Some(forest.sort_key(*parent, name)), None),
    })
}

/// Absent sorts before present, as SQL's `NULL` does.
fn cmp_sort_values(a: &SortValue, b: &SortValue) -> Ordering {
    fn opt<T>(a: &Option<T>, b: &Option<T>, f: impl Fn(&T, &T) -> Ordering) -> Ordering {
        match (a, b) {
            (None, None) => Ordering::Equal,
            (None, Some(_)) => Ordering::Less,
            (Some(_), None) => Ordering::Greater,
            (Some(x), Some(y)) => f(x, y),
        }
    }
    a.0.cmp(&b.0)
        .then_with(|| opt(&a.1, &b.1, |x, y| x.partial_cmp(y).unwrap_or(Ordering::Equal)))
        .then_with(|| opt(&a.2, &b.2, |x, y| x.cmp(y)))
        .then_with(|| opt(&a.3, &b.3, |x, y| x.cmp(y)))
}

/// The five components of one metarecord for one sort key: its
/// representative value — the least of its values ascending, the greatest
/// descending — or, lacking the field, the missing-last placeholders.
fn sort_components(forest: &Forest<'_>, key: &SortKey, held: &[&Value]) -> [Component; 5] {
    let values = held.iter().filter_map(|v| sort_value(forest, v));
    let rep = match key.order {
        SortOrder::Asc => values.min_by(cmp_sort_values),
        SortOrder::Desc => values.max_by(cmp_sort_values),
    };
    match rep {
        // A metarecord lacking the field: its type group is the one past
        // every type, as the SQL oracle's `CASE … ELSE 6` gave it.
        None => [
            Component::Int(1),
            Component::Int(6),
            Component::Num(NUM_SENTINEL),
            Component::Text(String::new()),
            Component::Bytes(Vec::new()),
        ],
        Some((group, num, text, bytes)) => [
            Component::Int(0),
            Component::Int(group),
            Component::Num(num.unwrap_or(NUM_SENTINEL)),
            Component::Text(text.unwrap_or_default()),
            Component::Bytes(bytes.unwrap_or_default()),
        ],
    }
}

fn compare_rows(
    directions: &[bool],
    a: &[Component],
    a_uuid: Uuid,
    b: &[Component],
    b_uuid: Uuid,
) -> Ordering {
    for ((x, y), asc) in a.iter().zip(b).zip(directions) {
        let ord = x.cmp(y);
        if ord != Ordering::Equal {
            return if *asc { ord } else { ord.reverse() };
        }
    }
    a_uuid.cmp(&b_uuid)
}

/// The components a cursor carries, typed back.
fn cursor_components(cursor: &Cursor, n_sort: usize) -> Result<Vec<Component>, ApiError> {
    let invalid = || ApiError::bad_request("invalid cursor");
    if cursor.keys.len() != 5 * n_sort {
        return Err(invalid());
    }
    cursor
        .keys
        .iter()
        .enumerate()
        .map(|(i, key)| {
            Ok(match i % 5 {
                0 | 1 => Component::Int(key.as_i64().ok_or_else(invalid)?),
                2 => {
                    let bytes = hex_decode(key.as_str().ok_or_else(invalid)?)?;
                    let arr: [u8; 8] = bytes.as_slice().try_into().map_err(|_| invalid())?;
                    Component::Num(f64::from_bits(u64::from_be_bytes(arr)))
                }
                3 => Component::Text(key.as_str().ok_or_else(invalid)?.to_string()),
                _ => Component::Bytes(hex_decode(key.as_str().ok_or_else(invalid)?)?),
            })
        })
        .collect()
}

fn hex_decode(s: &str) -> Result<Vec<u8>, ApiError> {
    if !s.len().is_multiple_of(2) || !s.is_ascii() {
        return Err(ApiError::bad_request("invalid cursor"));
    }
    (0..s.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&s[i..i + 2], 16)
                .map_err(|_| ApiError::bad_request("invalid cursor"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A float sort key travels in the cursor as the hex of its bits: a
    /// decimal JSON number can come back one ULP off, which at a page
    /// boundary duplicates or skips a row.
    #[test]
    fn float_cursor_roundtrip_is_bit_exact() {
        let mut cases = vec![
            0.0,
            -0.0,
            0.1,
            0.1 + 0.2,
            1.0 / 3.0,
            std::f64::consts::PI,
            f64::MIN_POSITIVE,
            f64::from_bits(1), // smallest subnormal
            f64::MAX,
            f64::MIN,
            2f64.powi(53) + 2.0,
        ];
        for i in 0..5000u64 {
            let f = f64::from_bits(i.wrapping_mul(0x9E37_79B9_7F4A_7C15));
            if f.is_finite() {
                cases.push(f);
            }
        }
        for &f in &cases {
            let keys = vec![
                Component::Int(0).to_cursor(),
                Component::Int(1).to_cursor(),
                Component::Num(f).to_cursor(),
                Component::Text(String::new()).to_cursor(),
                Component::Bytes(Vec::new()).to_cursor(),
            ];
            // Through the same JSON serialization the cursor undergoes.
            let json = serde_json::to_vec(&keys).unwrap();
            let cursor =
                Cursor { keys: serde_json::from_slice(&json).unwrap(), uuid: String::new(), h: 0 };
            let Component::Num(back) = cursor_components(&cursor, 1).unwrap()[2] else {
                panic!("not a number")
            };
            assert_eq!(f.to_bits(), back.to_bits(), "diverged at {f}");
        }
    }
}
