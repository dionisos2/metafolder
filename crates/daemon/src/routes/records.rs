//! Single metarecords (spec-data-model): create (one or in bulk), read,
//! delete, trash, per-name field access, and field rows by id.

use super::*;

#[derive(Deserialize)]
pub(super) struct CreateBody {
    fields: Vec<Field>,
    #[serde(default)]
    force: bool,
    /// Optional caller-supplied UUID (sync bare-record creation, spec-sync).
    /// Rejected with 409 if a metarecord already has it.
    #[serde(default)]
    uuid: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct TrashDeleteBody {
    uuids: Vec<String>,
    /// Delete even when something outside the set still references one of them.
    #[serde(default)]
    force: bool,
}

/// The metarecords, outside `targets`, that hold a reference to one of them.
///
/// Served from the index's reverse maps: the repository's reference-typed field
/// names come from the catalogue (a handful), and each one answers "who points
/// at this set" with one bitmap lookup per target. Proportional to the number of
/// reference-typed *fields*, never to the number of rows — it must not become a
/// scan of the value columns (spec-main, the `INDEXED BY` invariant).
///
/// `ExternalRef` and `RefBase` values do not populate those reverse maps, so an
/// inbound reference of either kind is not seen. The gap is named in doc "What
/// trashing does" rather than left to be discovered.
fn inbound_referrers(
    conn: &dyn crate::store::Store,
    cache: &crate::tree_cache::TreeCache,
    targets: &[Uuid],
) -> Result<Vec<Uuid>, ApiError> {
    let fields: Vec<String> = {
        let engine = engine(conn)?;
        ["ref", "tree_ref"]
            .iter()
            .flat_map(|ty| engine.field_catalog(Some(ty)))
            .map(|(name, _)| name)
            .collect()
    };
    let in_set: std::collections::HashSet<Uuid> = targets.iter().copied().collect();
    let mut referrers: Vec<Uuid> = Vec::new();
    // Chunked so the combinator-width cap cannot be reached by a repository
    // with an implausible number of reference fields.
    for chunk in fields.chunks(128) {
        let operands: Vec<MetaQuery> = chunk
            .iter()
            .map(|field| MetaQuery::Follows {
                field: field.clone(),
                target: FollowTarget::Condition(Box::new(MetaQuery::UuidIn {
                    uuids: targets.to_vec(),
                })),
            })
            .collect();
        let query = match operands.len() {
            0 => continue,
            1 => operands.into_iter().next().expect("one operand"),
            _ => MetaQuery::Or { operands },
        };
        for uuid in resolve_query_uuids(conn, cache, &query)? {
            // A reference from inside the set travels with it and comes back on
            // a restore: only the ones from outside would be left dangling.
            if !in_set.contains(&uuid) && !referrers.contains(&uuid) {
                referrers.push(uuid);
            }
        }
    }
    Ok(referrers)
}

/// `POST /repos/:repo/metarecords/trash`: deletes metarecords on the
/// trash-bin's behalf, in one revision stamped `origin = 'trash'`
/// (doc "POST /repos/:repo/metarecords/trash").
///
/// The trash-bin's two halves have different owners: the bytes are the client's
/// business, the data model is the daemon's, and neither reaches into the
/// other's. This is the daemon's half.
///
/// The origin is fixed by *which endpoint was called*, never by a parameter — a
/// client able to name its own origin could pass its writes off as the daemon's
/// and put them out of reach of undo. That is why this is a route of its own
/// rather than a flag on `POST …/query/delete`.
///
/// No record-count cap: uuids are 32 characters each, so axum's 2 MiB body
/// limit already bounds a call at tens of thousands of them, and a trashing has
/// to be *one* revision — paging it would break the thing it is for.
pub(super) async fn trash_delete_endpoint(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Result<Json<TrashDeleteBody>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    let uuids: Vec<Uuid> =
        body.uuids.iter().map(|u| parse_uuid(u)).collect::<Result<_, ApiError>>()?;

    with_repo(&state, repo_uuid, move |repo_state| {
        repo_state.ensure_writable()?;
        let mut conn = slowlog::timed("wait:conn", || repo_state.conn.lock_recover());

        if !body.force {
            let cache = repo_state.tree();
            let referrers = inbound_referrers(&conn, &cache, &uuids)?;
            if !referrers.is_empty() {
                let named: Vec<String> =
                    referrers.iter().take(10).map(|u| u.as_simple().to_string()).collect();
                return Err(ApiError::conflict(format!(
                    "{} metarecord(s) outside the set still reference it: {} — \
                     use force to trash it anyway",
                    referrers.len(),
                    named.join(", ")
                )));
            }
        }

        let mut writer = repo_state.writer(&mut conn, None)?;
        writer.set_origin(crate::log::ORIGIN_TRASH)?;
        let _phase = slowlog::phase("write.trash_delete");
        for uuid in &uuids {
            // Every way of ceasing to be a live duplicate goes through here, so
            // a group never outlives its members (doc "Duplicates").
            crate::duplicates::leave_group(&mut writer, crate::log::OpType::SetField, *uuid)?;
            writer.delete_metarecord(*uuid)?;
        }
        let effects = writer.effects();
        slowlog::timed("commit", || writer.commit())?;
        repo_state.settle(&conn, &effects)?;
        slowlog::note("deleted", uuids.len().to_string());
        Ok(Json(json!({ "deleted": uuids.len() })))
    })
    .await
}

/// The most metarecords one bulk create may carry. A cap in the shape of
/// [`ELIGIBILITY_MAX_PATHS`]: checked before any work, and the error names it so
/// the caller knows what to page by. Bodies are capped independently by axum's
/// 2 MiB `Json` limit, so a caller with fat metarecords pages sooner than this.
const BULK_CREATE_MAX_RECORDS: usize = 1000;

#[derive(Deserialize)]
pub(super) struct BulkCreateRecord {
    fields: Vec<Field>,
    /// Optional caller-supplied UUID, as on the single-record form.
    #[serde(default)]
    uuid: Option<String>,
}

#[derive(Deserialize)]
pub(super) struct BulkCreateBody {
    metarecords: Vec<BulkCreateRecord>,
    /// Applies to every metarecord of the batch: the one caller that needs it
    /// (restoring `mfr_path` from the trash) needs it uniformly.
    #[serde(default)]
    force: bool,
    /// A UUID already taken is skipped and counted, instead of failing the
    /// batch. What a subtree restore needs when the watcher has re-tracked a
    /// node already, and what sync's bare-record creation does by hand today.
    #[serde(default)]
    skip_existing: bool,
}

/// `POST /repos/:repo/metarecords/bulk`: creates many metarecords, each at its
/// own (optional) caller-supplied UUID, in **one revision**
/// (doc "Metarecord endpoints").
///
/// All-or-nothing, like every other batch writer here: one [`Writer`], one
/// transaction, and the first refusal rolls the whole batch back. Records are
/// applied **in the order given** — the forest rejects a `TreeRef` whose parent
/// has no row yet, so a subtree must arrive parent-first.
///
/// Nothing carries a version: a metarecord's version is a hash of its content
/// (doc "Metarecord version"), so recreating one at its UUID with its fields
/// gives it back exactly the version it had.
pub(super) async fn bulk_create_endpoint(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Result<Json<BulkCreateBody>, JsonRejection>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    if body.metarecords.len() > BULK_CREATE_MAX_RECORDS {
        return Err(ApiError::bad_request(format!(
            "too many metarecords in one bulk create: {} (limit {BULK_CREATE_MAX_RECORDS}) — \
             send them in several calls",
            body.metarecords.len()
        )));
    }
    // Parsed up front so a malformed uuid costs no repository work.
    let supplied: Vec<Option<Uuid>> = body
        .metarecords
        .iter()
        .map(|r| r.uuid.as_deref().map(parse_uuid).transpose())
        .collect::<Result<_, _>>()?;

    with_repo(&state, repo_uuid, move |repo_state| {
        repo_state.ensure_writable()?;
        for record in &body.metarecords {
            for field in &record.fields {
                check_writable(&field.name, body.force)?;
            }
        }
        let mut conn = slowlog::timed("wait:conn", || repo_state.conn.lock_recover());
        let mut writer = repo_state.writer(&mut conn, None)?;
        let writing = slowlog::phase("write.bulk_create");
        let mut created = 0usize;
        let mut skipped = 0usize;
        let mut uuids: Vec<String> = Vec::with_capacity(body.metarecords.len());
        for (record, supplied) in body.metarecords.into_iter().zip(supplied) {
            let touched: Vec<String> = record.fields.iter().map(|f| f.name.clone()).collect();
            let made = match supplied {
                Some(uuid) => {
                    if Rows::version(writer.store(), uuid)?.is_some() {
                        if body.skip_existing {
                            skipped += 1;
                            uuids.push(uuid.as_simple().to_string());
                            continue;
                        }
                        return Err(ApiError::conflict(format!(
                            "metarecord already exists: {uuid}"
                        )));
                    }
                    writer.create_metarecord_with_uuid(uuid, record.fields)?
                }
                None => writer.create_metarecord(record.fields)?,
            };
            slowlog::timed("validate.schema", || {
                validate_schema(repo_state, writer.store(), made.uuid, &touched)
            })?;
            created += 1;
            uuids.push(made.uuid.as_simple().to_string());
        }
        drop(writing);
        let effects = writer.effects();
        slowlog::timed("commit", || writer.commit())?;
        repo_state.settle(&conn, &effects)?;
        slowlog::note("created", created.to_string());
        Ok(Json(json!({ "created": created, "skipped": skipped, "uuids": uuids })))
    })
    .await
}

pub(super) async fn create_record_endpoint(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Result<Json<CreateBody>, JsonRejection>,
) -> Result<Json<MetaRecord>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    let supplied = body.uuid.as_deref().map(parse_uuid).transpose()?;
    with_repo(&state, repo_uuid, move |repo_state| {
        repo_state.ensure_writable()?;
        for field in &body.fields {
            check_writable(&field.name, body.force)?;
        }
        let mut conn = slowlog::timed("wait:conn", || repo_state.conn.lock_recover());
        let touched: Vec<String> = body.fields.iter().map(|f| f.name.clone()).collect();
        let mut writer = repo_state.writer(&mut conn, None)?;
        let created = match supplied {
            Some(uuid) => {
                if Rows::version(writer.store(), uuid)?.is_some() {
                    return Err(ApiError::conflict(format!("metarecord already exists: {uuid}")));
                }
                writer.create_metarecord_with_uuid(uuid, body.fields)?
            }
            None => writer.create_metarecord(body.fields)?,
        };
        validate_schema(repo_state, writer.store(), created.uuid, &touched)?;
        let effects = writer.effects();
        slowlog::timed("commit", || writer.commit())?;
        repo_state.settle(&conn, &effects)?;
        Ok(Json(created))
    })
    .await
}

pub(super) async fn get_record_endpoint(
    State(state): State<Arc<AppState>>,
    Path((repo, uuid)): Path<(String, String)>,
) -> Result<Json<MetaRecord>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    let uuid = parse_uuid(&uuid)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        let conn = slowlog::timed("wait:conn", || repo_state.conn.lock_recover());
        Ok(Json(metarecord_response(&conn, uuid)?))
    })
    .await
}

pub(super) async fn delete_record_endpoint(
    State(state): State<Arc<AppState>>,
    Path((repo, uuid)): Path<(String, String)>,
    Query(ev): Query<ExpectedVersion>,
) -> Result<StatusCode, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    let uuid = parse_uuid(&uuid)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        repo_state.ensure_writable()?;
        let mut conn = slowlog::timed("wait:conn", || repo_state.conn.lock_recover());
        if Rows::version(&*conn, uuid)?.is_none() {
            return Err(ApiError::not_found(format!("Metarecord not found: {uuid}")));
        }
        let mut writer = repo_state.writer(&mut conn, None)?;
        ensure_version(writer.store(), uuid, ev.expected_version)?;
        writer.delete_metarecord(uuid)?;
        let effects = writer.effects();
        slowlog::timed("commit", || writer.commit())?;
        repo_state.settle(&conn, &effects)?;
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}

#[derive(Deserialize)]
pub(super) struct SetFieldBody {
    name: String,
    #[serde(default)]
    value: Option<Value>,
    #[serde(default)]
    values: Option<Vec<Value>>,
    #[serde(default)]
    force: bool,
}

#[derive(Deserialize)]
pub(super) struct RecordFieldBody {
    #[serde(default)]
    value: Option<Value>,
    #[serde(default)]
    values: Option<Vec<Value>>,
    #[serde(default)]
    force: bool,
}

/// `GET /repos/:repo/metarecords/:uuid/fields/:name` — the field's value(s).
pub(super) async fn get_record_field(
    State(state): State<Arc<AppState>>,
    Path((repo, uuid, name)): Path<(String, String, String)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    let uuid = parse_uuid(&uuid)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        let conn = slowlog::timed("wait:conn", || repo_state.conn.lock_recover());
        ensure_exists(&conn, uuid)?;
        let rows = Rows::rows_named(&*conn, uuid, &name)?;
        let values: Vec<&Value> = rows.iter().map(|r| &r.value).collect();
        Ok(Json(json!({ "name": name, "values": values })))
    })
    .await
}

/// `PUT /repos/:repo/metarecords/:uuid/fields/:name` — set: replaces all rows of
/// `name` (one `SetField` op). `value` (one row) or `values` (multi-map).
pub(super) async fn set_record_field(
    State(state): State<Arc<AppState>>,
    Path((repo, uuid, name)): Path<(String, String, String)>,
    Query(ev): Query<ExpectedVersion>,
    payload: Result<Json<RecordFieldBody>, JsonRejection>,
) -> Result<Json<MetaRecord>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    let uuid = parse_uuid(&uuid)?;
    let rows = resolved_values(body.value, body.values)?;
    write_record_checked(&state, repo_uuid, uuid, ev.expected_version, move |writer| {
        check_writable(&name, body.force)?;
        ensure_exists(writer.store(), uuid)?;
        writer.set_field_multi(uuid, &name, rows)?;
        Ok(vec![name])
    })
    .await
    .map(Json)
}

/// `DELETE /repos/:repo/metarecords/:uuid/fields/:name` — unset: removes every
/// row of `name` (one `DeleteField` op), leaving the field unknown.
pub(super) async fn unset_record_field(
    State(state): State<Arc<AppState>>,
    Path((repo, uuid, name)): Path<(String, String, String)>,
    Query(ev): Query<ExpectedVersion>,
    payload: Option<Json<ForceBody>>,
) -> Result<StatusCode, ApiError> {
    let force = payload.map(|Json(b)| b.force).unwrap_or(false);
    let repo_uuid = parse_uuid(&repo)?;
    let uuid = parse_uuid(&uuid)?;
    write_record_checked(&state, repo_uuid, uuid, ev.expected_version, move |writer| {
        check_writable(&name, force)?;
        ensure_exists(writer.store(), uuid)?;
        writer.delete_fields_named(uuid, &name)?;
        Ok(vec![name])
    })
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
pub(super) struct SetRecordBody {
    fields: Vec<Field>,
    #[serde(default)]
    force: bool,
}

/// `PUT /repos/:repo/metarecords/:uuid` — whole-record set: replaces the entire
/// field set, keeping the UUID, as one `SetRecord` op (doc "Metarecord endpoints"). Literal
/// overwrite; reserved field names still need `force` to be written.
pub(super) async fn put_metarecord(
    State(state): State<Arc<AppState>>,
    Path((repo, uuid)): Path<(String, String)>,
    Query(ev): Query<ExpectedVersion>,
    payload: Result<Json<SetRecordBody>, JsonRejection>,
) -> Result<Json<MetaRecord>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    let uuid = parse_uuid(&uuid)?;
    write_record_checked(&state, repo_uuid, uuid, ev.expected_version, move |writer| {
        for field in &body.fields {
            check_writable(&field.name, body.force)?;
        }
        ensure_exists(writer.store(), uuid)?;
        let touched: Vec<String> = body.fields.iter().map(|f| f.name.clone()).collect();
        writer.set_record(uuid, body.fields)?;
        Ok(touched)
    })
    .await
    .map(Json)
}

pub(super) async fn append_field(
    State(state): State<Arc<AppState>>,
    Path((repo, uuid)): Path<(String, String)>,
    Query(ev): Query<ExpectedVersion>,
    payload: Result<Json<SetFieldBody>, JsonRejection>,
) -> Result<Json<MetaRecord>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    let uuid = parse_uuid(&uuid)?;
    let value = single_value(body.value, body.values)?;
    write_record_checked(&state, repo_uuid, uuid, ev.expected_version, move |writer| {
        check_writable(&body.name, body.force)?;
        slowlog::note("field", body.name.as_str());
        ensure_exists(writer.store(), uuid)?;
        writer.append_field(uuid, &body.name, value)?;
        Ok(vec![body.name])
    })
    .await
    .map(Json)
}

#[derive(Deserialize, Default)]
pub(super) struct ForceBody {
    #[serde(default)]
    force: bool,
}

// ── By-id field access (repo-level: the row id is unique per repo) ────────────

/// 404 unless field row `id` exists in this repo; returns its owning metarecord.
fn field_owner(conn: &dyn crate::store::Store, id: i64) -> Result<Uuid, ApiError> {
    Rows::owner_of_row(conn, id)?
        .ok_or_else(|| ApiError::not_found(format!("Field {id} not found")))
}

/// `GET /repos/:repo/fields/:id` — read one field row by its id (`mf field get`).
pub(super) async fn get_field_by_id(
    State(state): State<Arc<AppState>>,
    Path((repo, id)): Path<(String, i64)>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        let conn = slowlog::timed("wait:conn", || repo_state.conn.lock_recover());
        let row = Rows::row(&*conn, id)?
            .ok_or_else(|| ApiError::not_found(format!("Field {id} not found")))?;
        Ok(Json(json!({"id": row.id, "name": row.name, "value": row.value})))
    })
    .await
}

#[derive(Deserialize)]
pub(super) struct PatchFieldByIdBody {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    value: Option<Value>,
    #[serde(default)]
    force: bool,
}

/// `PATCH /repos/:repo/fields/:id` — change a row's name and/or value in place,
/// keeping its id (`mf field set`). The value type is validated against the
/// target name; reserved names (old or new) need `force`.
pub(super) async fn patch_field_by_id(
    State(state): State<Arc<AppState>>,
    Path((repo, id)): Path<(String, i64)>,
    payload: Result<Json<PatchFieldByIdBody>, JsonRejection>,
) -> Result<Json<MetaRecord>, ApiError> {
    let Json(body) = payload?;
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        repo_state.ensure_writable()?;
        let mut conn = slowlog::timed("wait:conn", || repo_state.conn.lock_recover());
        let uuid = field_owner(&conn, id)?;
        let old = Rows::row(&*conn, id)?
            .ok_or_else(|| ApiError::not_found(format!("Field {id} not found")))?;
        let new_name = body.name.clone().unwrap_or_else(|| old.name.clone());
        let new_value = body.value.clone().unwrap_or_else(|| old.value.clone());
        check_writable(&old.name, body.force)?;
        check_writable(&new_name, body.force)?;

        let mut writer = repo_state.writer(&mut conn, None)?;
        writer.rename_field(uuid, id, &new_name, new_value)?;
        validate_schema(repo_state, writer.store(), uuid, &[old.name.clone(), new_name.clone()])?;
        let effects = writer.effects();
        slowlog::timed("commit", || writer.commit())?;
        repo_state.settle(&conn, &effects)?;
        metarecord_response(&conn, uuid).map(Json)
    })
    .await
}

/// `DELETE /repos/:repo/fields/:id` — remove one row by id (`mf field delete`).
pub(super) async fn delete_field_by_id(
    State(state): State<Arc<AppState>>,
    Path((repo, id)): Path<(String, i64)>,
    payload: Option<Json<ForceBody>>,
) -> Result<StatusCode, ApiError> {
    let force = payload.map(|Json(b)| b.force).unwrap_or(false);
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        repo_state.ensure_writable()?;
        let mut conn = slowlog::timed("wait:conn", || repo_state.conn.lock_recover());
        let uuid = field_owner(&conn, id)?;
        let row = Rows::row(&*conn, id)?
            .ok_or_else(|| ApiError::not_found(format!("Field {id} not found")))?;
        check_writable(&row.name, force)?;
        let mut writer = repo_state.writer(&mut conn, None)?;
        writer.delete_field(uuid, id)?;
        validate_schema(repo_state, writer.store(), uuid, std::slice::from_ref(&row.name))?;
        let effects = writer.effects();
        slowlog::timed("commit", || writer.commit())?;
        repo_state.settle(&conn, &effects)?;
        Ok(StatusCode::NO_CONTENT)
    })
    .await
}
