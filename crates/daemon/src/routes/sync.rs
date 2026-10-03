//! Cross-repo sync (spec-sync "HTTP API"): links, status, commits.

use super::*;

use crate::sync;

/// Resolves two repo selectors to loaded repos in canonical order and runs `f`
/// on the blocking pool with both (`repo_a` < `repo_b` lexicographically).
/// 404 if either repo is not loaded; 400 if the two are the same repo.
async fn with_pair<T, F>(state: &AppState, x: &str, y: &str, f: F) -> Result<T, ApiError>
where
    T: Send + 'static,
    F: FnOnce(&RepoState, &RepoState) -> Result<T, ApiError> + Send + 'static,
{
    let x = parse_uuid(x)?;
    let y = parse_uuid(y)?;
    let (a, b) = sync::canonical_pair(x, y)
        .ok_or_else(|| ApiError::bad_request("a repository cannot be synced with itself"))?;
    let repo_a = state.repo(a)?;
    let repo_b = state.repo(b)?;
    tokio::task::spawn_blocking(move || f(&repo_a, &repo_b))
        .await
        .map_err(|e| ApiError::internal(format!("blocking task failed: {e}")))?
}

/// An opened per-pair sync database plus the repo hosting its file.
pub(super) struct PairDb {
    conn: sync::SyncDb,
    host: Uuid,
}

/// Locates and opens the pair's sync database (spec-sync "Location and
/// discovery"). `create_host` (a repo of the pair) creates the file under that
/// repo's `internal/` when absent; `None` is a read-only call, which returns
/// `None` for a pair that has no sync database yet.
fn open_pair_db(
    repo_a: &RepoState,
    repo_b: &RepoState,
    create_host: Option<Uuid>,
) -> Result<Option<PairDb>, ApiError> {
    let a = repo_a.config.repo_uuid;
    let b = repo_b.config.repo_uuid;
    let a_int = repo_a.internal_dir();
    let b_int = repo_b.internal_dir();

    match sync::locate(&a_int, &b_int, a, b) {
        sync::Located::Ambiguous => {
            Err(ApiError::conflict("sync database present in both repos; delete the stale copy"))
        }
        sync::Located::Found(path) => {
            let host = if path.starts_with(&a_int) { a } else { b };
            let conn = sync::open(&path)?;
            let ok = sync::read_meta(&conn, "repo_a")?.as_deref()
                == Some(&a.as_simple().to_string())
                && sync::read_meta(&conn, "repo_b")?.as_deref() == Some(&b.as_simple().to_string());
            if !ok {
                return Err(ApiError::conflict("sync database identity mismatch"));
            }
            // Relocation is supported: keep meta.host in step with the file's
            // actual location.
            if sync::read_meta(&conn, "host")?.as_deref() != Some(&host.as_simple().to_string()) {
                sync::write_meta(&conn, a, b, host)?;
            }
            Ok(Some(PairDb { conn, host }))
        }
        sync::Located::Absent => match create_host {
            None => Ok(None),
            Some(host) => {
                let dir = if host == a { &a_int } else { &b_int };
                std::fs::create_dir_all(dir)
                    .map_err(|e| ApiError::internal(format!("create internal/: {e}")))?;
                let conn = sync::open(&dir.join(sync::sync_db_filename(a, b)))?;
                sync::write_meta(&conn, a, b, host)?;
                Ok(Some(PairDb { conn, host }))
            }
        },
    }
}

fn link_json(l: &sync::Link) -> serde_json::Value {
    json!({
        "uuid": hex(l.uuid),
        "record_a": hex(l.record_a),
        "record_b": hex(l.record_b),
        "version_a": l.version_a,
        "version_b": l.version_b,
    })
}

/// `GET /sync/:a/:b/links` — list links (read-only).
pub(super) async fn sync_list_links(
    State(state): State<Arc<AppState>>,
    Path((a, b)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    with_pair(&state, &a, &b, move |repo_a, repo_b| {
        let (ra, rb) = (repo_a.config.repo_uuid, repo_b.config.repo_uuid);
        let (host, links) = match open_pair_db(repo_a, repo_b, None)? {
            None => (serde_json::Value::Null, vec![]),
            Some(db) => (json!(hex(db.host)), sync::list_links(&db.conn)?),
        };
        Ok(Json(json!({
            "repo_a": hex(ra),
            "repo_b": hex(rb),
            "host": host,
            "links": links.iter().map(link_json).collect::<Vec<_>>(),
        })))
    })
    .await
}

#[derive(Deserialize)]
pub(super) struct CreateLinkBody {
    record_a: String,
    record_b: String,
    #[serde(default)]
    host: Option<String>,
}

/// `POST /sync/:a/:b/links` — create a link (canonical roles).
pub(super) async fn sync_create_link(
    State(state): State<Arc<AppState>>,
    Path((a, b)): Path<(String, String)>,
    payload: Result<Json<CreateLinkBody>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Json(body) = payload?;
    with_pair(&state, &a, &b, move |repo_a, repo_b| {
        let ra = repo_a.config.repo_uuid;
        let rb = repo_b.config.repo_uuid;
        let record_a = parse_uuid(&body.record_a)?;
        let record_b = parse_uuid(&body.record_b)?;
        // Both endpoint metarecords must exist.
        if Rows::version(&*repo_a.conn.lock_recover(), record_a)?.is_none() {
            return Err(ApiError::not_found(format!("metarecord not found in repo_a: {record_a}")));
        }
        if Rows::version(&*repo_b.conn.lock_recover(), record_b)?.is_none() {
            return Err(ApiError::not_found(format!("metarecord not found in repo_b: {record_b}")));
        }
        let host = match &body.host {
            Some(h) => {
                let h = parse_uuid(h)?;
                if h != ra && h != rb {
                    return Err(ApiError::bad_request("host must be one of the pair"));
                }
                h
            }
            None => ra,
        };
        let db = open_pair_db(repo_a, repo_b, Some(host))?.expect("create_host given");
        if sync::link_for_record(&db.conn, sync::Side::A, record_a)?.is_some()
            || sync::link_for_record(&db.conn, sync::Side::B, record_b)?.is_some()
        {
            return Err(ApiError::conflict("a record is already linked in this pair"));
        }
        let link = sync::create_link(&db.conn, record_a, record_b)?;
        Ok(Json(link_json(&link)))
    })
    .await
}

/// `GET /sync/:a/:b/links/:link` — one link, its decoded snapshot, and both
/// endpoint metarecords inline.
pub(super) async fn sync_get_link(
    State(state): State<Arc<AppState>>,
    Path((a, b, link)): Path<(String, String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    with_pair(&state, &a, &b, move |repo_a, repo_b| {
        let link_uuid = parse_uuid(&link)?;
        let Some(db) = open_pair_db(repo_a, repo_b, None)? else {
            return Err(ApiError::not_found("no sync database for this pair"));
        };
        let Some(l) = sync::get_link(&db.conn, link_uuid)? else {
            return Err(ApiError::not_found(format!("link not found: {link_uuid}")));
        };
        let snapshot: Vec<serde_json::Value> = sync::read_snapshot(&db.conn, link_uuid)?
            .into_iter()
            .map(|f| {
                json!({
                    "name": f.name,
                    "value": &f.value,
                    "value_b": f.value_uuid_b.map(hex),
                    "side": f.side.map(sync::Side::name),
                })
            })
            .collect();
        let record_a = metarecord_response(&repo_a.conn.lock_recover(), l.record_a).ok();
        let record_b = metarecord_response(&repo_b.conn.lock_recover(), l.record_b).ok();
        Ok(Json(json!({
            "link": link_json(&l),
            "snapshot": snapshot,
            "record_a": record_a,
            "record_b": record_b,
        })))
    })
    .await
}

#[derive(Deserialize, Default)]
pub(super) struct WithEndpointParam {
    with_endpoint: Option<String>,
}

/// `DELETE /sync/:a/:b/links/:link[?with_endpoint=a|b]` — delete a link (and its
/// snapshot). With `with_endpoint`, the endpoint metarecord on that side is
/// deleted first (spec-sync "Metarecord deletion propagation" normative order).
pub(super) async fn sync_delete_link(
    State(state): State<Arc<AppState>>,
    Path((a, b, link)): Path<(String, String, String)>,
    Query(ep): Query<WithEndpointParam>,
) -> Result<Json<serde_json::Value>, ApiError> {
    with_pair(&state, &a, &b, move |repo_a, repo_b| {
        let link_uuid = parse_uuid(&link)?;
        let Some(db) = open_pair_db(repo_a, repo_b, None)? else {
            return Err(ApiError::not_found("no sync database for this pair"));
        };
        let Some(l) = sync::get_link(&db.conn, link_uuid)? else {
            return Err(ApiError::not_found(format!("link not found: {link_uuid}")));
        };
        if let Some(side) = ep.with_endpoint.as_deref() {
            let (repo, record) = match side {
                "a" => (repo_a, l.record_a),
                "b" => (repo_b, l.record_b),
                _ => return Err(ApiError::bad_request("with_endpoint must be 'a' or 'b'")),
            };
            repo.ensure_writable()?;
            let mut conn = repo.conn.lock_recover();
            if Rows::version(&*conn, record)?.is_some() {
                let mut writer = repo.writer(&mut conn, None)?;
                writer.delete_metarecord(record)?;
                let effects = writer.effects();
                slowlog::timed("commit", || writer.commit())?;
                repo.settle(&conn, &effects)?;
            }
        }
        sync::delete_link(&db.conn, link_uuid)?;
        Ok(Json(json!({ "deleted": true })))
    })
    .await
}

/// `GET /sync/:a/:b/status` — per-link change/conflict state (spec-sync
/// truth table). Read-only.
pub(super) async fn sync_status(
    State(state): State<Arc<AppState>>,
    Path((a, b)): Path<(String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    with_pair(&state, &a, &b, move |repo_a, repo_b| {
        let (ra, rb) = (repo_a.config.repo_uuid, repo_b.config.repo_uuid);
        let links = match open_pair_db(repo_a, repo_b, None)? {
            None => vec![],
            Some(db) => sync::list_links(&db.conn)?,
        };
        let conn_a = repo_a.conn.lock_recover();
        let conn_b = repo_b.conn.lock_recover();
        let mut out = Vec::with_capacity(links.len());
        for l in &links {
            let ea = Rows::version(&*conn_a, l.record_a)?;
            let eb = Rows::version(&*conn_b, l.record_b)?;
            let state = link_state(ea, eb, l.version_a, l.version_b);
            out.push(json!({
                "uuid": hex(l.uuid),
                "state": state,
                "e_a_version": ea,
                "e_b_version": eb,
                "version_a": l.version_a,
                "version_b": l.version_b,
            }));
        }
        Ok(Json(json!({ "repo_a": hex(ra), "repo_b": hex(rb), "links": out })))
    })
    .await
}

/// The change-detection state of a link (spec-sync "status"), by precedence.
fn link_state(ea: Option<u64>, eb: Option<u64>, va: Option<u64>, vb: Option<u64>) -> &'static str {
    match (ea.is_none(), eb.is_none()) {
        (true, true) => return "missing_both",
        (true, false) => return "missing_a",
        (false, true) => return "missing_b",
        (false, false) => {}
    }
    let (Some(va), Some(vb)) = (va, vb) else {
        return "never_synced";
    };
    match (ea != Some(va), eb != Some(vb)) {
        (false, false) => "in_sync",
        (true, false) => "ahead_a",
        (false, true) => "ahead_b",
        (true, true) => "conflict",
    }
}

#[derive(Deserialize)]
pub(super) struct CommitBody {
    commits: Vec<CommitEntry>,
}

#[derive(Deserialize)]
pub(super) struct CommitEntry {
    link: String,
    version_a: u64,
    version_b: u64,
    #[serde(default)]
    snapshot: Vec<SnapshotEntry>,
}

#[derive(Deserialize)]
pub(super) struct SnapshotEntry {
    name: String,
    value: Value,
    #[serde(default)]
    value_b: Option<String>,
    /// `"a"` / `"b"`: an entry one repository alone held (absent: common).
    #[serde(default)]
    side: Option<String>,
}

/// `POST /sync/:a/:b/links/commit` — batched sync-commit (spec-sync).
pub(super) async fn sync_commit(
    State(state): State<Arc<AppState>>,
    Path((a, b)): Path<(String, String)>,
    payload: Result<Json<CommitBody>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Json(body) = payload?;
    with_pair(&state, &a, &b, move |repo_a, repo_b| {
        let db = open_pair_db(repo_a, repo_b, Some(repo_a.config.repo_uuid))?
            .expect("create_host given");
        let mut commits = Vec::with_capacity(body.commits.len());
        for c in body.commits {
            let snapshot = c
                .snapshot
                .into_iter()
                .map(|f| {
                    let value_uuid_b = match f.value_b {
                        Some(h) => Some(parse_uuid(&h)?),
                        None => None,
                    };
                    let side = match f.side.as_deref() {
                        None => None,
                        Some(s) => Some(sync::Side::parse(s).map_err(|e| {
                            ApiError::bad_request(format!("snapshot entry '{}': {e}", f.name))
                        })?),
                    };
                    Ok(sync::SnapshotField { name: f.name, value: f.value, value_uuid_b, side })
                })
                .collect::<Result<Vec<_>, ApiError>>()?;
            commits.push(sync::Commit {
                link: parse_uuid(&c.link)?,
                version_a: c.version_a,
                version_b: c.version_b,
                snapshot,
            });
        }
        let n = commits.len();
        sync::commit_batch(&db.conn, &commits)?;
        Ok(Json(json!({ "committed": n })))
    })
    .await
}
