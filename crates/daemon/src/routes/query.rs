//! Queries and the set layer (doc "Query endpoints"): `POST /query` and its profile,
//! the preparation feeding the evaluator, batch field writes, retype, delete.

use super::*;

/// `select`: absent → UUID strings; `"*"` → full objects; list → restricted
/// objects (doc "Query endpoints").
#[derive(Deserialize)]
#[serde(untagged)]
pub(super) enum SelectSpec {
    Star(String),
    Fields(Vec<String>),
}

#[derive(Deserialize)]
pub(super) struct QueryBody {
    query: MetaQuery,
    #[serde(default)]
    select: Option<SelectSpec>,
    #[serde(default)]
    sort: Vec<SortKey>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    cursor: Option<String>,
    /// Adds the full result count to the pagination envelope.
    #[serde(default)]
    count: bool,
    /// Stops the query with a `409` (`reason: "timeout"`) once this many
    /// milliseconds have passed; absent, it runs to the end (doc "Query limits").
    #[serde(default)]
    timeout_ms: Option<u64>,
}

pub(super) async fn run_query(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Result<Json<QueryBody>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    // Register an observation-only task (doc "Tasks"): the result travels with
    // this response, so the task carries no result payload and its counts stay
    // unknown (the heavy part is one opaque evaluation). Registered here, before
    // the blocking work, so that a client hanging up cancels it.
    let repo = state.ready_repo(repo_uuid)?;
    let task = repo.tasks.start(TaskKind::Query);
    let _hang_up = CancelOnHangUp { repo: Arc::downgrade(&repo), task };
    drop(repo);
    with_repo(&state, repo_uuid, move |repo_state| {
        repo_state.tasks.mark_running(task);
        repo_state.tasks.set_progress(task, "querying", None, None);
        let outcome = run_query_inner(repo_state, &body, task);
        // A cancel request stops the query (in its loops, or between its
        // phases), surfacing here as an error: record the task as `cancelled`
        // (not `failed`) and report it as a 409 to the waiting client.
        if outcome.is_err() && repo_state.tasks.is_cancel_requested(task) {
            repo_state.tasks.mark_cancelled(task);
            return Err(ApiError::conflict("query cancelled")
                .with_field("reason", json!(crate::interrupt::Reason::Cancelled.as_str())));
        }
        match &outcome {
            Ok(_) => repo_state.tasks.finish(task, None),
            Err(e) => repo_state.tasks.fail(task, &e.message),
        }
        outcome
    })
    .await
}

/// Cancels a query's task when dropped. The handler holding it is dropped
/// before it completes only when the client hangs up — the GUI dropping a query
/// a newer one replaced, a Ctrl-C on `mf` — and nobody is left to read the
/// answer (doc "Query limits"). Dropped after the task
/// ended, it does nothing. `Weak`: it must not keep the repository alive.
pub(super) struct CancelOnHangUp {
    repo: std::sync::Weak<RepoState>,
    task: Uuid,
}

impl Drop for CancelOnHangUp {
    fn drop(&mut self) {
        if let Some(repo) = self.repo.upgrade() {
            repo.tasks.request_cancel(self.task);
        }
    }
}

/// `POST /repos/:repo/query/profile` — runs a query as `/query` does and
/// answers with its slow-log entry instead of its results: the phases, their
/// time and the keys each read, whatever the query cost (doc "Slow log"
/// "Replaying a query"). The replay itself is not logged.
pub(super) async fn profile_query(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Result<Json<QueryBody>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        slowlog::discard();
        let task = repo_state.tasks.start(TaskKind::Query);
        repo_state.tasks.mark_running(task);
        let (outcome, entry) = slowlog::capture("POST /repos/:repo/query", || {
            run_query_inner(repo_state, &body, task)
        });
        match &outcome {
            Ok(_) => repo_state.tasks.finish(task, None),
            Err(e) => repo_state.tasks.fail(task, &e.message),
        }
        outcome?;
        Ok(Json(json!({ "entry": entry })))
    })
    .await
}

/// One page of query results: the metarecords, the cursor for the next page
/// (`None` at the end), and the total — present only when the body asked to
/// `count`.
pub(super) type QueryPage = (Vec<Uuid>, Option<String>, Option<usize>);

/// What a repository's queries are evaluated against: the store's own derived
/// key spaces, read in one snapshot (doc "Storage").
pub(super) struct Engine<'a>(crate::kvstore::KvSource<'a>);

impl Engine<'_> {
    pub(super) fn eval(&self) -> crate::index::Eval<'_> {
        crate::index::Eval { src: &self.0, strategy: crate::index::PageStrategy::Auto }
    }

    pub(super) fn value_type(&self, field: &str) -> Option<String> {
        crate::index::Source::value_type(&self.0, field).map(str::to_string)
    }

    pub(super) fn field_catalog(&self, type_filter: Option<&str>) -> Vec<(String, String)> {
        self.0.field_catalog(type_filter)
    }

    /// Fails with the read error the source met, if it met one: its answers
    /// went empty from there, so they must not be served.
    pub(super) fn check(&self) -> Result<(), ApiError> {
        match self.0.take_error() {
            Some(e) => Err(ApiError::internal(format!("reading the store failed: {e:#}"))),
            None => Ok(()),
        }
    }
}

/// The repository's [`Engine`].
pub(super) fn engine(conn: &dyn crate::store::Store) -> Result<Engine<'_>, ApiError> {
    let kv = conn.as_kv().ok_or_else(|| ApiError::internal("a repository not on its store"))?;
    let src = kv.source().map_err(|e| ApiError::internal(format!("{e:#}")))?;
    Ok(Engine(src))
}

/// Resolves a query's index seeds and rewrites its index-unsupported text leaves
/// — the shared preparation feeding the bitmap index, used by both the paginated
/// query path ([`run_query_filter`]) and the whole-set resolution
/// ([`resolve_query_uuids`]). Path targets and exact-node operands resolve to
/// metarecords through the tree cache; the leaves the bitmaps cannot serve — a
/// `:path` predicate, an order-sensitive `osm` path — are resolved against the
/// same forest and rewritten to `UuidIn` sets (doc "No operand runs in SQL").
fn prepare_indexed_query<'a>(
    conn: &dyn crate::store::Store,
    cache: &crate::tree_cache::TreeCache,
    index: &crate::index::Eval<'_>,
    query: &MetaQuery,
) -> Result<(crate::index::QueryRoots<'a>, MetaQuery), ApiError> {
    let _phase = slowlog::phase("prepare");
    let mut roots = crate::index::QueryRoots::new();
    let mut path_targets = Vec::new();
    crate::index::collect_path_targets(query, &mut path_targets);
    for (field, path) in path_targets {
        if let Some(uuid) = cache.resolve_path(conn, &field, &path)? {
            roots.path.insert((field, path), uuid);
        }
    }
    // Exact-node `Eq`/`Neq` operands (`mfr_path = "/a/b.txt"`): resolved through
    // the same cache, but the entry is always inserted — including a `None` for a
    // path that is no node — so the index can tell "resolved to nothing" (empty
    // result) from "nobody resolved it" (a daemon bug, since this loop resolves
    // every one of them).
    let mut node_paths = Vec::new();
    crate::index::collect_node_paths(query, &mut node_paths);
    for (field, path) in node_paths {
        let node = cache.resolve_path(conn, &field, &path)?;
        roots.node.insert((field, path), node);
    }
    // The forest's own leaves. A `Matches` or an `osm direct` needs no rewrite:
    // the index runs the regex over the field's distinct values in memory.
    let indexed = crate::forest_query::resolve_path_leaves(cache, conn, Some(index), query)?;
    Ok((roots, indexed))
}

/// Resolves a query to *all* its matching uuids, index-accelerated — the
/// set-layer counterpart of [`run_query_filter`] (batch field writes, query
/// delete, tree resolution) which operate on the whole match set rather than a
/// page. Shares [`prepare_indexed_query`] so these writes get the same bitmap
/// acceleration as reads; an unsupported shape is reported by [`index_gap`],
/// there being nothing else to ask.
pub(super) fn resolve_query_uuids(
    conn: &dyn crate::store::Store,
    cache: &crate::tree_cache::TreeCache,
    query: &MetaQuery,
) -> Result<Vec<Uuid>, ApiError> {
    let _phase = slowlog::phase("resolve.uuids");
    crate::query_validate::validate_query(query)?;
    crate::query_validate::check_query_size(query)?;
    let engine = engine(conn)?;
    crate::query_validate::validate_query_types(query, &|f| engine.value_type(f))?;
    let (roots, indexed) = prepare_indexed_query(conn, cache, &engine.eval(), query)?;
    let evaluated = slowlog::timed("index.evaluate", || {
        engine.eval().evaluate_page_with_roots(&indexed, &[], None, None, &roots)
    });
    engine.check()?;
    match evaluated {
        Ok((uuids, _)) => Ok(uuids),
        Err(gap) => Err(index_gap(gap)),
    }
}

/// A query the index declined, turned into the answer the client gets. There is
/// no second engine to ask any more (doc "No operand runs in SQL"), so
/// only the cursor — the client's own input — can be at fault; anything else is
/// the daemon failing to serve what it promises, reported as such rather than
/// absorbed by an engine that would answer slowly.
fn index_gap(gap: crate::index::Unsupported) -> ApiError {
    if gap.is_cursor() {
        return ApiError::bad_request(gap.to_string());
    }
    ApiError::internal(format!("the query index cannot serve this query: {gap}"))
}

/// The evaluation reads one snapshot of the store, so it sees exactly the
/// committed state. Every operand is served from the store's derived key
/// spaces — the bitmaps, or the forest through [`prepare_indexed_query`] — so
/// there is no second engine to defer to: a shape that comes back `Unsupported` is a daemon
/// bug (see [`index_gap`]).
fn run_query_filter(
    conn: &dyn crate::store::Store,
    cache: &crate::tree_cache::TreeCache,
    body: &QueryBody,
    cancel: &dyn Fn() -> bool,
) -> Result<QueryPage, ApiError> {
    // Reject ill-defined comparisons and over-large queries upfront, before
    // anything is evaluated, so neither rejection depends on how the query
    // happens to be served (doc "Query limits", doc "Comparisons").
    crate::query_validate::validate_query(&body.query)?;
    crate::query_validate::check_query_size(&body.query)?;
    // The rejections that need the field's type follow, as soon as the index is
    // in hand — and *before* the preparation, which would otherwise rewrite an
    // invalid leaf into the empty set it matches and answer "no rows" where the
    // user deserves a 400 (doc "No operand runs in SQL").

    let sort_by: Vec<crate::index::SortBy> = body
        .sort
        .iter()
        .map(|k| crate::index::SortBy {
            field: k.field.clone(),
            ascending: matches!(k.order, crate::query_result::SortOrder::Asc),
        })
        .collect();

    // Resolve the query's index seeds (Path targets, exact-node operands) and
    // rewrite the leaves the bitmaps cannot serve into the `uuid_in` sets the
    // forest says they match. The preparation must be a function of the query
    // *shape* alone: the list asks for `count` on the first page only, and if
    // that changed what is prepared, page 1 and page 2 would be evaluating
    // different queries and would reject each other's cursor (which is bound to
    // a hash of the rewritten query).
    let engine = engine(conn)?;
    crate::query_validate::validate_query_types(&body.query, &|f| engine.value_type(f))?;
    let index = engine.eval();

    let (mut roots, indexed_query) = prepare_indexed_query(conn, cache, &index, &body.query)?;
    // Full-path sort keys for a `tree_ref` sort key, rebuilt from the store's
    // forest (doc "Pagination and sorting", doc "The forest in the store").
    let sort_keys = crate::tree_cache::SortKeys::new(conn);
    roots.keys = Some(&sort_keys);
    // The preparation above (path seeds, forest leaves) can be the heavy phase
    // on a large repo; if a Stop landed during it, don't start the evaluation.
    if cancel() {
        return Err(ApiError::conflict("query cancelled"));
    }

    // With `count` the page and the total come from a single evaluation; without
    // it, only the page is computed.
    let paged = slowlog::timed("index.evaluate", || {
        if body.count {
            index
                .page_and_count(
                    &indexed_query,
                    &sort_by,
                    body.limit,
                    body.cursor.as_deref(),
                    &roots,
                )
                .map(|(uuids, next, total)| (uuids, next, Some(total as usize)))
        } else {
            index
                .evaluate_page_with_roots(
                    &indexed_query,
                    &sort_by,
                    body.limit,
                    body.cursor.as_deref(),
                    &roots,
                )
                .map(|(uuids, next)| (uuids, next, None))
        }
    });
    engine.check()?;
    if let Some(e) = sort_keys.take_error() {
        return Err(ApiError::internal(format!("reading the forest failed: {e:#}")));
    }
    paged.map_err(index_gap)
}

/// [`run_query_pass`] interruptible inside its loops (`crate::interrupt`): by
/// a cancel request on its task, and by the body's `timeout_ms`. A query
/// stopped either way answers `409` with the `reason`, whatever its failing
/// reads made of its answer.
fn run_query_inner(
    repo_state: &RepoState,
    body: &QueryBody,
    task: Uuid,
) -> Result<Response, ApiError> {
    use crate::interrupt::{self, Reason};
    use std::sync::atomic::{AtomicBool, Ordering};
    let cancelled = Arc::new(AtomicBool::new(false));
    let flag = cancelled.clone();
    repo_state.tasks.set_canceller(task, Box::new(move || flag.store(true, Ordering::Relaxed)));
    // A request made before the canceller was in place (a client that hung up
    // while the query waited for the repository) set only the task's flag.
    if repo_state.tasks.is_cancel_requested(task) {
        cancelled.store(true, Ordering::Relaxed);
    }
    let deadline =
        body.timeout_ms.map(|ms| std::time::Instant::now() + std::time::Duration::from_millis(ms));
    let probe = Box::new(move || {
        if cancelled.load(Ordering::Relaxed) {
            Some(Reason::Cancelled)
        } else if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            Some(Reason::TimedOut)
        } else {
            None
        }
    });
    match interrupt::run(probe, || run_query_pass(repo_state, body, task)) {
        (_, Some(reason)) => {
            let message = interrupt::Interrupted(reason).to_string();
            Err(ApiError::conflict(message).with_field("reason", json!(reason.as_str())))
        }
        (outcome, None) => outcome,
    }
}

fn run_query_pass(
    repo_state: &RepoState,
    body: &QueryBody,
    task: Uuid,
) -> Result<Response, ApiError> {
    {
        if body.count && body.limit.is_none() {
            // The unwrapped (bare array) response has nowhere to carry it.
            return Err(ApiError::bad_request("'count' requires 'limit'"));
        }
        let conn = slowlog::timed("wait:conn", || repo_state.conn.lock_recover());
        // Cooperative cancellation (doc "Tasks"): the evaluation and the result
        // assembly poll this flag.
        let cancel = || repo_state.tasks.is_cancel_requested(task);
        let cache = repo_state.tree();
        note_query(body);
        let (uuids, next_cursor, total) = run_query_filter(&conn, &cache, body, &cancel)?;
        slowlog::note("results", uuids.len().to_string());

        let results: Vec<serde_json::Value> = match &body.select {
            None => uuids.into_iter().map(|u| json!(hex(u))).collect(),
            Some(select) => {
                let fields_filter: Option<Vec<String>> = match select {
                    SelectSpec::Star(s) if s == "*" => None,
                    SelectSpec::Star(s) => {
                        return Err(ApiError::bad_request(format!(
                            "invalid select: '{s}' (expected \"*\" or a field list)"
                        )))
                    }
                    SelectSpec::Fields(list) => Some(list.clone()),
                };
                slowlog::timed("assemble", || {
                    crate::query_result::assemble_selected(
                        &conn,
                        &uuids,
                        fields_filter.as_deref(),
                        &cancel,
                    )
                })?
            }
        };

        if body.limit.is_some() {
            Ok(Json(Page { results, next_cursor, total }).into_response())
        } else {
            Ok(Json(results).into_response())
        }
    }
}

#[derive(Deserialize)]
pub(super) struct BatchSetBody {
    query: MetaQuery,
    name: String,
    #[serde(default)]
    value: Option<Value>,
    #[serde(default)]
    values: Option<Vec<Value>>,
    #[serde(default)]
    force: bool,
}

/// Resolves a `{value | values}` field-write body to its row set; exactly one of
/// the two must be present (set accepts several, the single-value ops one).
pub(super) fn resolved_values(
    value: Option<Value>,
    values: Option<Vec<Value>>,
) -> Result<Vec<Value>, ApiError> {
    match (value, values) {
        (Some(_), Some(_)) => {
            Err(ApiError::bad_request("provide either 'value' or 'values', not both"))
        }
        (Some(v), None) => Ok(vec![v]),
        (None, Some(vs)) => Ok(vs),
        (None, None) => Err(ApiError::bad_request("missing 'value' (or 'values')")),
    }
}

/// Like [`resolved_values`] but for operations that take exactly one value
/// (append, remove).
pub(super) fn single_value(
    value: Option<Value>,
    values: Option<Vec<Value>>,
) -> Result<Value, ApiError> {
    match (value, values) {
        (Some(v), None) => Ok(v),
        _ => Err(ApiError::bad_request("this operation takes a single 'value'")),
    }
}

/// Runs the query server-side and sets the field on every match in a single
/// transaction (one revision). `value` sets one row; `values` a multi-map set —
/// either way one `SetField` op per metarecord.
pub(super) async fn batch_set(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Result<Json<BatchSetBody>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    let rows = resolved_values(body.value, body.values)?;
    let field = body.name.clone();
    // A whole-field overwrite always rewrites the rows, so every match counts as
    // updated — unlike the append/remove forms, which report what they changed.
    write_matches(&state, repo_uuid, body.name, body.force, body.query, move |writer, uuid| {
        writer.set_field_multi(uuid, &field, rows.clone())?;
        Ok(true)
    })
    .await
}

/// Runs the query server-side and adds one field row to every match in a
/// single transaction (one revision) — the bulk form of `POST
/// /metarecords/:uuid/fields`, and the set-layer half of `mf metarecord field
/// add`. Multi-map: appends, never replaces existing rows.
pub(super) async fn batch_add(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Result<Json<BatchSetBody>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    let value = single_value(body.value, body.values)?;
    let field = body.name.clone();
    // A match that already holds the value gains nothing, so it is not counted
    // (doc "No duplicate rows").
    write_matches(&state, repo_uuid, body.name, body.force, body.query, move |writer, uuid| {
        Ok(writer.append_field(uuid, &field, value.clone())?.created())
    })
    .await
}

/// Runs the query server-side and removes every field row equal to
/// `(name, value)` from each match in a single transaction (one revision) — the
/// inverse of `batch_add`. `updated` counts the metarecords actually changed
/// (those that carried at least one matching row).
pub(super) async fn batch_remove(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Result<Json<BatchSetBody>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    let value = single_value(body.value, body.values)?;
    let field = body.name.clone();
    write_matches(&state, repo_uuid, body.name, body.force, body.query, move |writer, uuid| {
        Ok(writer.delete_fields_valued(uuid, &field, &value)? > 0)
    })
    .await
}

#[derive(Deserialize)]
pub(super) struct BatchUnsetBody {
    query: MetaQuery,
    name: String,
    #[serde(default)]
    force: bool,
}

/// Runs the query server-side and removes the field *entirely* (every row of
/// `name`) from each match in a single transaction (one revision; one
/// `DeleteField` op per affected metarecord). The field becomes unknown. `updated`
/// counts the metarecords that carried the field.
pub(super) async fn batch_unset(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Result<Json<BatchUnsetBody>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    let field = body.name.clone();
    write_matches(&state, repo_uuid, body.name, body.force, body.query, move |writer, uuid| {
        Ok(writer.delete_fields_named(uuid, &field)? > 0)
    })
    .await
}

/// One write of a `POST /query/fields/batch`: a creation, or one of the four
/// set-layer verbs over a query.
#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub(super) enum BatchOp {
    Create {
        #[serde(default)]
        uuid: Option<String>,
        fields: Vec<Field>,
        #[serde(default)]
        force: bool,
    },
    Set {
        query: MetaQuery,
        name: String,
        #[serde(default)]
        value: Option<Value>,
        #[serde(default)]
        values: Option<Vec<Value>>,
        #[serde(default)]
        force: bool,
    },
    Add {
        query: MetaQuery,
        name: String,
        value: Value,
        #[serde(default)]
        force: bool,
    },
    Remove {
        query: MetaQuery,
        name: String,
        value: Value,
        #[serde(default)]
        force: bool,
    },
    Unset {
        query: MetaQuery,
        name: String,
        #[serde(default)]
        force: bool,
    },
}

#[derive(Deserialize)]
pub(super) struct BatchBody {
    ops: Vec<BatchOp>,
}

/// Writes one metarecord of a batch op; answers whether it changed.
type WriteOne = Box<dyn FnMut(&mut Writer, Uuid) -> Result<bool, ApiError>>;

/// A batch op made ready to apply: its query already resolved.
enum Ready {
    Create(Option<Uuid>, Vec<Field>),
    Set(Vec<Uuid>, String, Vec<Value>),
    Add(Vec<Uuid>, String, Value),
    Remove(Vec<Uuid>, String, Value),
    Unset(Vec<Uuid>, String),
}

/// `POST /repos/:repo/query/fields/batch`: several writes — creations and the
/// set-layer verbs — applied in order as **one** revision, so a client
/// operation made of several steps (`mf tag add` and its rewrites) is undone
/// in one step (doc "Editing a set of metarecords"). Every query is resolved
/// against the repository as it stood *before* the batch, so an op never sees
/// what an earlier one wrote. Schema validation runs once at the end, per
/// touched metarecord, on every name the batch wrote to it. All or nothing:
/// any failure rolls the whole batch back. Answers one result per op —
/// `{uuid}` for a creation, `{updated}` for the others.
pub(super) async fn batch_writes(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Result<Json<BatchBody>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        repo_state.ensure_writable()?;
        let mut conn = slowlog::timed("wait:conn", || repo_state.conn.lock_recover());
        let cache = repo_state.tree();
        let mut ready = Vec::with_capacity(body.ops.len());
        for op in body.ops {
            ready.push(match op {
                BatchOp::Create { uuid, fields, force } => {
                    for field in &fields {
                        check_writable(&field.name, force)?;
                    }
                    Ready::Create(uuid.as_deref().map(parse_uuid).transpose()?, fields)
                }
                BatchOp::Set { query, name, value, values, force } => {
                    check_writable(&name, force)?;
                    let rows = resolved_values(value, values)?;
                    Ready::Set(resolve_query_uuids(&*conn, &cache, &query)?, name, rows)
                }
                BatchOp::Add { query, name, value, force } => {
                    check_writable(&name, force)?;
                    Ready::Add(resolve_query_uuids(&*conn, &cache, &query)?, name, value)
                }
                BatchOp::Remove { query, name, value, force } => {
                    check_writable(&name, force)?;
                    Ready::Remove(resolve_query_uuids(&*conn, &cache, &query)?, name, value)
                }
                BatchOp::Unset { query, name, force } => {
                    check_writable(&name, force)?;
                    Ready::Unset(resolve_query_uuids(&*conn, &cache, &query)?, name)
                }
            });
        }

        let mut writer = repo_state.writer(&mut conn, None)?;
        let writing = slowlog::phase("write.fields");
        // Every metarecord the batch changed, with the names it wrote there.
        let mut touched: std::collections::BTreeMap<Uuid, Vec<String>> = Default::default();
        let mut touch = |uuid: Uuid, name: &str| {
            let names = touched.entry(uuid).or_default();
            if !names.iter().any(|n| n == name) {
                names.push(name.to_string());
            }
        };
        let mut results = Vec::with_capacity(ready.len());
        for op in ready {
            let (uuids, name, mut write): (Vec<Uuid>, String, WriteOne) = match op {
                Ready::Create(uuid, fields) => {
                    let names: Vec<String> = fields.iter().map(|f| f.name.clone()).collect();
                    let created = match uuid {
                        Some(uuid) => {
                            if Rows::version(writer.store(), uuid)?.is_some() {
                                return Err(ApiError::conflict(format!(
                                    "metarecord already exists: {uuid}"
                                )));
                            }
                            writer.create_metarecord_with_uuid(uuid, fields)?
                        }
                        None => writer.create_metarecord(fields)?,
                    };
                    for name in &names {
                        touch(created.uuid, name);
                    }
                    results.push(json!({ "uuid": hex(created.uuid) }));
                    continue;
                }
                Ready::Set(uuids, name, rows) => {
                    let field = name.clone();
                    (
                        uuids,
                        name,
                        Box::new(move |w: &mut Writer, u| {
                            w.set_field_multi(u, &field, rows.clone())?;
                            Ok(true)
                        }),
                    )
                }
                Ready::Add(uuids, name, value) => {
                    let field = name.clone();
                    (
                        uuids,
                        name,
                        Box::new(move |w: &mut Writer, u| {
                            Ok(w.append_field(u, &field, value.clone())?.created())
                        }),
                    )
                }
                Ready::Remove(uuids, name, value) => {
                    let field = name.clone();
                    (
                        uuids,
                        name,
                        Box::new(move |w: &mut Writer, u| {
                            Ok(w.delete_fields_valued(u, &field, &value)? > 0)
                        }),
                    )
                }
                Ready::Unset(uuids, name) => {
                    let field = name.clone();
                    (
                        uuids,
                        name,
                        Box::new(
                            move |w: &mut Writer, u| Ok(w.delete_fields_named(u, &field)? > 0),
                        ),
                    )
                }
            };
            let mut updated = 0usize;
            for uuid in uuids {
                if write(&mut writer, uuid)? {
                    updated += 1;
                    touch(uuid, &name);
                }
            }
            results.push(json!({ "updated": updated }));
        }
        drop(writing);
        for (uuid, names) in &touched {
            slowlog::timed("validate.schema", || {
                validate_schema(repo_state, writer.store(), *uuid, names)
            })?;
        }
        let effects = writer.effects();
        slowlog::timed("commit", || writer.commit())?;
        repo_state.settle(&conn, &effects)?;
        Ok(Json(json!({ "results": results })))
    })
    .await
}

#[derive(Deserialize)]
pub(super) struct RetypeBody {
    name: String,
    to: String,
}

/// `POST /repos/:repo/retype`: converts every non-`Nothing` row of the field
/// `name` to any value type, repository-wide, in one revision
/// (doc "Changing a field's type"). Reserved fields (`mfr_*`/`mf_*`) are
/// rejected unconditionally — the system owns their types.
pub(super) async fn retype_field(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Result<Json<RetypeBody>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    let name = body.name;
    if name.starts_with("mfr_") || name.starts_with("mf_") {
        return Err(ApiError::bad_request(format!(
            "field '{name}' is reserved; its type is owned by the system and cannot be retyped"
        )));
    }
    let to = FieldType::parse(&body.to).ok_or_else(|| {
        ApiError::bad_request(format!(
            "invalid target type '{}': retype targets one of \
             string/int/float/bool/datetime/ref/tree_ref/externalref/refbase",
            body.to
        ))
    })?;
    with_repo(&state, repo_uuid, move |repo_state| {
        repo_state.ensure_writable()?;
        let mut conn = slowlog::timed("wait:conn", || repo_state.conn.lock_recover());
        let mut writer = repo_state.writer(&mut conn, None)?;
        let summary = writer.retype_field(&name, to)?;
        let effects = writer.effects();
        slowlog::timed("commit", || writer.commit())?;
        repo_state.settle(&conn, &effects)?;
        Ok(Json(json!({
            "converted": summary.converted,
            "fallback_count": summary.fallback_uuids.len(),
            "fallback_uuids": summary.fallback_uuids.iter().map(|u| hex(*u)).collect::<Vec<_>>(),
        })))
    })
    .await
}

#[derive(Deserialize)]
pub(super) struct QueryDeleteBody {
    query: MetaQuery,
}

/// `POST /repos/:repo/query/delete` — deletes every metarecord matching `query` in a
/// single transaction (one revision). Atomic and free of the client-side
/// TOCTOU of selecting then deleting one-by-one over HTTP.
pub(super) async fn delete_by_query(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Result<Json<QueryDeleteBody>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        repo_state.ensure_writable()?;
        let mut conn = slowlog::timed("wait:conn", || repo_state.conn.lock_recover());
        let cache = repo_state.tree();
        let uuids = resolve_query_uuids(&conn, &cache, &body.query)?;

        let mut writer = repo_state.writer(&mut conn, None)?;
        let writing = slowlog::phase("write.fields");
        for uuid in &uuids {
            writer.delete_metarecord(*uuid)?;
        }
        drop(writing);
        let effects = writer.effects();
        slowlog::timed("commit", || writer.commit())?;
        repo_state.settle(&conn, &effects)?;
        Ok(Json(json!({"deleted": uuids.len()})))
    })
    .await
}

/// Records what a slow query ran — the IR whole (the daemon receives it, not
/// the text that produced it), with the sort, limit and count that decide how
/// it was evaluated: enough to replay it (doc "POST /repos/:repo/query/profile").
fn note_query(body: &QueryBody) {
    if let Ok(json) = serde_json::to_string(&body.query) {
        slowlog::note("query", json.chars().take(slowlog::MAX_QUERY_CHARS).collect::<String>());
    }
    if !body.sort.is_empty() {
        if let Ok(json) = serde_json::to_string(&body.sort) {
            slowlog::note("sort", json);
        }
    }
    if let Some(limit) = body.limit {
        slowlog::note("limit", limit.to_string());
    }
    if body.count {
        slowlog::note("count", "true");
    }
}
