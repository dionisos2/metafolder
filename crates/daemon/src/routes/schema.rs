//! The user schema (spec-schema): read, reload, check.

use super::*;

pub(super) async fn get_schema(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    let repo_state = state.ready_repo(repo_uuid)?;
    let guard = repo_state.schema.lock_recover();
    Ok(Json(match guard.as_ref() {
        Some(schema) => schema.raw().clone(),
        None => crate::schema::CompiledSchema::empty_raw(),
    }))
}

/// Re-reads the schema file; on error the previous schema stays in effect.
pub(super) async fn reload_schema(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        let loaded = crate::schema::load_for_repo(&repo_state.metafolder_dir, &repo_state.config)
            .map_err(ApiError::bad_request)?;
        let raw = loaded
            .as_ref()
            .map(|s| s.raw().clone())
            .unwrap_or_else(crate::schema::CompiledSchema::empty_raw);
        *repo_state.schema.lock_recover() = loaded;
        Ok(Json(raw))
    })
    .await
}

#[derive(Deserialize, Default)]
pub(super) struct CheckBody {
    #[serde(default)]
    query: Option<MetaQuery>,
    /// Cap on the number of violations returned. The scan stops once it is
    /// exceeded (so a huge repo never builds an unusable response); the response
    /// then carries `truncated: true`. `None` returns every violation.
    #[serde(default)]
    limit: Option<usize>,
}

/// Scans metarecords and reports constraint violations (the schema file is never
/// validated retroactively on edit). With `limit`, stops after that many and
/// flags `truncated`.
pub(super) async fn check_schema(
    State(state): State<Arc<AppState>>,
    Path(repo): Path<String>,
    payload: Option<Json<CheckBody>>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let body = payload.map(|Json(b)| b).unwrap_or_default();
    let repo_uuid = parse_uuid(&repo)?;
    with_repo(&state, repo_uuid, move |repo_state| {
        let conn = slowlog::timed("wait:conn", || repo_state.conn.lock_recover());
        let guard = repo_state.schema.lock_recover();
        let mut violations: Vec<serde_json::Value> = Vec::new();
        let mut checked = 0usize;
        if let Some(schema) = guard.as_ref() {
            // Which metarecords to validate. A scoped check (`query`) walks that
            // (usually small) result set. A whole-repo check does NOT scan every
            // metarecord: it validates only the ones the index-served candidate
            // queries flag as *able* to violate — nearly none on a healthy repo,
            // so the once-per-open heads-up stays cheap even at 400k records.
            let uuids = match &body.query {
                Some(query) => {
                    let cache = repo_state.tree();
                    resolve_query_uuids(&conn, &cache, query)?
                }
                None => crate::schema::violation_candidates(
                    schema,
                    &conn,
                    // A cap on candidates suffices once we only need `limit + 1`
                    // violations to report truncation; unbounded for a full audit.
                    body.limit.map(|l| l + 1),
                )?,
            };
            let fields = schema.constrained_fields();
            // Collect up to `limit + 1` so truncation is exact, then stop.
            'scan: for uuid in &uuids {
                checked += 1;
                for violation in
                    crate::schema::validate_entry_fields(schema, &conn, *uuid, &fields)?
                {
                    violations
                        .push(serde_json::to_value(&violation).expect("violation serialization"));
                    if body.limit.is_some_and(|l| violations.len() > l) {
                        break 'scan;
                    }
                }
            }
            // For a whole-repo check that ran to completion, report the true
            // repository size as `checked`: the candidate set is an exhaustive
            // superset of the violators, so every metarecord was effectively
            // examined (the non-candidates are provably clean).
            let truncated = body.limit.is_some_and(|l| violations.len() > l);
            if body.query.is_none() && !truncated {
                checked = Rows::metarecord_count(&*conn)?;
            }
        }
        let truncated = body.limit.is_some_and(|l| violations.len() > l);
        if let Some(l) = body.limit {
            violations.truncate(l);
        }
        // `checked` is the number of metarecords actually examined — fewer than
        // the total when the scan stopped early at the cap.
        Ok(Json(json!({"checked": checked, "violations": violations, "truncated": truncated})))
    })
    .await
}
