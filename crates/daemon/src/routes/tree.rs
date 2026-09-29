//! The forest and field-catalogue routes: TreeRef paths resolved both ways,
//! a forest's roots and a node's children, the field catalogue.

use super::*;

#[derive(Deserialize)]
pub(super) struct QueryResolveTreeBody {
    query: MetaQuery,
    #[serde(default = "default_tree_field")]
    field: String,
}

fn default_tree_field() -> String {
    "mfr_path".to_string()
}

/// `POST /repos/:repo/query/fields/resolve-tree`: resolves the TreeRef `field`
/// (default `mfr_path`) of every metarecord matching `query` to repo-root-
/// relative paths. Each metarecord maps to an array of paths — one, a node
/// holding one position per forest; empty when it has none or its chain is
/// stale. Resolved from the store's forest — one round-trip whatever the depth. (Target an explicit set with a
/// `uuid_in` query.)
pub(super) async fn query_resolve_tree(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Result<Json<QueryResolveTreeBody>, JsonRejection>,
) -> Result<Response, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    let Json(body) = payload?;
    let field = body.field;
    with_repo(&state, repo_uuid, move |repo_state| {
        let conn = slowlog::timed("wait:conn", || repo_state.conn.lock_recover());
        let cache = repo_state.tree();
        let uuids = resolve_query_uuids(&conn, &cache, &body.query)?;
        let mut out = serde_json::Map::new();
        for uuid in uuids {
            let paths = cache.paths_of(&conn, &field, uuid)?;
            out.insert(hex(uuid), json!(paths));
        }
        Ok(Json(serde_json::Value::Object(out)).into_response())
    })
    .await
}

/// `GET /repos/:repo/metarecords/:uuid/fields/:name/resolve-tree`: the direct
/// (single-metarecord) form of `resolve-tree`.
pub(super) async fn resolve_record_field_tree(
    State(state): State<Arc<AppState>>,
    Path((repo, uuid, name)): Path<(String, String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    let uuid = parse_uuid(&uuid)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        let conn = slowlog::timed("wait:conn", || repo_state.conn.lock_recover());
        let cache = repo_state.tree();
        let paths = cache.paths_of(&conn, &name, uuid)?;
        Ok(Json(json!({ "paths": paths })))
    })
    .await
}

/// `GET /repos/:repo/metarecords/:uuid/mf-sync`: the record's effective
/// `mf_sync` mode (spec-sync) — `external` when an external tool owns its
/// content, else `internal`. Resolved from the record's `mfr_path` position
/// (inherited like `mf_watch`); a record with no `mfr_path` is `internal`.
pub(super) async fn get_record_mf_sync(
    State(state): State<Arc<AppState>>,
    Path((repo, uuid)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    let uuid = parse_uuid(&uuid)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        let conn = slowlog::timed("wait:conn", || repo_state.conn.lock_recover());
        let cache = repo_state.tree();
        let paths = cache.paths_of(&conn, "mfr_path", uuid)?;
        let mode = match paths.first() {
            // `paths_of` and `resolve_mf_sync` (eligibility) share the same
            // leading-"/"-rooted form (`""` = root, `/a/b` = nested).
            Some(p) => crate::eligibility::resolve_mf_sync(&conn, &cache, p)?,
            None => "internal".to_string(),
        };
        Ok(Json(json!({ "mf_sync": mode })))
    })
    .await
}

#[derive(Deserialize)]
pub(super) struct ResolvePathBody {
    #[serde(default = "default_tree_field")]
    field: String,
    /// Repo-root-relative path (components split on `/`, in the Path string
    /// format): leading-`/`-rooted for the filesystem forest (e.g. `/music/jazz`,
    /// whose first component is the empty root name), no leading `/` for a
    /// named-root forest such as tags (e.g. `tag1/tag2`).
    path: String,
    /// Which reading of the path to resolve (spec-data-model "Tree names"):
    /// `verbatim` for the characters as written, `escaped` for the bytes a
    /// `%XX` stands for. Absent = both, which resolves to nothing when they
    /// name different files rather than picking one.
    #[serde(default)]
    form: crate::tree_cache::PathForm,
}

/// `POST /repos/:repo/tree/resolve-path`: resolves a repo-root-relative path in
/// the TreeRef `field` (default `mfr_path`) to the uuid of the node at that
/// path, or `null` when no such node exists. The inverse of `resolve-tree`
/// (uuid → paths). Used to set a TreeRef value from a path: resolve the parent
/// path to a uuid, then post `{parent, name}`. One round-trip; one keyed read
/// of the store's forest per component.
///
/// Sibling names are unique, so at most one node matches — *per reading*. A
/// component that spells the escaped form of an undecodable byte can name two
/// different files (one really called `%E9.txt`, one holding the byte), and
/// `form` says which is meant; without it such a path resolves to `null`
/// rather than to a guess.
pub(super) async fn resolve_tree_path(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Result<Json<ResolvePathBody>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    let Json(body) = payload?;
    with_repo(&state, repo_uuid, move |repo_state| {
        let conn = slowlog::timed("wait:conn", || repo_state.conn.lock_recover());
        let cache = repo_state.tree();
        let uuid = cache.resolve_path_as(&conn, &body.field, &body.path, body.form)?;
        Ok(Json(json!({ "uuid": uuid.map(hex) })))
    })
    .await
}

#[derive(Deserialize)]
pub(super) struct ListFieldsParams {
    /// Optional value-type filter (e.g. `tree_ref`, `ref`); absent = all types.
    #[serde(rename = "type")]
    type_filter: Option<String>,
}

/// `GET /repos/:repo/fields[?type=<value_type>]`: the distinct field names
/// known to the repository, each with its value type — the data-derived catalog
/// (field names present on metarecords, `Nothing` excluded) merged with the
/// schema's declared field types (schema-priority on conflict; schema-only
/// fields, e.g. `path: tree_ref` declared but not yet carried, are included).
/// With `?type=`, only that value type is returned (e.g. `tree_ref` to populate
/// a picker), applied after the merge. Response is a JSON array
/// `[{"name": ..., "type": ...}, ...]` ordered by name.
pub(super) async fn list_fields(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    Query(params): Query<ListFieldsParams>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        // Extract the schema's declared types into an owned Vec, releasing the
        // schema lock before taking the connection (never hold both).
        let schema_decls = repo_state
            .schema
            .lock_recover()
            .as_ref()
            .map(|s| s.declared_types())
            .unwrap_or_default();
        // The data-derived catalog is the store's own (the index's `present`/
        // `types` key spaces): every distinct field name and value type, no
        // scan. Like every read it waits for the connection, so behind a long
        // write (until reads stop taking it, spec-storage increment 4).
        let conn = slowlog::timed("wait:conn", || repo_state.conn.lock_recover());
        let data = engine(&*conn)?.field_catalog(None);
        // Merge in the schema (schema-priority, schema-only fields added), then
        // apply the `?type=` filter (so a schema-only field of that type shows).
        let names =
            crate::schema::merge_field_catalog(data, schema_decls, params.type_filter.as_deref());
        let out: Vec<serde_json::Value> =
            names.into_iter().map(|(name, ty)| json!({"name": name, "type": ty})).collect();
        Ok(Json(serde_json::Value::Array(out)))
    })
    .await
}

#[derive(Deserialize)]
pub(super) struct TreeRootsParams {
    #[serde(default = "default_tree_field")]
    field: String,
}

#[derive(Deserialize)]
pub(super) struct TreeChildrenParams {
    #[serde(default = "default_tree_field")]
    field: String,
    /// The parent node's metarecord uuid (hex), whose direct children are listed.
    uuid: String,
}

/// `GET /repos/:repo/tree/roots?field=<field>`: the forest roots of a TreeRef
/// field — the nodes whose direct parent is the root sentinel (no parent).
/// Response `[{"uuid": "<hex>", "name": "<name>"}, ...]`, ordered by name. This
/// is the entry point for navigating a forest top-down (the empty path the
/// query DSL resolves to the sentinel matches the *children* of the named root,
/// not the roots themselves, and only when a root is literally named ""). The
/// tree-explorer panel starts here.
pub(super) async fn tree_roots(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    Query(params): Query<TreeRootsParams>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        let conn = slowlog::timed("wait:conn", || repo_state.conn.lock_recover());
        // Roots are stored with `value_uuid = ZERO_UUID` (the sentinel).
        let mut roots = Rows::children(&*conn, &params.field, ZERO_UUID)?;
        roots.sort_by(|a, b| a.1.cmp(&b.1));
        let out: Vec<serde_json::Value> = roots
            .into_iter()
            .map(|(uuid, name)| json!({"uuid": hex(uuid), "name": name}))
            .collect();
        Ok(Json(serde_json::Value::Array(out)))
    })
    .await
}

/// `GET /repos/:repo/tree/children?field=<field>&uuid=<hex>`: the direct
/// children of one TreeRef node as `[{"uuid": "<hex>", "name": "<name>"}, ...]`,
/// ordered by name. One prefix read of the store's forest. Lets a client list a directory's tracked entries — names +
/// their metarecords — in one call, without a query and a per-record fetch of
/// every child.
pub(super) async fn tree_children(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    Query(params): Query<TreeChildrenParams>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    let parent = parse_uuid(&params.uuid)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        let conn = slowlog::timed("wait:conn", || repo_state.conn.lock_recover());
        let cache = repo_state.tree();
        let mut children = cache.children_of(&conn, &params.field, parent)?;
        children.sort_by(|a, b| a.0.cmp(&b.0));
        let out: Vec<serde_json::Value> = children
            .into_iter()
            .map(|(name, uuid)| json!({"uuid": hex(uuid), "name": name}))
            .collect();
        Ok(Json(serde_json::Value::Array(out)))
    })
    .await
}
