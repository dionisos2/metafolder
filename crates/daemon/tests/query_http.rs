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
async fn test_query_path_target_sort_and_count() {
    // The GUI's core query (`mfr_path ->* "/dir"` + sort + count) end to end over
    // HTTP. Building the tree through the API also proves manual TreeRef writes
    // keep the (complete) tree cache in sync: if they did not, the path would
    // fail to resolve and the result would be wrong/empty.
    let (app, repo, root) = setup("pathsort").await;
    let node = |parent: Option<&str>, name: &str, rate: Option<i64>| {
        let mut fields = vec![json!({"name": "cat",
            "value": {"type": "tree_ref", "value": {"parent": parent, "name": name}}})];
        if let Some(r) = rate {
            fields.push(json!({"name": "rate", "value": {"type": "int", "value": r}}));
        }
        json!(fields)
    };
    let docs = create(&app, &repo, node(None, "docs", None)).await;
    let a = create(&app, &repo, node(Some(&docs), "a", Some(5))).await;
    let _b = create(&app, &repo, node(Some(&docs), "b", Some(2))).await;
    let _c = create(&app, &repo, node(Some(&docs), "c", None)).await; // no rate → sorts last
    let d = create(&app, &repo, node(Some(&docs), "d", Some(8))).await;

    let q = json!({"type": "follows_transitive", "field": "cat", "target": "docs"});
    let (status, page) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/query"),
        Some(json!({"query": q, "sort": [{"field": "rate", "order": "desc"}],
                    "limit": 2, "count": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["total"], json!(4), "exact descendant count (a,b,c,d)");
    let results: Vec<&str> =
        page["results"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert_eq!(results, vec![d.as_str(), a.as_str()], "rate desc: d(8), a(5)");

    // A path that resolves to nothing → empty result, count 0 (not an error).
    let qn = json!({"type": "follows_transitive", "field": "cat", "target": "nope"});
    let (status, page) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/query"),
        Some(json!({"query": qn, "limit": 2, "count": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["total"], json!(0));
    assert_eq!(page["results"].as_array().unwrap().len(), 0);

    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn test_query_rejects_meaningless_comparisons_with_400() {
    // The route rejects ill-defined comparisons upfront (before choosing an
    // engine), so a bad query never silently runs O(n) or returns confusing
    // rows — it is a clean 400.
    let (app, repo, root) = setup("badcmp").await;
    for bad in [
        // ordered comparison on a bool
        json!({"type": "lt", "field": "flag", "value": {"type": "bool", "value": true}}),
        // ordered comparison on a tree_ref
        json!({"type": "gt", "field": "cat",
               "value": {"type": "tree_ref", "value": {"parent": null, "name": "x"}}}),
        // comparison against nothing
        json!({"type": "eq", "field": "k", "value": {"type": "nothing"}}),
    ] {
        let (status, body) = request(
            &app,
            "POST",
            &format!("/repos/{repo}/query"),
            Some(json!({"query": bad, "limit": 10})),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "should reject {bad}: {body}");
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn test_query_correct_after_async_load() {
    // After a load (whose warmup runs in the background), a query must return
    // correct results — whether served by the freshly built index or the SQL
    // fallback, the load→warmup→query path must not break or mis-answer.
    let (app, repo, root) = setup("afterload").await;
    let node = |parent: Option<&str>, name: &str, rate: i64| {
        json!([
            {"name": "cat", "value": {"type": "tree_ref", "value": {"parent": parent, "name": name}}},
            {"name": "rate", "value": {"type": "int", "value": rate}}
        ])
    };
    let docs = create(&app, &repo, json!([{"name": "cat",
        "value": {"type": "tree_ref", "value": {"parent": null, "name": "docs"}}}]))
    .await;
    let hi = create(&app, &repo, node(Some(&docs), "hi", 9)).await;
    let lo = create(&app, &repo, node(Some(&docs), "lo", 1)).await;

    // Unload then reload: the reload warms in the background.
    let (st, _) = request(&app, "POST", &format!("/repos/{repo}/unload"), None).await;
    assert_eq!(st, StatusCode::OK);
    let (st, _) =
        request(&app, "POST", "/repos/load", Some(json!({"root": root.to_str().unwrap()}))).await;
    assert_eq!(st, StatusCode::OK);

    let q = json!({"type": "follows_transitive", "field": "cat", "target": "docs"});
    let (status, page) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/query"),
        Some(json!({"query": q, "sort": [{"field": "rate", "order": "desc"}],
                    "limit": 10, "count": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{page}");
    assert_eq!(page["total"], json!(2));
    let results: Vec<&str> =
        page["results"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert_eq!(results, vec![hi.as_str(), lo.as_str()], "rate desc: hi(9), lo(1)");

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

#[tokio::test]
async fn test_batch_set_multi_value() {
    let (app, repo, root) = setup("set_multi").await;
    for genre in ["jazz", "jazz", "rock"] {
        create(
            &app,
            &repo,
            json!([{"name": "genre", "value": {"type": "string", "value": genre}}]),
        )
        .await;
    }

    // `values` replaces all rows of the name with a multi-map set, in one op.
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/set"),
        Some(json!({
            "query": {"type": "eq", "field": "genre", "value": {"type": "string", "value": "jazz"}},
            "name": "tag",
            "values": [{"type": "string", "value": "a"}, {"type": "string", "value": "b"}]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "multi set failed: {body}");
    assert_eq!(body, json!({"updated": 2}));

    // Both rows present on each jazz metarecord.
    for tag in ["a", "b"] {
        let (_, hits) = request(
            &app,
            "POST",
            &format!("/repos/{repo}/query"),
            Some(json!({"query": {"type": "eq", "field": "tag",
                                  "value": {"type": "string", "value": tag}}})),
        )
        .await;
        assert_eq!(hits.as_array().unwrap().len(), 2, "tag={tag}");
    }

    // A second multi-set replaces the previous rows (set semantics).
    let (_, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/set"),
        Some(json!({
            "query": {"type": "eq", "field": "genre", "value": {"type": "string", "value": "jazz"}},
            "name": "tag",
            "values": [{"type": "string", "value": "c"}]
        })),
    )
    .await;
    assert_eq!(body, json!({"updated": 2}));
    let (_, gone) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/query"),
        Some(json!({"query": {"type": "eq", "field": "tag",
                              "value": {"type": "string", "value": "a"}}})),
    )
    .await;
    assert_eq!(gone.as_array().unwrap().len(), 0, "old rows replaced");

    // Providing both value and values is a 400.
    let (status, _) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/set"),
        Some(json!({
            "query": {"type": "is_present", "field": "genre"},
            "name": "tag",
            "value": {"type": "string", "value": "x"},
            "values": [{"type": "string", "value": "y"}]
        })),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn test_batch_append() {
    let (app, repo, root) = setup("append").await;
    for genre in ["jazz", "jazz", "rock"] {
        create(
            &app,
            &repo,
            json!([{"name": "genre", "value": {"type": "string", "value": genre}}]),
        )
        .await;
    }

    // Append a tag row to every jazz metarecord (multi-map: never replaces).
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/append"),
        Some(json!({
            "query": {"type": "eq", "field": "genre", "value": {"type": "string", "value": "jazz"}},
            "name": "tag",
            "value": {"type": "string", "value": "a"}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "append failed: {body}");
    assert_eq!(body, json!({"updated": 2}));

    // A second append adds a second row rather than replacing the first.
    let (_, body2) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/append"),
        Some(json!({
            "query": {"type": "eq", "field": "genre", "value": {"type": "string", "value": "jazz"}},
            "name": "tag",
            "value": {"type": "string", "value": "b"}
        })),
    )
    .await;
    assert_eq!(body2, json!({"updated": 2}));

    // Both tag rows coexist on the two jazz metarecords (multi-map preserved).
    for tag in ["a", "b"] {
        let (_, hits) = request(
            &app,
            "POST",
            &format!("/repos/{repo}/query"),
            Some(json!({"query": {"type": "eq", "field": "tag",
                                  "value": {"type": "string", "value": tag}}})),
        )
        .await;
        assert_eq!(hits.as_array().unwrap().len(), 2, "tag={tag}");
    }

    // Reserved field without force → 400.
    let (status, _) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/append"),
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

#[tokio::test]
async fn test_remove_by_query() {
    let (app, repo, root) = setup("remove").await;
    // Two metarecords, each carrying tag=test and tag=keep (multi-map).
    for _ in 0..2 {
        create(
            &app,
            &repo,
            json!([
                {"name": "tag", "value": {"type": "string", "value": "test"}},
                {"name": "tag", "value": {"type": "string", "value": "keep"}}
            ]),
        )
        .await;
    }

    // Remove only the tag=test rows across every metarecord (inverse of add).
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/remove"),
        Some(json!({
            "query": {"type": "is_present", "field": "tag"},
            "name": "tag",
            "value": {"type": "string", "value": "test"}
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "remove failed: {body}");
    assert_eq!(body, json!({"updated": 2}));

    // tag=test is gone; tag=keep is untouched.
    let (_, gone) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/query"),
        Some(json!({"query": {"type": "eq", "field": "tag",
                              "value": {"type": "string", "value": "test"}}})),
    )
    .await;
    assert_eq!(gone.as_array().unwrap().len(), 0);
    let (_, kept) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/query"),
        Some(json!({"query": {"type": "eq", "field": "tag",
                              "value": {"type": "string", "value": "keep"}}})),
    )
    .await;
    assert_eq!(kept.as_array().unwrap().len(), 2);

    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn test_set_record_reflected_in_index_query() {
    let (app, repo, root) = setup("setrecord_idx").await;
    let uuid = create(
        &app,
        &repo,
        json!([{"name": "a", "value": {"type": "int", "value": 1}}]),
    )
    .await;

    // Whole-record set: drop `a`, add `b`.
    let (status, _) = request(
        &app,
        "PUT",
        &format!("/repos/{repo}/metarecords/{uuid}"),
        Some(json!({"fields": [{"name": "b", "value": {"type": "int", "value": 2}}]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // The incremental index refresh must reflect the replacement: `a` is gone,
    // `b` is present (this query routes through the in-memory index).
    let (_, gone) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/query"),
        Some(json!({"query": {"type": "is_present", "field": "a"}})),
    )
    .await;
    assert_eq!(gone.as_array().unwrap().len(), 0, "old field cleared from the index");
    let (_, present) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/query"),
        Some(json!({"query": {"type": "eq", "field": "b", "value": {"type": "int", "value": 2}}})),
    )
    .await;
    assert_eq!(present.as_array().unwrap().len(), 1, "new field indexed");

    std::fs::remove_dir_all(root).unwrap();
}

#[tokio::test]
async fn test_batch_unset_removes_the_whole_field() {
    let (app, repo, root) = setup("unset").await;
    // Two metarecords, each with two tag rows.
    for _ in 0..2 {
        create(
            &app,
            &repo,
            json!([
                {"name": "tag", "value": {"type": "string", "value": "a"}},
                {"name": "tag", "value": {"type": "string", "value": "b"}}
            ]),
        )
        .await;
    }

    let revisions = |body: &serde_json::Value| body["revisions"].as_array().unwrap().len();
    let (_, before) = request(&app, "GET", &format!("/repos/{repo}/log"), None).await;
    let revs_before = revisions(&before);

    // Unset removes the whole field (both rows) from every match, one revision.
    let (status, body) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/unset"),
        Some(json!({"query": {"type": "is_present", "field": "tag"}, "name": "tag"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "unset failed: {body}");
    assert_eq!(body, json!({"updated": 2}));

    let (_, left) = request(
        &app,
        "POST",
        &format!("/repos/{repo}/query"),
        Some(json!({"query": {"type": "is_present", "field": "tag"}})),
    )
    .await;
    assert_eq!(left.as_array().unwrap().len(), 0, "field is now unknown everywhere");

    // One revision per metarecord change, but bundled into one revision overall.
    let (_, after) = request(&app, "GET", &format!("/repos/{repo}/log"), None).await;
    assert_eq!(revisions(&after) - revs_before, 1, "bulk unset must be one revision");

    std::fs::remove_dir_all(root).unwrap();
}

// ── In-memory index wiring (spec-indexing increment 5) ──────────────────────

/// After a write advances the log HEAD, the index is rebuilt before the next
/// query, so results reflect the write (it is never served stale).
#[tokio::test]
async fn test_index_reflects_writes_after_rebuild() {
    let (app, repo, root) = setup("freshness").await;
    create(&app, &repo, json!([{"name": "rate", "value": {"type": "int", "value": 5}}])).await;

    let q = |min: i64| {
        json!({
            "query": {"type": "gte", "field": "rate", "value": {"type": "int", "value": min}},
            "limit": 100,
            "count": true
        })
    };

    // First query builds the index from the current state.
    let (_, p) = request(&app, "POST", &format!("/repos/{repo}/query"), Some(q(5))).await;
    assert_eq!(p["total"], json!(1));

    // A second create advances HEAD; the next query must rebuild and see it.
    create(&app, &repo, json!([{"name": "rate", "value": {"type": "int", "value": 10}}])).await;
    let (_, p) = request(&app, "POST", &format!("/repos/{repo}/query"), Some(q(8))).await;
    assert_eq!(p["total"], json!(1), "the new rate=10 record");
    let (_, p) = request(&app, "POST", &format!("/repos/{repo}/query"), Some(q(5))).await;
    assert_eq!(p["total"], json!(2), "both records");

    std::fs::remove_dir_all(root).unwrap();
}

/// A query the index does not accelerate (`matches`) falls back to the SQL
/// engine transparently and returns the correct result.
#[tokio::test]
async fn test_unsupported_query_falls_back_to_sql() {
    let (app, repo, root) = setup("fallback").await;
    create(&app, &repo, json!([{"name": "name", "value": {"type": "string", "value": "hello"}}]))
        .await;
    create(&app, &repo, json!([{"name": "name", "value": {"type": "string", "value": "world"}}]))
        .await;

    let body = json!({
        "query": {"type": "matches", "field": "name", "pattern": "^h"},
        "limit": 100
    });
    let (status, page) =
        request(&app, "POST", &format!("/repos/{repo}/query"), Some(body)).await;
    assert_eq!(status, StatusCode::OK, "matches query failed: {page}");
    assert_eq!(page["results"].as_array().unwrap().len(), 1);

    std::fs::remove_dir_all(root).unwrap();
}
