//! Query compilation and execution (spec-query). A `Query` compiles to a CTE
//! chain — one CTE per node — over the EAV `field` table; the result is
//! restricted to metarecords owned exclusively by the current repository.
//! `Follows`/`FollowsTransitive` path targets are resolved through the tree
//! cache before SQL generation (hybrid execution). Sorting and keyset
//! pagination follow spec-data-model "Pagination".

use anyhow::Result;
use rusqlite::types::Value as SqlValue;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use metafolder_core::metarecord::{Field, MetaRecord, Value, ZERO_UUID};
use metafolder_core::query::{Aspect, FollowTarget, OsmMode, Query};

use crate::db;
use crate::error::ApiError;
use crate::pagination::{self, Cursor};
use crate::tree_cache::TreeCache;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SortOrder {
    Asc,
    Desc,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SortKey {
    pub field: String,
    #[serde(default = "default_order")]
    pub order: SortOrder,
}

fn default_order() -> SortOrder {
    SortOrder::Asc
}

/// Sentinel replacing NULL numeric key components so that keyset comparisons
/// stay two-valued. A NULL component only ever meets another NULL (the
/// type-group column discriminates first), so sentinels never decide an
/// ordering between two real values.
const NUM_SENTINEL: &str = "-9e99";

/// The recursive CTE that reconstructs the full-path sort key of every
/// `tree_ref` row of sort key `i`'s field, for the rows of the filtered universe
/// `_res`. Emitted for every sort key: on a non-`tree_ref` field the base case
/// selects nothing, so it costs one empty scan.
///
/// Each step prepends the parent's name, so a row's *terminal* tuple — the one
/// the walk could not extend — carries the components of its whole path joined
/// by [`crate::tree_cache::PATH_KEY_SEP`], a separator below every character a
/// name can hold, which makes a plain byte comparison of two keys a
/// component-by-component comparison of the two paths (spec-data-model "Sorting
/// a `TreeRef` field").
///
/// Two details exist only to reproduce, row for row, what the tree cache does
/// with the same forest — the in-memory index builds its keys from it
/// (`tree_cache::SortKeys`) and the two engines must not diverge
/// (`tests/index_oracle.rs`):
///
/// - a node is linked under its parent's *first* position, so the step joins the
///   lowest-id row of the parent rather than all of them;
/// - a node whose parent has no `tree_ref` row is left detached, i.e. treated as
///   a root — so the walk stops there too, which is what the terminal predicate
///   [`path_key_terminal`] adds to "the parent is the root sentinel".
fn path_key_cte(i: usize) -> String {
    let sep = crate::tree_cache::PATH_KEY_SEP as u32;
    let max_depth = crate::log::MAX_TREE_DEPTH;
    format!(
        "SELECT f.id, f.value_uuid, f.value_name, 0 \
           FROM field f JOIN _res ON _res.uuid = f.metarecord_uuid \
          WHERE f.field_name = ? AND f.value_type = 'tree_ref' \
         UNION ALL \
         SELECT p.id, pf.value_uuid, pf.value_name || char({sep}) || p.path_key, p.depth + 1 \
           FROM _p{i} p JOIN field pf ON pf.id = ({FIRST_TREE_ROW}) \
          WHERE p.depth < {max_depth} AND p.parent != {ZERO_BLOB_SQL}"
    )
}

/// The join condition selecting a row's *terminal* path tuple out of
/// [`path_key_cte`]'s chain: the walk stopped either at a root or at a parent
/// carrying no `tree_ref` row (a detached node, which the tree cache treats as a
/// root as well).
fn path_key_terminal(i: usize) -> String {
    format!(
        "_p{i}.id = field.id AND (_p{i}.parent = {ZERO_BLOB_SQL} \
             OR NOT EXISTS (SELECT 1 FROM field x \
                  WHERE x.metarecord_uuid = _p{i}.parent \
                    AND x.field_name = ? AND x.value_type = 'tree_ref'))"
    )
}

/// The lowest-id `tree_ref` row of the node `p.parent` — the position the tree
/// cache links a child under when its parent is itself multi-position.
const FIRST_TREE_ROW: &str = "SELECT MIN(x.id) FROM field x \
     WHERE x.metarecord_uuid = p.parent AND x.field_name = ? AND x.value_type = 'tree_ref'";

/// The 16 zero bytes a root `tree_ref` row stores as its parent
/// ([`metafolder_core::metarecord::ZERO_UUID`]), as a SQL blob literal.
const ZERO_BLOB_SQL: &str = "x'00000000000000000000000000000000'";

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
pub(crate) fn too_wide_message(got: usize) -> String {
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
                db::encode_value(value).value_type
            )))
        }
        _ => Ok(()),
    }
}

/// Assembles the `select`-projected JSON objects for a page of result UUIDs,
/// polling `cancel` every few hundred rows so a long assembly (the dominant cost
/// of a `select=*` query over many matches) can be stopped (spec-tasks
/// "Cancellation"). `fields_filter = None` keeps every field; `Some(list)` keeps
/// only the named ones. Pass `&|| false` for uncancellable callers.
pub fn assemble_selected(
    conn: &Connection,
    uuids: &[Uuid],
    fields_filter: Option<&[String]>,
    cancel: &dyn Fn() -> bool,
) -> Result<Vec<serde_json::Value>, ApiError> {
    // Batched reads: the whole page's versions and field rows in a couple of
    // `IN (…)` scans, not a query per metarecord.
    let versions = db::versions_for(conn, uuids)?;
    let mut rows = db::field_rows_for(conn, uuids)?;
    let mut objects = Vec::with_capacity(uuids.len());
    for (i, &uuid) in uuids.iter().enumerate() {
        if i % 256 == 0 && cancel() {
            return Err(ApiError::conflict("query cancelled"));
        }
        let version = *versions
            .get(&uuid)
            .ok_or_else(|| ApiError::not_found(format!("Metarecord not found: {uuid}")))?;
        let fields: Vec<Field> = rows
            .remove(&uuid)
            .unwrap_or_default()
            .into_iter()
            .filter(|r| fields_filter.is_none_or(|f| f.contains(&r.name)))
            .map(|r| Field { id: Some(r.id), name: r.name, value: r.value })
            .collect();
        let metarecord = MetaRecord { uuid, version, fields };
        objects.push(serde_json::to_value(metarecord).expect("metarecord serialization"));
    }
    Ok(objects)
}

/// Counts the matching metarecords without fetching them: the same CTE chain
/// as `execute`, wrapped in a `COUNT(*)` (no sort CTEs, no pagination).
pub fn count(conn: &Connection, cache: &mut TreeCache, query: &Query) -> Result<usize, ApiError> {
    check_query_size(query)?;
    validate_query(query)?;
    crate::query_validate::validate_query_types(query, &|f| stored_type(conn, f).ok().flatten())?;
    let mut compiler = Compiler::new(conn, cache);
    let last = compiler.compile_node(query)?;
    let Compiler { ctes, params, .. } = compiler;
    let cte_sql: Vec<String> =
        ctes.into_iter().map(|(name, body)| format!("{name} AS ({body})")).collect();
    let sql = format!(
        "WITH {} SELECT COUNT(*) FROM {last} WHERE uuid IN (SELECT uuid FROM _repo)",
        cte_sql.join(", ")
    );
    let total: i64 = conn
        .query_row(&sql, rusqlite::params_from_iter(params.iter()), |row| row.get(0))
        .map_err(anyhow::Error::from)?;
    Ok(total as usize)
}

/// Executes a query: returns one page of matching UUIDs in query order plus
/// the next cursor (always None when `limit` is absent).
pub fn execute(
    conn: &Connection,
    cache: &mut TreeCache,
    query: &Query,
    sort: &[SortKey],
    limit: Option<usize>,
    cursor: Option<&str>,
) -> Result<(Vec<Uuid>, Option<String>), ApiError> {
    if cursor.is_some() && limit.is_none() {
        return Err(ApiError::bad_request("'cursor' requires 'limit'"));
    }
    check_query_size(query)?;
    validate_query(query)?;
    crate::query_validate::validate_query_types(query, &|f| stored_type(conn, f).ok().flatten())?;

    // The cursor is bound to the exact (query, sort) pair that produced it.
    let hash = pagination::context_hash(&[
        "query",
        &serde_json::to_string(query).map_err(|e| ApiError::internal(e.to_string()))?,
        &serde_json::to_string(sort).map_err(|e| ApiError::internal(e.to_string()))?,
    ]);

    let mut compiler = Compiler::new(conn, cache);
    let last = compiler.compile_node(query)?;
    let Compiler { mut ctes, mut params, .. } = compiler;

    // The filtered universe. Marked MATERIALIZED where the CTEs are emitted
    // below, so it is computed once and joined to `field` per sort key rather
    // than re-evaluated each time (which re-runs the whole filter — see there).
    ctes.push((
        "_res".into(),
        format!("SELECT uuid FROM {last} WHERE uuid IN (SELECT uuid FROM _repo)"),
    ));

    // One CTE per sort key: the metarecord's representative row for that field
    // (min for asc, max for desc), normalised into comparable components. Each
    // `_s{i}` is built by joining the *filtered* universe `_res` to `field`
    // (LEFT, so a metarecord lacking the field still yields one row, flagged
    // `present = 0`), so it carries exactly the `_res` uuids — one per uuid.
    // The window is therefore computed only over the filtered rows, and the
    // final query drives straight from `_s0` (no `_res LEFT JOIN _s{i}` against
    // an unindexed window output, which was the O(filtered × total) trap that
    // used to make `FollowsTransitive` + sort pathological on large repos).
    let driver = if sort.is_empty() { "_res" } else { "_s0" };
    let mut joins = String::new();
    let mut select_cols = format!("{driver}.uuid AS uuid");
    let mut order_by = Vec::new();
    // (alias, ascending) pairs forming the total order.
    let mut components: Vec<(String, bool)> = Vec::new();

    for (i, key) in sort.iter().enumerate() {
        let dir = match key.order {
            SortOrder::Asc => "ASC",
            SortOrder::Desc => "DESC",
        };
        let grp = "CASE field.value_type \
             WHEN 'bool' THEN 0 WHEN 'int' THEN 1 WHEN 'float' THEN 1 \
             WHEN 'string' THEN 2 WHEN 'datetime' THEN 3 \
             WHEN 'ref' THEN 4 WHEN 'refbase' THEN 4 WHEN 'externalref' THEN 4 \
             WHEN 'tree_ref' THEN 5 ELSE 6 END";
        // datetime is stored as Unix ms in value_int, so it sorts numerically;
        // its own `grp` (3) keeps it from interleaving with bool/int/float.
        let num = "CASE WHEN field.value_type IN ('bool', 'int', 'datetime') \
                THEN CAST(field.value_int AS REAL) \
             WHEN field.value_type = 'float' THEN field.value_real END";
        // A `tree_ref` sorts on its *whole* path, not on the last component
        // (spec-data-model "Sorting a `TreeRef` field"): `_p{i}` reconstructs
        // one key per row of the sort field, walking the parent chain up to a
        // root. The in-memory engine builds the same key from the tree cache.
        let text = format!(
            "CASE WHEN field.value_type = 'string' THEN field.value_text \
             WHEN field.value_type = 'tree_ref' THEN _p{i}.path_key END"
        );
        let blob = "CASE WHEN field.value_type IN ('ref', 'refbase', 'externalref') \
             THEN field.value_uuid END";
        ctes.push((format!("_p{i}(id, parent, path_key, depth)"), path_key_cte(i)));
        // The two `?` of `_p{i}`: the base case's field name, then the
        // recursive step's (inside `FIRST_TREE_ROW`).
        params.push(SqlValue::Text(key.field.clone()));
        params.push(SqlValue::Text(key.field.clone()));
        let terminal = path_key_terminal(i);
        ctes.push((
            format!("_s{i}"),
            format!(
                "SELECT uuid, present, grp, vnum, vtext, vblob FROM ( \
                   SELECT _res.uuid AS uuid, \
                          CASE WHEN field.metarecord_uuid IS NULL THEN 0 ELSE 1 END AS present, \
                          {grp} AS grp, {num} AS vnum, {text} AS vtext, {blob} AS vblob, \
                          ROW_NUMBER() OVER (PARTITION BY _res.uuid \
                              ORDER BY {grp} {dir}, {num} {dir}, {text} {dir}, {blob} {dir}) \
                              AS rn \
                   FROM _res LEFT JOIN field \
                     ON field.metarecord_uuid = _res.uuid \
                        AND field.field_name = ? AND field.value_type != 'nothing' \
                   LEFT JOIN _p{i} ON {terminal} \
                 ) WHERE rn = 1"
            ),
        ));
        // `_s{i}`'s two `?`: the `field` join's name, then `terminal`'s.
        params.push(SqlValue::Text(key.field.clone()));
        params.push(SqlValue::Text(key.field.clone()));

        // `_s0` is the driver; later keys join 1:1 on uuid (same uuid set).
        if i > 0 {
            joins.push_str(&format!(" LEFT JOIN _s{i} ON _s{i}.uuid = _s0.uuid"));
        }
        select_cols.push_str(&format!(
            ", CASE WHEN _s{i}.present = 0 THEN 1 ELSE 0 END AS nf{i}, \
               COALESCE(_s{i}.grp, -1) AS g{i}, COALESCE(_s{i}.vnum, {NUM_SENTINEL}) AS n{i}, \
               COALESCE(_s{i}.vtext, '') AS t{i}, COALESCE(_s{i}.vblob, x'') AS b{i}"
        ));
        // Metarecords without the sort field always come last, whatever `order`.
        order_by.push(format!("nf{i} ASC"));
        components.push((format!("nf{i}"), true));
        let asc = key.order == SortOrder::Asc;
        for col in ["g", "n", "t", "b"] {
            order_by.push(format!("{col}{i} {dir}"));
            components.push((format!("{col}{i}"), asc));
        }
    }
    order_by.push("uuid ASC".to_string());
    components.push(("uuid".to_string(), true));

    // Keyset resumption: skip everything up to and including the cursor row.
    let mut where_clause = String::new();
    if let Some(token) = cursor {
        let parsed = pagination::decode(token, hash)?;
        let values = cursor_values(&parsed, sort.len())?;
        where_clause = format!(" WHERE {}", keyset_predicate(&components, &values, &mut params));
    }

    let cte_sql: Vec<String> = ctes
        .into_iter()
        .map(|(name, body)| {
            // Force materialisation of the filtered universe: it is joined
            // to `field` once per sort CTE (and is the driver when there is
            // no sort), and re-evaluating it per reference (SQLite's default
            // for an inlined view) re-runs the whole filter each time —
            // catastrophic when the filter is itself a tree walk.
            let hint = if name == "_res" { " MATERIALIZED" } else { "" };
            format!("{name} AS{hint} ({body})")
        })
        .collect();
    let mut sql = format!(
        "WITH RECURSIVE {} SELECT * FROM (SELECT {select_cols} FROM {driver}{joins}){where_clause} ORDER BY {}",
        cte_sql.join(", "),
        order_by.join(", ")
    );
    if let Some(limit) = limit {
        sql.push_str(" LIMIT ?");
        params.push(SqlValue::Integer(limit as i64 + 1));
    }

    // Execute; keep each row's key components to build the next cursor from
    // the last *returned* row (the lookahead row is discarded).
    let mut stmt = conn.prepare(&sql).map_err(anyhow::Error::from)?;
    let mut rows =
        stmt.query(rusqlite::params_from_iter(params.iter())).map_err(anyhow::Error::from)?;
    let mut page: Vec<(Uuid, Vec<serde_json::Value>)> = Vec::new();
    while let Some(row) = rows.next().map_err(anyhow::Error::from)? {
        let uuid = db::bytes_to_uuid(row.get::<_, Vec<u8>>(0).map_err(anyhow::Error::from)?)?;
        let mut keys = Vec::new();
        if limit.is_some() {
            for c in 0..(5 * sort.len()) {
                // Component layout per sort key: nf, g (ints), n (real,
                // IEEE-754 bits hex-encoded), t (text), b (blob, hex-encoded).
                let col = c + 1;
                let v = match c % 5 {
                    0 | 1 => {
                        serde_json::json!(row.get::<_, i64>(col).map_err(anyhow::Error::from)?)
                    }
                    2 => float_to_cursor(row.get::<_, f64>(col).map_err(anyhow::Error::from)?),
                    3 => {
                        serde_json::json!(row.get::<_, String>(col).map_err(anyhow::Error::from)?)
                    }
                    _ => serde_json::json!(hex_encode(
                        &row.get::<_, Vec<u8>>(col).map_err(anyhow::Error::from)?
                    )),
                };
                keys.push(v);
            }
        }
        page.push((uuid, keys));
    }

    match limit {
        None => Ok((page.into_iter().map(|(u, _)| u).collect(), None)),
        Some(limit) => {
            let has_more = page.len() > limit;
            page.truncate(limit);
            let next = if has_more && !page.is_empty() {
                let (last_uuid, keys) = page.last().expect("non-empty page");
                Some(pagination::encode(&Cursor {
                    keys: keys.clone(),
                    uuid: last_uuid.as_simple().to_string(),
                    h: hash,
                }))
            } else {
                None
            };
            Ok((page.into_iter().map(|(u, _)| u).collect(), next))
        }
    }
}

/// Converts the JSON cursor key components back into typed SQL values, in
/// component order (5 per sort key, then the metarecord UUID).
fn cursor_values(cursor: &Cursor, n_sort: usize) -> Result<Vec<SqlValue>, ApiError> {
    let invalid = || ApiError::bad_request("invalid cursor");
    if cursor.keys.len() != 5 * n_sort {
        return Err(invalid());
    }
    let mut values = Vec::with_capacity(cursor.keys.len() + 1);
    for (i, key) in cursor.keys.iter().enumerate() {
        let v = match i % 5 {
            0 | 1 => SqlValue::Integer(key.as_i64().ok_or_else(invalid)?),
            2 => SqlValue::Real(float_from_cursor(key)?),
            3 => SqlValue::Text(key.as_str().ok_or_else(invalid)?.to_string()),
            _ => SqlValue::Blob(hex_decode(key.as_str().ok_or_else(invalid)?)?),
        };
        values.push(v);
    }
    values.push(SqlValue::Blob(db::uuid_to_bytes(cursor.last_uuid()?)));
    Ok(values)
}

/// Builds the strict "row is after the cursor" predicate:
/// `(c0 > v0 OR (c0 = v0 AND (c1 > v1 OR ...)))` with per-component
/// direction. Parameters are appended in text order.
fn keyset_predicate(
    components: &[(String, bool)],
    values: &[SqlValue],
    params: &mut Vec<SqlValue>,
) -> String {
    fn build(
        components: &[(String, bool)],
        values: &[SqlValue],
        params: &mut Vec<SqlValue>,
        i: usize,
    ) -> String {
        let (name, asc) = &components[i];
        let op = if *asc { ">" } else { "<" };
        params.push(values[i].clone());
        if i == components.len() - 1 {
            format!("{name} {op} ?")
        } else {
            params.push(values[i].clone());
            let rest = build(components, values, params, i + 1);
            format!("({name} {op} ? OR ({name} = ? AND {rest}))")
        }
    }
    build(components, values, params, 0)
}

use metafolder_core::hex::encode as hex_encode;

/// The metarecord uuids whose `field` `tree_ref` name contains `term`
/// (case-insensitive), trigram-pre-filtered when `term` is ≥ 3 chars. Shared by
/// the SQL OSM `Path` engine and, so the bitmap index can accelerate a
/// single-term OSM path, by `run_query_filter` — which resolves these "term
/// nodes" and hands them to the index as the seeds of a subtree expansion (the
/// index has no substring-of-name index of its own), mirroring how it resolves
/// `Path` targets to root metarecords.
pub fn osm_name_nodes(conn: &Connection, field: &str, term: &str) -> Result<Vec<Uuid>, ApiError> {
    let pattern = format!("(?i){}", regex::escape(term));
    let collect = |sql: &str, params: &[&dyn rusqlite::ToSql]| -> Result<Vec<Uuid>, ApiError> {
        let mut stmt = conn.prepare(sql).map_err(anyhow::Error::from)?;
        let rows =
            stmt.query_map(params, |row| row.get::<_, Vec<u8>>(0)).map_err(anyhow::Error::from)?;
        let mut out = Vec::new();
        for bytes in rows {
            out.push(db::bytes_to_uuid(bytes.map_err(anyhow::Error::from)?)?);
        }
        Ok(out)
    };
    if term.chars().count() >= 3 {
        let phrase = crate::fts::match_phrase(term);
        collect(
            "SELECT DISTINCT metarecord_uuid FROM field \
             WHERE field_name = ?1 AND value_type = 'tree_ref' \
               AND id IN (SELECT rowid FROM field_text WHERE text MATCH ?2) \
               AND value_name REGEXP ?3",
            &[&field, &phrase, &pattern],
        )
    } else {
        collect(
            "SELECT DISTINCT metarecord_uuid FROM field \
             WHERE field_name = ?1 AND value_type = 'tree_ref' AND value_name REGEXP ?2",
            &[&field, &pattern],
        )
    }
}

/// Every metarecord with a `tree_ref` value in `field` (the unpruned candidate
/// set for an all-short-terms or empty OSM path query).
fn all_tree_ref_nodes(conn: &Connection, field: &str) -> Result<Vec<Uuid>, ApiError> {
    let mut stmt = conn
        .prepare(
            "SELECT DISTINCT metarecord_uuid FROM field \
             WHERE field_name = ?1 AND value_type = 'tree_ref'",
        )
        .map_err(anyhow::Error::from)?;
    let rows =
        stmt.query_map([field], |row| row.get::<_, Vec<u8>>(0)).map_err(anyhow::Error::from)?;
    let mut out = Vec::new();
    for bytes in rows {
        out.push(db::bytes_to_uuid(bytes.map_err(anyhow::Error::from)?)?);
    }
    Ok(out)
}

/// The metarecords matching an OSM `Path` query, as a plain uuid list — WITHOUT
/// the SQL `VALUES` inlining the query engine would wrap around them, which on a
/// large result set is a multi-megabyte statement to build and parse. Shared by
/// the SQL engine (`Compiler::osm_path`, which still inlines to form a CTE) and
/// by `resolve_index_leaves`, which hands the result to the bitmap index as a
/// `UuidIn` (so a multi-term path search composes with the rest of the query and
/// gets an O(1) count). Candidate pruning is by the ≥3-char terms' name nodes
/// and their subtrees; a single ≥3-char term is exact without the per-path check
/// (every candidate lies under a node whose name holds the term), while
/// multi-term (order-sensitive) and all-short-term queries verify the assembled
/// path. Rejects a non-`tree_ref` field with 400, like the engine.
/// What a field holds, for [`crate::query_validate::validate_query_types`]:
/// `tree_ref` when the field is a forest, else any other type it carries,
/// `None` for a field with no data. The SQL side's answer to the question the
/// index answers from its `types` map.
pub fn stored_type(conn: &Connection, field: &str) -> Result<Option<String>, ApiError> {
    let mut stmt = conn
        .prepare_cached("SELECT DISTINCT value_type FROM field WHERE field_name = ?1")
        .map_err(anyhow::Error::from)?;
    let types =
        stmt.query_map([field], |row| row.get::<_, String>(0)).map_err(anyhow::Error::from)?;
    let mut other = None;
    for value_type in types {
        let value_type = value_type.map_err(anyhow::Error::from)?;
        if value_type == "tree_ref" {
            return Ok(Some(value_type));
        }
        if value_type != "nothing" {
            other = Some(value_type);
        }
    }
    Ok(other)
}

/// The metarecords whose assembled `field` path matches `terms` in order.
///
/// The result is **sorted**, and that is part of the contract, not a detail of
/// how it was computed. This set is inlined verbatim into a `UuidIn` leaf — by
/// [`resolve_index_leaves`] for the index, by the compiler for SQL — and
/// `resolve_index_leaves` runs again on *every page* of a paginated query. The
/// index binds its cursor to a hash of the query it was handed, so a set whose
/// order drifts between two calls makes page 2 look like a cursor from some
/// other query: the index defers, the SQL engine is passed a cursor it cannot
/// decode, and a two-word search dies on "invalid cursor" halfway through.
/// Three of the four ways out of this function build their answer in a
/// `HashSet`, whose iteration order differs between instances — hence the sort
/// at the one place every path goes through.
pub fn osm_path_matches(
    conn: &Connection,
    cache: &mut TreeCache,
    field: &str,
    terms: &[String],
) -> Result<Vec<Uuid>, ApiError> {
    let mut matched = osm_path_matches_unordered(conn, cache, field, terms)?;
    matched.sort_unstable();
    Ok(matched)
}

fn osm_path_matches_unordered(
    conn: &Connection,
    cache: &mut TreeCache,
    field: &str,
    terms: &[String],
) -> Result<Vec<Uuid>, ApiError> {
    // A blank query matches every metarecord with a path in this forest.
    if terms.is_empty() {
        return all_tree_ref_nodes(conn, field);
    }
    // The production path: one walk of the resident forest, no SQL at all.
    if let Some(matched) = cache.osm_path_matches(field, terms)? {
        return Ok(matched);
    }
    // Pruning by node *name* is only sound for a term that fits inside one
    // segment: a term containing the separator can only match across segments,
    // so no name contains it and it would prune everything away.
    let prunable = |t: &&String| t.chars().count() >= 3 && !t.contains('/');
    let mut candidates: Option<std::collections::HashSet<Uuid>> = None;
    for term in terms.iter().filter(prunable) {
        let mut reachable = std::collections::HashSet::new();
        for node in osm_name_nodes(conn, field, term)? {
            reachable.insert(node);
            for desc in cache.descendants(conn, field, node)? {
                reachable.insert(desc);
            }
        }
        candidates = Some(match candidates.take() {
            None => reachable,
            Some(prev) => &prev & &reachable,
        });
        if candidates.as_ref().is_some_and(|c| c.is_empty()) {
            return Ok(Vec::new());
        }
    }
    // A single ≥3-char, separator-free term needs no ordered check: every
    // candidate lies under a node whose name contains the term, so its path
    // contains the term.
    if matches!(terms, [only] if prunable(&only)) {
        return Ok(candidates.map(|s| s.into_iter().collect()).unwrap_or_default());
    }
    // Otherwise verify the ordered match on the real path. With no ≥3-char term
    // nothing was pruned, so every path-bearing metarecord is a candidate.
    let candidates: Vec<Uuid> = match candidates {
        Some(set) => set.into_iter().collect(),
        None => all_tree_ref_nodes(conn, field)?,
    };
    let mut matched = Vec::new();
    for uuid in candidates {
        for path in cache.paths_of(conn, field, uuid)? {
            if metafolder_core::query::osm_ordered_match(&path.to_lowercase(), terms) {
                matched.push(uuid);
                break;
            }
        }
    }
    Ok(matched)
}

/// The case-insensitive ordered-substring regex for OSM `Direct` mode:
/// `["con", "def"]` → `(?i)con.*def`. Terms are regex-escaped; empty `terms`
/// yields `(?i)`, which matches any string ("present" semantics). Unanchored
/// (`REGEXP` searches), so this is exactly "con then def, non-overlapping".
pub(crate) fn osm_regex(terms: &[String]) -> String {
    let body = terms.iter().map(|t| regex::escape(t)).collect::<Vec<_>>().join(".*");
    format!("(?i){body}")
}

fn hex_decode(s: &str) -> Result<Vec<u8>, ApiError> {
    if !s.len().is_multiple_of(2) {
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

/// Encodes a float sort-key value into the cursor as the hex of its raw
/// IEEE-754 bits (not a JSON number). serde_json's default float parser is not
/// correctly rounded, so a decimal encoding can come back off by 1 ULP, which
/// at a page boundary duplicates or skips a row; the bit form round-trips
/// exactly, like the blob component.
fn float_to_cursor(f: f64) -> serde_json::Value {
    serde_json::json!(hex_encode(&f.to_bits().to_be_bytes()))
}

/// Inverse of [`float_to_cursor`].
fn float_from_cursor(key: &serde_json::Value) -> Result<f64, ApiError> {
    let invalid = || ApiError::bad_request("invalid cursor");
    let bytes = hex_decode(key.as_str().ok_or_else(invalid)?)?;
    let arr: [u8; 8] = bytes.as_slice().try_into().map_err(|_| invalid())?;
    Ok(f64::from_bits(u64::from_be_bytes(arr)))
}

// ── Compiler ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CmpOp {
    Eq,
    Lt,
    Lte,
    Gt,
    Gte,
}

impl CmpOp {
    fn symbol(&self) -> &'static str {
        match self {
            CmpOp::Eq => "=",
            CmpOp::Lt => "<",
            CmpOp::Lte => "<=",
            CmpOp::Gt => ">",
            CmpOp::Gte => ">=",
        }
    }

    fn is_ordered(&self) -> bool {
        !matches!(self, CmpOp::Eq)
    }
}

struct Compiler<'a> {
    conn: &'a Connection,
    cache: &'a mut TreeCache,
    ctes: Vec<(String, String)>,
    params: Vec<SqlValue>,
    counter: usize,
}

impl<'a> Compiler<'a> {
    /// The `_repo` CTE (declared first) holds the universe: every metarecord of
    /// the repository (one repository per database file). It both isolates
    /// results and serves as the complement base for `Not`.
    fn new(conn: &'a Connection, cache: &'a mut TreeCache) -> Self {
        let ctes = vec![("_repo".to_string(), "SELECT uuid FROM metarecord".to_string())];
        Self { conn, cache, ctes, params: Vec::new(), counter: 0 }
    }

    /// Runs a sub-query on its own (a fresh compiler and statement) and
    /// returns the matching UUIDs, repo-filtered like a top-level query.
    /// Used by the hybrid `FollowsTransitive` execution, whose tree-cache
    /// walk needs the root set before SQL generation.
    fn execute_condition(&mut self, cond: &Query) -> Result<Vec<Uuid>, ApiError> {
        let mut sub = Compiler::new(self.conn, self.cache);
        let last = sub.compile_node(cond)?;
        let Compiler { ctes, params, .. } = sub;
        let cte_sql: Vec<String> =
            ctes.into_iter().map(|(name, body)| format!("{name} AS ({body})")).collect();
        let sql = format!(
            "WITH {} SELECT uuid FROM {last} WHERE uuid IN (SELECT uuid FROM _repo)",
            cte_sql.join(", ")
        );
        let mut stmt = self.conn.prepare(&sql).map_err(anyhow::Error::from)?;
        let mut rows =
            stmt.query(rusqlite::params_from_iter(params.iter())).map_err(anyhow::Error::from)?;
        let mut uuids = Vec::new();
        while let Some(row) = rows.next().map_err(anyhow::Error::from)? {
            uuids.push(db::bytes_to_uuid(row.get::<_, Vec<u8>>(0).map_err(anyhow::Error::from)?)?);
        }
        Ok(uuids)
    }

    fn fresh(&mut self) -> String {
        let name = format!("_q{}", self.counter);
        self.counter += 1;
        name
    }

    fn add(&mut self, body: String) -> String {
        let name = self.fresh();
        self.ctes.push((name.clone(), body));
        name
    }

    fn empty(&mut self) -> String {
        self.add("SELECT x'' AS uuid WHERE 0".to_string())
    }

    fn push_text(&mut self, s: &str) {
        self.params.push(SqlValue::Text(s.to_string()));
    }

    /// `Matches` (regex), in the two shapes it takes: a `Path` aspect is
    /// answered from the tree cache and inlined, anything else is a REGEXP scan
    /// narrowed by the FTS5 trigram pre-filter.
    fn matches(&mut self, field: &str, pattern: &str, aspect: Aspect) -> Result<String, ApiError> {
        crate::regexp::compile(pattern)
            .map_err(|e| ApiError::bad_request(format!("invalid regex pattern: {e}")))?;
        if aspect == Aspect::Path {
            // Hybrid, like osm path mode: the assembled paths are built
            // through the tree cache and the matching uuids inlined.
            let re = crate::regexp::compile(pattern)
                .map_err(|e| ApiError::bad_request(format!("invalid regex pattern: {e}")))?;
            let matched = self.path_matches(field, &|path: &str| re.is_match(path))?;
            return self.inline_uuids(matched);
        }
        // Trigram pre-filter (spec-query "MATCHES via FTS5"): when every
        // match must contain a literal substring (≥ 3 chars), restrict
        // the REGEXP scan to the rows the FTS index reports containing it
        // (`id IN (… field_text … MATCH …)`). A sound over-approximation
        // — REGEXP still re-checks every surviving row, so the result is
        // identical to the full scan. (Driving from the FTS via a JOIN
        // was measured *slower* once wrapped in the repo-isolation CTE,
        // so the membership test is kept as the spec describes.)
        self.push_text(field);
        let prefilter = match crate::fts::required_fts_literal(pattern) {
            Some(literal) => {
                self.push_text(&crate::fts::match_phrase(&literal));
                "id IN (SELECT rowid FROM field_text WHERE text MATCH ?) AND "
            }
            None => "",
        };
        self.push_text(pattern);
        self.push_text(pattern);
        Ok(self.add(format!(
            "SELECT DISTINCT metarecord_uuid AS uuid FROM field \
                 WHERE field_name = ? AND {prefilter}\
                   ((value_type = 'string' AND value_text REGEXP ?) OR \
                    (value_type = 'tree_ref' AND value_name REGEXP ?))"
        )))
    }

    /// `SameAs`: the rows of `field` whose value tuple also occurs among the
    /// `field` rows of the target set.
    ///
    /// The whole tuple is compared with `IS` (NULL-safe equality) under an equal
    /// `value_type`, so one statement covers every value type — the unused
    /// columns are NULL on both sides and compare equal. `Nothing` rows are
    /// excluded on both sides: sharing an absence is not sharing a value.
    fn same_as(&mut self, field: &str, target: &Query) -> Result<String, ApiError> {
        // The sub-query is compiled first so its `?` placeholders are
        // pushed before this node's, matching the order they appear in
        // the assembled SQL.
        let sub = self.compile_node(target)?;
        self.push_text(field);
        self.push_text(field);
        Ok(self.add(format!(
            "SELECT DISTINCT a.metarecord_uuid AS uuid FROM field a \
                 WHERE a.field_name = ? AND a.value_type != 'nothing' \
                   AND EXISTS (SELECT 1 FROM field b \
                                WHERE b.field_name = ? AND b.value_type != 'nothing' \
                                  AND b.metarecord_uuid IN (SELECT uuid FROM {sub}) \
                                  AND b.value_type = a.value_type \
                                  AND b.value_text IS a.value_text \
                                  AND b.value_int IS a.value_int \
                                  AND b.value_real IS a.value_real \
                                  AND b.value_uuid IS a.value_uuid \
                                  AND b.value_ref_repo IS a.value_ref_repo \
                                  AND b.value_name_bytes IS a.value_name_bytes)"
        )))
    }

    /// `Follows` (`->`): the metarecords whose `field` points at the target —
    /// every match of a sub-query, or the single node at a path.
    fn follows(&mut self, field: &str, target: &FollowTarget) -> Result<String, ApiError> {
        match target {
            FollowTarget::Condition(cond) => {
                let sub = self.compile_node(cond)?;
                self.push_text(field);
                Ok(self.add(format!(
                    "SELECT DISTINCT metarecord_uuid AS uuid FROM field \
                     WHERE field_name = ? AND value_type IN ('ref', 'tree_ref') \
                       AND value_uuid IN (SELECT uuid FROM {sub})"
                )))
            }
            FollowTarget::Path(path) => {
                let conn = self.conn;
                let target = self.cache.resolve_path(conn, field, path)?;
                match target {
                    None => Ok(self.empty()),
                    Some(uuid) => {
                        self.push_text(field);
                        self.params.push(SqlValue::Blob(db::uuid_to_bytes(uuid)));
                        Ok(self.add(
                            "SELECT DISTINCT metarecord_uuid AS uuid FROM field \
                             WHERE field_name = ? AND value_type = 'tree_ref' \
                               AND value_uuid = ?"
                                .to_string(),
                        ))
                    }
                }
            }
        }
    }

    /// `FollowsTransitive` (`->*` / `=>*`): the descendants — and, when the
    /// query is inclusive, the roots themselves — of a node set in `field`'s
    /// forest.
    fn follows_transitive(
        &mut self,
        field: &str,
        target: &FollowTarget,
        inclusive: bool,
    ) -> Result<String, ApiError> {
        // Hybrid execution: the root set (one path-resolved metarecord,
        // or every match of the condition sub-query) and its
        // descendants are collected through the tree cache, then
        // injected as inline literals (no bound parameter limit).
        // Only TreeRef trees have descendants; on a Ref field this
        // matches nothing by construction. When `inclusive` (DSL `=>*`),
        // the roots themselves are part of the result (whole subtree).
        let conn = self.conn;
        let roots = match target {
            FollowTarget::Path(path) => match self.cache.resolve_path(conn, field, path)? {
                None => Vec::new(),
                Some(uuid) => vec![uuid],
            },
            FollowTarget::Condition(cond) => self.execute_condition(cond)?,
        };
        // The inclusive form keeps the roots only on an actual tree_ref
        // forest — on a ref field FollowsTransitive matches nothing
        // (TreeRef-only), matching the bitmap index's `supports_transitive`
        // gate. `descendants` is already empty for a non-forest field.
        let include_roots = inclusive && self.field_is_tree_ref(field)?;
        let mut descendants = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for root in roots {
            if include_roots && seen.insert(root) {
                descendants.push(root);
            }
            for d in self.cache.descendants(conn, field, root)? {
                if seen.insert(d) {
                    descendants.push(d);
                }
            }
        }
        if descendants.is_empty() {
            return Ok(self.empty());
        }
        let literals: Vec<String> =
            descendants.iter().map(|u| format!("(x'{}')", hex_encode(u.as_bytes()))).collect();
        Ok(self.add(format!("SELECT column1 AS uuid FROM (VALUES {})", literals.join(","))))
    }

    fn compile_node(&mut self, q: &Query) -> Result<String, ApiError> {
        match q {
            Query::IsPresent { field, aspect } => self.presence(field, *aspect, true),
            Query::IsAbsent { field, aspect } => self.presence(field, *aspect, false),
            Query::IsUnknown { field } => {
                self.push_text(field);
                Ok(self.add(
                    "SELECT uuid FROM _repo WHERE uuid NOT IN \
                     (SELECT metarecord_uuid FROM field WHERE field_name = ?)"
                        .to_string(),
                ))
            }

            Query::Eq { field, value, aspect } => self.comparison(field, value, CmpOp::Eq, *aspect),
            Query::Lt { field, value, aspect } => self.comparison(field, value, CmpOp::Lt, *aspect),
            Query::Lte { field, value, aspect } => {
                self.comparison(field, value, CmpOp::Lte, *aspect)
            }
            Query::Gt { field, value, aspect } => self.comparison(field, value, CmpOp::Gt, *aspect),
            Query::Gte { field, value, aspect } => {
                self.comparison(field, value, CmpOp::Gte, *aspect)
            }
            Query::Neq { field, value, aspect } => {
                // At least one non-Nothing occurrence differing from `value`
                // (a different value type counts as differing).
                if *aspect == Aspect::Path {
                    // The path is assembled outside SQL, so the negation is
                    // taken there too. Like every `Neq`, it asks for at least
                    // one *differing* occurrence — not the complement of `Eq`:
                    // a multi-valued node holding one matching and one differing
                    // path satisfies both.
                    let matched = self.path_comparison_differs(field, value)?;
                    return self.inline_uuids(matched);
                }
                self.push_text(field);
                let pred = self.scalar_predicate(field, value, CmpOp::Eq, *aspect)?;
                Ok(self.add(format!(
                    "SELECT DISTINCT metarecord_uuid AS uuid FROM field \
                     WHERE field_name = ? AND value_type != 'nothing' AND NOT ({pred})"
                )))
            }

            Query::And { operands } => self.combine(operands, "INTERSECT"),
            Query::Or { operands } => self.combine(operands, "UNION"),
            Query::Not { operand } => {
                let sub = self.compile_node(operand)?;
                Ok(self.add(format!("SELECT uuid FROM _repo EXCEPT SELECT uuid FROM {sub}")))
            }

            Query::Matches { field, pattern, aspect } => self.matches(field, pattern, *aspect),

            Query::Osm { field, terms, mode } => match mode {
                OsmMode::Direct => self.osm_direct(field, terms),
                OsmMode::Path => self.osm_path(field, terms),
            },

            Query::SameAs { field, target } => self.same_as(field, target),

            Query::Follows { field, target } => self.follows(field, target),

            Query::FollowsTransitive { field, target, inclusive } => {
                self.follows_transitive(field, target, *inclusive)
            }

            Query::UuidIn { uuids } => {
                if uuids.is_empty() {
                    return Ok(self.empty());
                }
                // Inline the uuids as literals (no bound-parameter limit) and
                // intersect with `_repo` so non-owned / unknown uuids drop out.
                let literals: Vec<String> =
                    uuids.iter().map(|u| format!("(x'{}')", hex_encode(u.as_bytes()))).collect();
                Ok(self.add(format!(
                    "SELECT uuid FROM _repo WHERE uuid IN (SELECT column1 FROM (VALUES {}))",
                    literals.join(",")
                )))
            }
        }
    }

    /// OSM `Direct` mode: an ordered-substring match over the field row's own
    /// text (`value_text` / `value_name`). Like `Matches`, the `REGEXP` scan is
    /// pre-filtered by the FTS5 trigram index on the ≥ 3-char terms (a sound
    /// over-approximation; `REGEXP` re-checks order).
    fn osm_direct(&mut self, field: &str, terms: &[String]) -> Result<String, ApiError> {
        let pattern = osm_regex(terms);
        self.push_text(field);
        let long: Vec<String> = terms
            .iter()
            .filter(|t| t.chars().count() >= 3)
            .map(|t| crate::fts::match_phrase(t))
            .collect();
        let prefilter = if long.is_empty() {
            ""
        } else {
            self.push_text(&long.join(" "));
            "id IN (SELECT rowid FROM field_text WHERE text MATCH ?) AND "
        };
        self.push_text(&pattern);
        self.push_text(&pattern);
        Ok(self.add(format!(
            "SELECT DISTINCT metarecord_uuid AS uuid FROM field \
             WHERE field_name = ? AND {prefilter}\
               ((value_type = 'string' AND value_text REGEXP ?) OR \
                (value_type = 'tree_ref' AND value_name REGEXP ?))"
        )))
    }

    /// OSM `Path` mode (TreeRef only): an ordered-substring match over the
    /// assembled path `seg1/…/segN`. Hybrid, like `FollowsTransitive` — the
    /// matching metarecords are computed through the tree cache and inlined as
    /// literals. Candidate pruning: for each ≥ 3-char term, the nodes whose
    /// *name* contains it (trigram-indexed) expanded to descendants-or-self;
    /// their intersection is a superset of the matches (each term lies within
    /// one segment), verified by the final ordered check on the real path.
    fn osm_path(&mut self, field: &str, terms: &[String]) -> Result<String, ApiError> {
        if terms.is_empty() {
            // A blank query matches every path-bearing metarecord: a direct
            // scan, no inlining.
            self.push_text(field);
            return Ok(self.add(
                "SELECT DISTINCT metarecord_uuid AS uuid FROM field \
                 WHERE field_name = ? AND value_type = 'tree_ref'"
                    .to_string(),
            ));
        }
        // The matching uuids (pruned + order-verified) computed without the
        // VALUES inlining, then wrapped in a VALUES CTE for the engine.
        let matched = osm_path_matches(self.conn, self.cache, field, terms)?;
        if matched.is_empty() {
            return Ok(self.empty());
        }
        let literals: Vec<String> =
            matched.iter().map(|u| format!("(x'{}')", hex_encode(u.as_bytes()))).collect();
        Ok(self.add(format!("SELECT column1 AS uuid FROM (VALUES {})", literals.join(","))))
    }

    fn field_is_tree_ref(&self, field: &str) -> Result<bool, ApiError> {
        Ok(self
            .conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM field \
                 WHERE field_name = ?1 AND value_type = 'tree_ref')",
                [field],
                |row| row.get::<_, bool>(0),
            )
            .map_err(anyhow::Error::from)?)
    }

    /// `IsPresent` / `IsAbsent`, aspect-aware. Under `parent` the question is
    /// whether the node has a real parent: a forest root's parent is the root
    /// sentinel, so `field:parent IS ABSENT` is the predicate form of "is a
    /// root" — the one the `Follows` arrow cannot express.
    fn presence(&mut self, field: &str, aspect: Aspect, present: bool) -> Result<String, ApiError> {
        if aspect == Aspect::Parent {
            self.push_text(field);
            self.params.push(SqlValue::Blob(db::uuid_to_bytes(Uuid::nil())));
            let sym = if present { "!=" } else { "=" };
            return Ok(self.add(format!(
                "SELECT DISTINCT metarecord_uuid AS uuid FROM field \
                 WHERE field_name = ? AND value_type = 'tree_ref' AND value_uuid {sym} ?"
            )));
        }
        self.push_text(field);
        let sym = if present { "!=" } else { "=" };
        Ok(self.add(format!(
            "SELECT DISTINCT metarecord_uuid AS uuid FROM field \
             WHERE field_name = ? AND value_type {sym} 'nothing'"
        )))
    }

    /// Wraps a computed UUID set as a `VALUES` CTE (the hybrid shape shared
    /// with `osm_path` and `FollowsTransitive`).
    fn inline_uuids(&mut self, uuids: Vec<Uuid>) -> Result<String, ApiError> {
        if uuids.is_empty() {
            return Ok(self.empty());
        }
        let literals: Vec<String> =
            uuids.iter().map(|u| format!("(x'{}')", hex_encode(u.as_bytes()))).collect();
        Ok(self.add(format!("SELECT column1 AS uuid FROM (VALUES {})", literals.join(","))))
    }

    /// The field's tree nodes whose *assembled path* satisfies `pred`. Hybrid,
    /// like osm path mode: the paths come from the tree cache, never from SQL.
    /// A multi-valued node matches when any of its paths does.
    fn path_matches(
        &mut self,
        field: &str,
        pred: &dyn Fn(&str) -> bool,
    ) -> Result<Vec<Uuid>, ApiError> {
        let mut matched = Vec::new();
        for uuid in all_tree_ref_nodes(self.conn, field)? {
            for path in self.cache.paths_of(self.conn, field, uuid)? {
                if pred(&path) {
                    matched.push(uuid);
                    break;
                }
            }
        }
        Ok(matched)
    }

    /// `path_matches` for a comparison against a string operand.
    fn path_comparison_matches(
        &mut self,
        field: &str,
        value: &Value,
        op: CmpOp,
    ) -> Result<Vec<Uuid>, ApiError> {
        let Value::String(operand) = value else {
            return Err(ApiError::bad_request(format!(
                "the ':path' aspect compares against a string, got {}",
                db::encode_value(value).value_type
            )));
        };
        let operand = operand.clone();
        self.path_matches(field, &move |path: &str| match op {
            CmpOp::Eq => path == operand,
            CmpOp::Lt => path < operand.as_str(),
            CmpOp::Lte => path <= operand.as_str(),
            CmpOp::Gt => path > operand.as_str(),
            CmpOp::Gte => path >= operand.as_str(),
        })
    }

    /// The `Neq` counterpart of [`Self::path_comparison_matches`]: the nodes
    /// holding at least one path *different* from the operand.
    fn path_comparison_differs(
        &mut self,
        field: &str,
        value: &Value,
    ) -> Result<Vec<Uuid>, ApiError> {
        let Value::String(operand) = value else {
            return Err(ApiError::bad_request(format!(
                "the ':path' aspect compares against a string, got {}",
                db::encode_value(value).value_type
            )));
        };
        let operand = operand.clone();
        self.path_matches(field, &move |path: &str| path != operand)
    }

    fn comparison(
        &mut self,
        field: &str,
        value: &Value,
        op: CmpOp,
        aspect: Aspect,
    ) -> Result<String, ApiError> {
        if aspect == Aspect::Path {
            let matched = self.path_comparison_matches(field, value, op)?;
            return self.inline_uuids(matched);
        }
        self.push_text(field);
        let pred = self.scalar_predicate(field, value, op, aspect)?;
        Ok(self.add(format!(
            "SELECT DISTINCT metarecord_uuid AS uuid FROM field \
             WHERE field_name = ? AND ({pred})"
        )))
    }

    fn combine(&mut self, operands: &[Query], set_op: &str) -> Result<String, ApiError> {
        if operands.is_empty() {
            return Err(ApiError::bad_request("'and'/'or' need at least one operand"));
        }
        // `check_query_size` already rejected this upfront; kept so the limit
        // still holds for any future caller that compiles without it.
        if operands.len() > MAX_COMBINATOR_OPERANDS {
            return Err(ApiError::bad_request(too_wide_message(operands.len())));
        }
        let mut parts = Vec::with_capacity(operands.len());
        for operand in operands {
            let sub = self.compile_node(operand)?;
            parts.push(format!("SELECT uuid FROM {sub}"));
        }
        Ok(self.add(parts.join(&format!(" {set_op} "))))
    }

    /// Row-level predicate for one comparison operand; pushes its parameters.
    /// `field` is needed for the exact-node path case (below).
    fn scalar_predicate(
        &mut self,
        field: &str,
        value: &Value,
        op: CmpOp,
        aspect: Aspect,
    ) -> Result<String, ApiError> {
        let sym = op.symbol();
        let ordered_only_eq = |type_name: &str| {
            ApiError::bad_request(format!(
                "ordered comparison is not supported on {type_name} values"
            ))
        };
        match value {
            Value::Nothing => Err(ApiError::bad_request(
                "comparisons with 'nothing' are not allowed; use is_absent / is_unknown",
            )),
            // Int and Float compare numerically together.
            Value::Int(n) => {
                self.params.push(SqlValue::Real(*n as f64));
                Ok(format!(
                    "value_type IN ('int', 'float') AND \
                     COALESCE(CAST(value_int AS REAL), value_real) {sym} ?"
                ))
            }
            Value::Float(f) => {
                self.params.push(SqlValue::Real(*f));
                Ok(format!(
                    "value_type IN ('int', 'float') AND \
                     COALESCE(CAST(value_int AS REAL), value_real) {sym} ?"
                ))
            }
            Value::String(text) => {
                // The `parent` aspect compares the TreeRef parent, addressed by
                // the path of the node it must be (spec-query "Field aspects").
                // The same set as `field -> "<path>"`, spelled as a comparison.
                if aspect == Aspect::Parent {
                    let conn = self.conn;
                    let node = self.cache.resolve_path(conn, field, text)?;
                    return Ok(match node {
                        Some(u) => {
                            self.params.push(SqlValue::Blob(db::uuid_to_bytes(u)));
                            "value_type = 'tree_ref' AND value_uuid = ?".to_string()
                        }
                        None => "0".to_string(),
                    });
                }
                // Default (`raw`) equality on a tree_ref field is the *exact
                // node*: the operand is a path, resolved through the tree cache,
                // and identity (metarecord_uuid) is compared — at every depth,
                // a forest root included. On a string field the path never
                // resolves, leaving plain literal equality. Neq compiles as a
                // negated Eq, so `op` is Eq here; `check_aspect` has already
                // refused a bare ordered comparison on a tree_ref.
                if aspect == Aspect::Raw && matches!(op, CmpOp::Eq) {
                    let conn = self.conn;
                    let node = self.cache.resolve_path(conn, field, text)?;
                    self.push_text(text); // the string-field literal branch
                    return Ok(match node {
                        Some(u) => {
                            self.params.push(SqlValue::Blob(db::uuid_to_bytes(u)));
                            "(value_type = 'string' AND value_text = ?) OR \
                             (value_type = 'tree_ref' AND metarecord_uuid = ?)"
                                .to_string()
                        }
                        None => "value_type = 'string' AND value_text = ?".to_string(),
                    });
                }
                // The `value` aspect: the row's own text — `value_text`, or
                // `value_name` (the leaf name) on a tree_ref row.
                self.push_text(text);
                self.push_text(text);
                Ok(format!(
                    "(value_type = 'string' AND value_text {sym} ?) OR \
                     (value_type = 'tree_ref' AND value_name {sym} ?)"
                ))
            }
            Value::DateTime(ms) => {
                // datetime is stored as Unix ms in value_int and compares
                // numerically, but only against other datetime values.
                self.params.push(SqlValue::Integer(*ms));
                Ok(format!("value_type = 'datetime' AND value_int {sym} ?"))
            }
            Value::Bool(b) => {
                if op.is_ordered() {
                    return Err(ordered_only_eq("bool"));
                }
                self.params.push(SqlValue::Integer(*b as i64));
                Ok("value_type = 'bool' AND value_int = ?".to_string())
            }
            Value::Ref(u) => {
                if op.is_ordered() {
                    return Err(ordered_only_eq("ref"));
                }
                self.params.push(SqlValue::Blob(db::uuid_to_bytes(*u)));
                Ok("value_type = 'ref' AND value_uuid = ?".to_string())
            }
            Value::RefBase(u) => {
                if op.is_ordered() {
                    return Err(ordered_only_eq("refbase"));
                }
                self.params.push(SqlValue::Blob(db::uuid_to_bytes(*u)));
                Ok("value_type = 'refbase' AND value_uuid = ?".to_string())
            }
            Value::TreeRef { parent, name } => {
                if op.is_ordered() {
                    return Err(ordered_only_eq("tree_ref"));
                }
                self.params.push(SqlValue::Blob(db::uuid_to_bytes(parent.unwrap_or(ZERO_UUID))));
                self.push_text(name.display().as_ref());
                Ok("value_type = 'tree_ref' AND value_uuid = ? AND value_name = ?".to_string())
            }
            Value::ExternalRef { repo, metarecord } => {
                if op.is_ordered() {
                    return Err(ordered_only_eq("externalref"));
                }
                self.params.push(SqlValue::Blob(db::uuid_to_bytes(*metarecord)));
                self.params.push(SqlValue::Blob(db::uuid_to_bytes(*repo)));
                Ok("value_type = 'externalref' AND value_uuid = ? AND value_ref_repo = ?"
                    .to_string())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // Deterministic sweep of bit patterns (one such value drifts by 1 ULP
        // through a decimal JSON round-trip, which this encoding avoids).
        for i in 0..5000u64 {
            let f = f64::from_bits(i.wrapping_mul(0x9E37_79B9_7F4A_7C15));
            if f.is_finite() {
                cases.push(f);
            }
        }
        for &f in &cases {
            // Through the same JSON serialization the cursor undergoes.
            let value = float_to_cursor(f);
            let json = serde_json::to_vec(&value).unwrap();
            let back = float_from_cursor(&serde_json::from_slice(&json).unwrap()).unwrap();
            assert_eq!(f.to_bits(), back.to_bits(), "diverged at {f}");
        }
    }

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
