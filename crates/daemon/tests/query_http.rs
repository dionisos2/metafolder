//! HTTP-level tests for `POST /query` (select, sort, pagination envelope)
//! and `POST /set` (batch set).

use std::path::PathBuf;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use metafolder_daemon::routes;
use metafolder_daemon::state::AppState;
use serde_json::{json, Value};
use tower::util::ServiceExt;
use uuid::Uuid;

fn temp_dir(prefix: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!("metafolder_qhttp_{prefix}_{}", Uuid::new_v4()));
    std::fs::create_dir_all(&path).unwrap();
    path
}

async fn request(app: &Router, method: &str, uri: &str, body: Option<Value>) -> (StatusCode, Value) {
    let builder = Request::builder().method(method).uri(uri);
    let request = match body {
        Some(v) => builder
            .header("content-type", "application/json")
            .body(Body::from(v.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

async fn setup(prefix: &str) -> (Router, String, PathBuf) {
    let app = routes::build(std::sync::Arc::new(AppState::new()));
    let root = temp_dir(prefix);
    let (status, body) =
        request(&app, "POST", "/repos/init", Some(json!({"root": root.to_str().unwrap()}))).await;
    assert_eq!(status, StatusCode::OK, "init failed: {body}");
    let repo = body["repo_uuid"].as_str().unwrap().to_string();
    (app, repo, root)
}

async fn create(app: &Router, repo: &str, fields: Value) -> String {
    let (status, body) =
        request(app, "POST", &format!("/repos/{repo}/metarecords"), Some(json!({"fields": fields})))
            .await;
    assert_eq!(status, StatusCode::OK, "create failed: {body}");
    body["uuid"].as_str().unwrap().to_string()
}

#[tokio::test]
async fn test_query_returns_uuids_by_default() {
    let (app, repo, root) = setup("uuids").await;
    let a = create(
        &app,
        &repo,
        json!([{"name": "tag", "value": {"type": "string", "value": "jazz"}}]),
    )
    .await;
    let _b = create(
        &app,
        &repo,
        json!([{"name": "tag", "value": {"type": "string", "value": "rock"}}]),
    )
    .await;

    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/query"),
        Some(json!({"query": {"type": "eq", "field": "tag",
                              "value": {"type": "string", "value": "jazz"}}})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "query failed: {body}");
    assert_eq!(body, json!([a]));

    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn test_query_select_fields_and_star() {
    let (app, repo, root) = setup("select").await;
    let _ = create(
        &app,
        &repo,
        json!([
            {"name": "tag", "value": {"type": "string", "value": "jazz"}},
            {"name": "rating", "value": {"type": "int", "value": 5}},
            {"name": "note", "value": {"type": "string", "value": "great"}}
        ]),
    )
    .await;

    let query = json!({"type": "eq", "field": "tag", "value": {"type": "string", "value": "jazz"}});

    // Restricted field list.
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/query"),
        Some(json!({"query": query, "select": ["rating"]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "query failed: {body}");
    let items = body.as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert!(items[0]["uuid"].is_string());
    assert!(items[0]["version"].is_u64());
    let names: Vec<&str> =
        items[0]["fields"].as_array().unwrap().iter().map(|f| f["name"].as_str().unwrap()).collect();
    assert_eq!(names, vec!["rating"], "only selected fields are included");

    // Full objects with "*".
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/query"),
        Some(json!({"query": query, "select": "*"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body[0]["fields"].as_array().unwrap().len(), 3);

    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn test_query_sort_and_pagination_envelope() {
    let (app, repo, root) = setup("sortpage").await;
    for i in 0..5 {
        create(
            &app,
            &repo,
            json!([
                {"name": "n", "value": {"type": "int", "value": i}},
                {"name": "k", "value": {"type": "string", "value": "x"}}
            ]),
        )
        .await;
    }
    let query = json!({"type": "eq", "field": "k", "value": {"type": "string", "value": "x"}});
    let body = json!({
        "query": query,
        "sort": [{"field": "n", "order": "desc"}],
        "limit": 3
    });
    let (status, page1) =
        request(&app, "POST", &format!("/repos/{repo}/query"), Some(body.clone())).await;
    assert_eq!(status, StatusCode::OK, "query failed: {page1}");
    assert_eq!(page1["results"].as_array().unwrap().len(), 3);
    let cursor = page1["next_cursor"].as_str().expect("cursor expected").to_string();

    let mut body2 = body.clone();
    body2["cursor"] = json!(cursor);
    let (status, page2) =
        request(&app, "POST", &format!("/repos/{repo}/query"), Some(body2)).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(page2["results"].as_array().unwrap().len(), 2);
    assert!(page2["next_cursor"].is_null());

    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn test_query_count_reports_the_full_total() {
    let (app, repo, root) = setup("count").await;
    for i in 0..5 {
        create(&app, &repo, json!([{"name": "n", "value": {"type": "int", "value": i}}])).await;
    }
    let query = json!({"type": "is_present", "field": "n"});

    // The total covers the whole result set, not just the page.
    let (status, page) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/query"),
        Some(json!({"query": query, "limit": 2, "count": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "query failed: {page}");
    assert_eq!(page["results"].as_array().unwrap().len(), 2);
    assert_eq!(page["total"], json!(5));

    // Without count the field is absent entirely.
    let (_, page) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/query"),
        Some(json!({"query": query, "limit": 2})),
    )
    .await;
    assert!(page.get("total").is_none(), "total must be opt-in: {page}");

    // count without limit: the unwrapped array has nowhere to carry it.
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/query"),
        Some(json!({"query": query, "count": true})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn test_delete_by_query() {
    let (app, repo, _root) = setup("delete_query").await;
    for genre in ["jazz", "jazz", "rock"] {
        create(
            &app,
            &repo,
            json!([{"name": "genre", "value": {"type": "string", "value": genre}}]),
        )
        .await;
    }

    let revisions = |body: &serde_json::Value| body["revisions"].as_array().unwrap().len();
    let (_, before) = request(&app, "GET", &format!("/repos/{repo}/log"), None).await;
    let revs_before = revisions(&before);

    // Atomic predicate delete: one request, one revision, returns the count.
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/delete"),
        Some(json!({
            "query": {"type": "eq", "field": "genre", "value": {"type": "string", "value": "jazz"}}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "delete failed: {body}");
    assert_eq!(body, json!({"deleted": 2}));

    // Only the non-matching metarecord remains.
    let (_, left) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/query"),
        Some(json!({"query": {"type": "is_present", "field": "genre"}})),
    )
    .await;
    assert_eq!(left.as_array().unwrap().len(), 1);

    // The whole delete is a single revision (atomic, rollback-able as a unit).
    let (_, after) = request(&app, "GET", &format!("/repos/{repo}/log"), None).await;
    assert_eq!(revisions(&after) - revs_before, 1, "bulk delete must be one revision");
}

#[tokio::test]
async fn test_batch_set() {
    let (app, repo, root) = setup("set").await;
    for genre in ["jazz", "jazz", "rock"] {
        create(
            &app,
            &repo,
            json!([{"name": "genre", "value": {"type": "string", "value": genre}}]),
        )
        .await;
    }

    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/set"),
        Some(json!({
            "query": {"type": "eq", "field": "genre", "value": {"type": "string", "value": "jazz"}},
            "name": "rating",
            "value": {"type": "int", "value": 5}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "set failed: {body}");
    assert_eq!(body, json!({"updated": 2}));

    // The updated entries now match a rating query.
    let (_, rated) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/query"),
        Some(json!({"query": {"type": "eq", "field": "rating",
                              "value": {"type": "int", "value": 5}}})),
    )
    .await;
    assert_eq!(rated.as_array().unwrap().len(), 2);

    // Reserved field without force → 400, nothing written.
    let (status, _) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/set"),
        Some(json!({
            "query": {"type": "is_present", "field": "genre"},
            "name": "mfr_size",
            "value": {"type": "int", "value": 1}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    std::fs::remove_dir_all(root).unwrap();
}
