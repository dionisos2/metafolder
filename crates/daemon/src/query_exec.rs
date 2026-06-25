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

use metafolder_core::metarecord::{Value, ZERO_UUID};
use metafolder_core::query::{FollowTarget, Query};

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
        Query::And { operands } | Query::Or { operands } => {
            operands.iter().map(node_count).sum()
        }
        Query::Not { operand } => node_count(operand),
        Query::Follows { target, .. } | Query::FollowsTransitive { target, .. } => match target {
            FollowTarget::Condition(c) => node_count(c),
            FollowTarget::Path(_) => 0,
        },
        _ => 0, // leaf predicates
    };
    1 + children
}

/// Rejects an over-large query before compiling it (spec-query "Limits").
fn check_query_size(q: &Query) -> Result<(), ApiError> {
    let n = node_count(q);
    if n > MAX_QUERY_NODES {
        return Err(ApiError::bad_request(format!(
            "query too large ({n} nodes, maximum {MAX_QUERY_NODES}); decompose it into smaller queries"
        )));
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
            operands.iter().try_for_each(validate_query)
        }
        Query::Not { operand } => validate_query(operand),
        Query::Follows { target, .. } | Query::FollowsTransitive { target, .. } => match target {
            FollowTarget::Condition(c) => validate_query(c),
            FollowTarget::Path(_) => Ok(()),
        },
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

/// Counts the matching metarecords without fetching them: the same CTE chain
/// as `execute`, wrapped in a `COUNT(*)` (no sort CTEs, no pagination).
pub fn count(
    conn: &Connection,
    cache: &mut TreeCache,
    db_id: Uuid,
    query: &Query,
) -> Result<usize, ApiError> {
    check_query_size(query)?;
    validate_query(query)?;
    let mut compiler = Compiler::new(conn, cache, db_id);
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
    db_id: Uuid,
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

    // The cursor is bound to the exact (query, sort) pair that produced it.
    let hash = pagination::context_hash(&[
        "query",
        &db_id.as_simple().to_string(),
        &serde_json::to_string(query).map_err(|e| ApiError::internal(e.to_string()))?,
        &serde_json::to_string(sort).map_err(|e| ApiError::internal(e.to_string()))?,
    ]);

    let mut compiler = Compiler::new(conn, cache, db_id);
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
        let text = "CASE WHEN field.value_type = 'string' THEN field.value_text \
             WHEN field.value_type = 'tree_ref' THEN field.value_name END";
        let blob = "CASE WHEN field.value_type IN ('ref', 'refbase', 'externalref') \
             THEN field.value_uuid END";
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
                 ) WHERE rn = 1"
            ),
        ));
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

    let cte_sql: Vec<String> =
        ctes
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
        "WITH {} SELECT * FROM (SELECT {select_cols} FROM {driver}{joins}){where_clause} ORDER BY {}",
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
    let mut rows = stmt
        .query(rusqlite::params_from_iter(params.iter()))
        .map_err(anyhow::Error::from)?;
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
                    3 => serde_json::json!(row.get::<_, String>(col).map_err(anyhow::Error::from)?),
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

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
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
        let leaf = || Query::IsPresent { field: "x".into() };
        assert_eq!(node_count(&leaf()), 1);

        // 1 (Or) + 5 leaves; nesting and follow conditions also count.
        let nested = Query::And {
            operands: vec![
                leaf(),
                Query::Not { operand: Box::new(leaf()) },
                Query::FollowsTransitive {
                    field: "mfr_path".into(),
                    target: FollowTarget::Condition(Box::new(leaf())),
                },
            ],
        };
        // And + leaf + (Not + leaf) + (FollowsTransitive + leaf) = 6
        assert_eq!(node_count(&nested), 6);

        // At the limit passes; one over is rejected.
        let at_limit =
            Query::Or { operands: (0..MAX_QUERY_NODES - 1).map(|_| leaf()).collect() };
        assert_eq!(node_count(&at_limit), MAX_QUERY_NODES);
        assert!(check_query_size(&at_limit).is_ok());

        let over = Query::Or { operands: (0..MAX_QUERY_NODES).map(|_| leaf()).collect() };
        assert_eq!(node_count(&over), MAX_QUERY_NODES + 1);
        let err = check_query_size(&over).unwrap_err();
        assert_eq!(err.status, axum::http::StatusCode::BAD_REQUEST);
    }
}

// ── Compiler ──────────────────────────────────────────────────────────────────

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
    db_id: Uuid,
    ctes: Vec<(String, String)>,
    params: Vec<SqlValue>,
    counter: usize,
}

impl<'a> Compiler<'a> {
    /// The `_repo` CTE (declared first, so its parameter binds first) holds
    /// the universe: metarecords owned exclusively by the current repository.
    /// It both isolates results and serves as the complement base for `Not`.
    fn new(conn: &'a Connection, cache: &'a mut TreeCache, db_id: Uuid) -> Self {
        let ctes = vec![(
            "_repo".to_string(),
            "SELECT m1.metarecord_uuid AS uuid FROM metarecord_db m1 \
             WHERE m1.db_id = ? \
               AND (SELECT COUNT(*) FROM metarecord_db m2 \
                    WHERE m2.metarecord_uuid = m1.metarecord_uuid) = 1"
                .to_string(),
        )];
        let params = vec![SqlValue::Blob(db::uuid_to_bytes(db_id))];
        Self { conn, cache, db_id, ctes, params, counter: 0 }
    }

    /// Runs a sub-query on its own (a fresh compiler and statement) and
    /// returns the matching UUIDs, repo-filtered like a top-level query.
    /// Used by the hybrid `FollowsTransitive` execution, whose tree-cache
    /// walk needs the root set before SQL generation.
    fn execute_condition(&mut self, cond: &Query) -> Result<Vec<Uuid>, ApiError> {
        let mut sub = Compiler::new(self.conn, self.cache, self.db_id);
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

    fn compile_node(&mut self, q: &Query) -> Result<String, ApiError> {
        match q {
            Query::IsPresent { field } => {
                self.push_text(field);
                Ok(self.add(
                    "SELECT DISTINCT metarecord_uuid AS uuid FROM field \
                     WHERE field_name = ? AND value_type != 'nothing'"
                        .to_string(),
                ))
            }
            Query::IsAbsent { field } => {
                self.push_text(field);
                Ok(self.add(
                    "SELECT DISTINCT metarecord_uuid AS uuid FROM field \
                     WHERE field_name = ? AND value_type = 'nothing'"
                        .to_string(),
                ))
            }
            Query::IsUnknown { field } => {
                self.push_text(field);
                Ok(self.add(
                    "SELECT uuid FROM _repo WHERE uuid NOT IN \
                     (SELECT metarecord_uuid FROM field WHERE field_name = ?)"
                        .to_string(),
                ))
            }

            Query::Eq { field, value } => self.comparison(field, value, CmpOp::Eq),
            Query::Lt { field, value } => self.comparison(field, value, CmpOp::Lt),
            Query::Lte { field, value } => self.comparison(field, value, CmpOp::Lte),
            Query::Gt { field, value } => self.comparison(field, value, CmpOp::Gt),
            Query::Gte { field, value } => self.comparison(field, value, CmpOp::Gte),
            Query::Neq { field, value } => {
                // At least one non-Nothing occurrence differing from `value`
                // (a different value type counts as differing).
                self.push_text(field);
                let pred = self.scalar_predicate(value, CmpOp::Eq)?;
                Ok(self.add(format!(
                    "SELECT DISTINCT metarecord_uuid AS uuid FROM field \
                     WHERE field_name = ? AND value_type != 'nothing' AND NOT ({pred})"
                )))
            }

            Query::And { operands } => self.combine(operands, "INTERSECT"),
            Query::Or { operands } => self.combine(operands, "UNION"),
            Query::Not { operand } => {
                let sub = self.compile_node(operand)?;
                Ok(self.add(format!(
                    "SELECT uuid FROM _repo EXCEPT SELECT uuid FROM {sub}"
                )))
            }

            Query::Matches { field, pattern } => {
                crate::regexp::compile(pattern).map_err(|e| {
                    ApiError::bad_request(format!("invalid regex pattern: {e}"))
                })?;
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

            Query::Follows { field, target } => match target {
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
            },

            Query::FollowsTransitive { field, target } => {
                // Hybrid execution: the root set (one path-resolved metarecord,
                // or every match of the condition sub-query) and its
                // descendants are collected through the tree cache, then
                // injected as inline literals (no bound parameter limit).
                // Only TreeRef trees have descendants; on a Ref field this
                // matches nothing by construction.
                let conn = self.conn;
                let roots = match target {
                    FollowTarget::Path(path) => {
                        match self.cache.resolve_path(conn, field, path)? {
                            None => Vec::new(),
                            Some(uuid) => vec![uuid],
                        }
                    }
                    FollowTarget::Condition(cond) => self.execute_condition(cond)?,
                };
                let mut descendants = Vec::new();
                let mut seen = std::collections::HashSet::new();
                for root in roots {
                    for d in self.cache.descendants(conn, field, root)? {
                        if seen.insert(d) {
                            descendants.push(d);
                        }
                    }
                }
                if descendants.is_empty() {
                    return Ok(self.empty());
                }
                let literals: Vec<String> = descendants
                    .iter()
                    .map(|u| format!("(x'{}')", hex_encode(u.as_bytes())))
                    .collect();
                Ok(self.add(format!(
                    "SELECT column1 AS uuid FROM (VALUES {})",
                    literals.join(",")
                )))
            }

            Query::UuidIn { uuids } => {
                if uuids.is_empty() {
                    return Ok(self.empty());
                }
                // Inline the uuids as literals (no bound-parameter limit) and
                // intersect with `_repo` so non-owned / unknown uuids drop out.
                let literals: Vec<String> = uuids
                    .iter()
                    .map(|u| format!("(x'{}')", hex_encode(u.as_bytes())))
                    .collect();
                Ok(self.add(format!(
                    "SELECT uuid FROM _repo WHERE uuid IN (SELECT column1 FROM (VALUES {}))",
                    literals.join(",")
                )))
            }
        }
    }

    fn comparison(&mut self, field: &str, value: &Value, op: CmpOp) -> Result<String, ApiError> {
        self.push_text(field);
        let pred = self.scalar_predicate(value, op)?;
        Ok(self.add(format!(
            "SELECT DISTINCT metarecord_uuid AS uuid FROM field \
             WHERE field_name = ? AND ({pred})"
        )))
    }

    fn combine(&mut self, operands: &[Query], set_op: &str) -> Result<String, ApiError> {
        if operands.is_empty() {
            return Err(ApiError::bad_request("'and'/'or' need at least one operand"));
        }
        if operands.len() > MAX_COMBINATOR_OPERANDS {
            return Err(ApiError::bad_request(format!(
                "a single 'and'/'or' may have at most {MAX_COMBINATOR_OPERANDS} operands \
                 (got {}); nest or decompose it",
                operands.len()
            )));
        }
        let mut parts = Vec::with_capacity(operands.len());
        for operand in operands {
            let sub = self.compile_node(operand)?;
            parts.push(format!("SELECT uuid FROM {sub}"));
        }
        Ok(self.add(parts.join(&format!(" {set_op} "))))
    }

    /// Row-level predicate for one comparison operand; pushes its parameters.
    fn scalar_predicate(&mut self, value: &Value, op: CmpOp) -> Result<String, ApiError> {
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
            Value::String(s) => {
                // Same convention as Matches and sorting: on a tree_ref row,
                // a string operand compares against the name component.
                self.push_text(s);
                self.push_text(s);
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
                self.params
                    .push(SqlValue::Blob(db::uuid_to_bytes(parent.unwrap_or(ZERO_UUID))));
                self.push_text(name);
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
