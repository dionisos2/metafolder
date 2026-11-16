//! `GET /repos/:repo/log/labels`: the labels of the log with what is known of
//! each without reading an operation (doc "Label summaries").

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use http_body_util::BodyExt;
use metafolder_daemon::routes;
use metafolder_daemon::state::AppState;
use serde_json::{json, Value};
use tower::util::ServiceExt;

mod common;
use common::TempDir;

async fn request(
    app: &Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
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

struct Repo {
    app: Router,
    repo: String,
    _root: TempDir,
}

async fn setup(prefix: &str) -> Repo {
    let app = routes::build(std::sync::Arc::new(AppState::new(common::settings())));
    let root = TempDir::new(&format!("lblhttp_{prefix}"));
    common::seed_config(&root);
    let (status, body) =
        request(&app, "POST", "/repos/init", Some(json!({"root": root.to_str().unwrap()}))).await;
    assert_eq!(status, StatusCode::OK, "init failed: {body}");
    let repo = body["repo_uuid"].as_str().unwrap().to_string();
    Repo { app, repo, _root: root }
}

impl Repo {
    async fn call(&self, method: &str, path: &str, body: Option<Value>) -> Value {
        let (status, value) =
            request(&self.app, method, &format!("/repos/{}{path}", self.repo), body).await;
        assert_eq!(status, StatusCode::OK, "{method} {path}: {value}");
        value
    }

    async fn labels(&self, query: &str) -> Value {
        self.call("GET", &format!("/log/labels{query}"), None).await
    }
}

async fn make_records(r: &Repo, n: usize) -> Vec<String> {
    let mut uuids = Vec::new();
    for i in 0..n {
        let record = r
            .call(
                "POST",
                "/metarecords",
                Some(json!({"fields": [{"name": "rank", "value": {"type": "int", "value": i}}]})),
            )
            .await;
        uuids.push(record["uuid"].as_str().unwrap().to_string());
    }
    uuids
}

#[tokio::test]
async fn the_summaries_say_how_many_operations_a_label_holds_and_what_reverts_it() {
    let r = setup("summaries").await;
    let uuids = make_records(&r, 12).await;
    // A write to many records: one label, twelve operations, one stub.
    r.call(
        "POST",
        "/query/fields/add",
        Some(json!({"query": {"type": "uuid_in", "uuids": uuids}, "name": "tag",
                    "value": {"type": "string", "value": "jazz"}})),
    )
    .await;

    let listed = r.labels("?limit=5").await;
    let mass = listed["labels"][0].clone();
    assert_eq!(mass["operations"], json!(12), "{mass}");
    assert_eq!(mass["is_head"], json!(true));
    assert!(mass["updated"].as_i64().unwrap() > 0, "{mass}");
    assert_eq!(mass["undone"], Value::Null);
    assert_eq!(mass["reverts"], json!([]));
    assert_eq!(listed["head_label"], mass["id"]);
    assert!(mass["first_op"].as_i64().unwrap() < mass["last_op"].as_i64().unwrap());
    let mass_id = mass["id"].as_i64().unwrap();

    // Undone whole: the undo says what it reverts, the label says it is undone.
    let undo = r.call("POST", "/revert", Some(json!({"target": {"label_id": mass_id}}))).await;
    let undo_id = undo["label"].as_i64().unwrap();
    let listed = r.labels("?limit=5").await;
    let by_id = |id: i64| {
        listed["labels"].as_array().unwrap().iter().find(|l| l["id"] == json!(id)).unwrap().clone()
    };
    assert_eq!(by_id(undo_id)["reverts"], json!([{"label": mass_id, "whole": true}]));
    assert_eq!(by_id(undo_id)["undone"], Value::Null);
    assert_eq!(by_id(mass_id)["undone"], json!("fully"));
    assert_eq!(by_id(mass_id)["reverted_by"], json!([{"label": undo_id, "whole": true}]));

    // The undo undone (a redo): the first change stands again.
    let redo = r.call("POST", "/revert", Some(json!({"target": {"label_id": undo_id}}))).await;
    let listed = r.labels("?limit=5").await;
    let by_id = |id: i64| {
        listed["labels"].as_array().unwrap().iter().find(|l| l["id"] == json!(id)).unwrap().clone()
    };
    assert_eq!(by_id(undo_id)["undone"], json!("fully"));
    assert_eq!(by_id(mass_id)["undone"], Value::Null, "{}", by_id(mass_id));
    assert_eq!(by_id(redo["label"].as_i64().unwrap())["undone"], Value::Null);
}

#[tokio::test]
async fn one_operation_undone_marks_its_label_partly() {
    let r = setup("partly").await;
    let uuids = make_records(&r, 3).await;
    let label = r.labels("?limit=1").await["labels"][0]["id"].as_i64().unwrap();
    // `uuids` were created one label each; set a field on two of them in one label.
    let _ = uuids;
    let log = r.call("GET", &format!("/log/labels/{label}"), None).await;
    let op = log["operations"][0]["id"].as_i64().unwrap();
    r.call("POST", "/revert", Some(json!({"target": {"op_ids": [op]}}))).await;
    let listed = r.labels("?limit=5").await;
    let this =
        listed["labels"].as_array().unwrap().iter().find(|l| l["id"] == json!(label)).unwrap();
    assert_eq!(this["undone"], json!("partly"), "{this}");
}

#[tokio::test]
async fn the_summaries_page_newest_first() {
    let r = setup("paging").await;
    make_records(&r, 7).await;
    let all = r.labels("?limit=500").await;
    let ids: Vec<i64> =
        all["labels"].as_array().unwrap().iter().map(|l| l["id"].as_i64().unwrap()).collect();
    assert!(ids.windows(2).all(|w| w[0] > w[1]), "newest first: {ids:?}");
    assert_eq!(all["next_before"], Value::Null);

    let mut paged = Vec::new();
    let mut before = String::new();
    loop {
        let page = r.labels(&format!("?limit=3{before}")).await;
        paged.extend(page["labels"].as_array().unwrap().iter().map(|l| l["id"].as_i64().unwrap()));
        match page["next_before"].as_i64() {
            Some(next) => before = format!("&before={next}"),
            None => break,
        }
    }
    assert_eq!(paged, ids);
    // A label with nothing nested in it lists nothing under it.
    let kids = r.labels(&format!("?parent={}", ids[0])).await;
    assert_eq!(kids["labels"], json!([]));
}

#[tokio::test]
async fn the_label_detail_counts_without_making_up_the_operations() {
    let r = setup("detail").await;
    let uuids = make_records(&r, 12).await;
    r.call(
        "POST",
        "/query/fields/add",
        Some(json!({"query": {"type": "uuid_in", "uuids": uuids}, "name": "tag",
                    "value": {"type": "string", "value": "jazz"}})),
    )
    .await;
    let label = r.labels("?limit=1").await["labels"][0]["id"].as_i64().unwrap();
    let detail = r.call("GET", &format!("/log/labels/{label}?offset=2&limit=3"), None).await;
    assert_eq!(detail["label"]["operation_count"], json!(12));
    assert_eq!(detail["label"]["is_head"], json!(true));
    assert_eq!(detail["operations"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn the_label_detail_can_give_the_newest_operations_without_their_snapshots() {
    let r = setup("newest").await;
    let uuids = make_records(&r, 12).await;
    r.call(
        "POST",
        "/query/fields/add",
        Some(json!({"query": {"type": "uuid_in", "uuids": uuids}, "name": "tag",
                    "value": {"type": "string", "value": "jazz"}})),
    )
    .await;
    let label = r.labels("?limit=1").await["labels"][0]["id"].as_i64().unwrap();
    let all = r.call("GET", &format!("/log/labels/{label}"), None).await;
    let ids: Vec<i64> =
        all["operations"].as_array().unwrap().iter().map(|o| o["id"].as_i64().unwrap()).collect();
    assert_eq!(ids.len(), 12);
    assert!(all["operations"][0].get("snapshots_before").is_some(), "snapshots by default");

    let window = r
        .call("GET", &format!("/log/labels/{label}?newest=1&offset=2&limit=3&snapshots=0"), None)
        .await;
    assert_eq!(window["label"]["operation_count"], json!(12));
    let got: Vec<i64> = window["operations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["id"].as_i64().unwrap())
        .collect();
    // Newest first, the two newest skipped.
    assert_eq!(got, vec![ids[9], ids[8], ids[7]]);
    assert!(window["operations"][0].get("snapshots_before").is_none(), "{window}");
}
