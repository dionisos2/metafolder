//! Paths on the wire as their exact bytes (doc "Tree names"): a path that is
//! text is a plain string, one that is not is `{text, bytes}`, and a request
//! can name a node exactly — so that a program never has to read a displayed
//! `%E9` back, which a file really named `caf%E9` shows too.

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
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn hex(bytes: &[u8]) -> String {
    metafolder_core::hex::encode(bytes)
}

/// A `cat` forest: `all`, and under it two nodes whose names *show* the same —
/// `caf%E9`, one holding the byte 0xE9, the other the three characters.
struct Forest {
    app: Router,
    repo: String,
    all: String,
    byte: String,
    literal: String,
    _root: TempDir,
}

async fn forest(prefix: &str) -> Forest {
    let app = routes::build(std::sync::Arc::new(AppState::new(common::settings())));
    let root = TempDir::new(&format!("treepaths_{prefix}"));
    common::seed_config(&root);
    let (_, body) =
        request(&app, "POST", "/repos/init", Some(json!({"root": root.to_str().unwrap()}))).await;
    let repo = body["repo_uuid"].as_str().unwrap().to_string();
    let create = |name: Value, parent: Option<String>| {
        let (app, repo) = (app.clone(), repo.clone());
        async move {
            let fields = json!([{"name": "cat",
                "value": {"type": "tree_ref", "value": {"parent": parent, "name": name}}}]);
            let (status, body) = request(
                &app,
                "POST",
                &format!("/repos/{repo}/metarecords"),
                Some(json!({"fields": fields})),
            )
            .await;
            assert_eq!(status, StatusCode::OK, "{body}");
            body["uuid"].as_str().unwrap().to_string()
        }
    };
    let all = create(json!("all"), None).await;
    let byte = create(json!({"text": "caf%E9", "bytes": hex(b"caf\xe9")}), Some(all.clone())).await;
    let literal = create(json!("caf%E9"), Some(all.clone())).await;
    Forest { app, repo, all, byte, literal, _root: root }
}

#[tokio::test]
async fn test_resolve_tree_gives_the_bytes_of_a_path_that_is_not_text() {
    let f = forest("out").await;
    let paths = |uuid: &str| {
        let (app, uri) = (
            f.app.clone(),
            format!("/repos/{}/metarecords/{uuid}/fields/cat/resolve-tree", f.repo),
        );
        async move { request(&app, "GET", &uri, None).await.1["paths"].clone() }
    };
    assert_eq!(paths(&f.byte).await, json!([{"text": "all/caf%E9", "bytes": hex(b"all/caf\xe9")}]));
    assert_eq!(paths(&f.literal).await, json!(["all/caf%E9"]), "text is a plain string");

    let (_, bulk) = request(
        &f.app,
        "POST",
        &format!("/repos/{}/query/fields/resolve-tree", f.repo),
        Some(json!({"field": "cat", "query": {"type": "uuid_in", "uuids": [f.byte]}})),
    )
    .await;
    assert_eq!(bulk[&f.byte], json!([{"text": "all/caf%E9", "bytes": hex(b"all/caf\xe9")}]));
}

#[tokio::test]
async fn test_resolve_path_takes_a_path_exactly() {
    let f = forest("in").await;
    let resolve = |body: Value| {
        let (app, uri) = (f.app.clone(), format!("/repos/{}/tree/resolve-path", f.repo));
        async move { request(&app, "POST", &uri, Some(body)).await.1["uuid"].clone() }
    };
    // Typed text: both readings, two different nodes — none.
    assert_eq!(resolve(json!({"field": "cat", "path": "all/caf%E9"})).await, Value::Null);
    // The bytes, as a path object: that node.
    let exact = json!({"text": "all/caf%E9", "bytes": hex(b"all/caf\xe9")});
    assert_eq!(resolve(json!({"field": "cat", "path": exact})).await, json!(f.byte));
    // A string read verbatim is its own bytes: the other one.
    assert_eq!(
        resolve(json!({"field": "cat", "path": "all/caf%E9", "form": "verbatim"})).await,
        json!(f.literal)
    );
}

/// Many positions at once, exactly, answered in the order asked — what a sync
/// plan needs to find the occupants of a whole scope's positions.
#[tokio::test]
async fn test_resolve_paths_answers_each_path_in_order() {
    let f = forest("bulk").await;
    let (status, body) = request(
        &f.app,
        "POST",
        &format!("/repos/{}/tree/resolve-paths", f.repo),
        Some(json!({"field": "cat", "form": "verbatim", "paths": [
            {"text": "all/caf%E9", "bytes": hex(b"all/caf\xe9")},
            "all/caf%E9",
            "all/nope",
            "all",
        ]})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["uuids"], json!([f.byte, f.literal, null, f.all]));
}

#[tokio::test]
async fn test_a_forest_listing_names_a_node_by_its_bytes() {
    let f = forest("children").await;
    let (_, children) = request(
        &f.app,
        "GET",
        &format!("/repos/{}/tree/children?field=cat&uuid={}", f.repo, f.all),
        None,
    )
    .await;
    let names: Vec<Value> =
        children.as_array().unwrap().iter().map(|c| c["name"].clone()).collect();
    assert!(names.contains(&json!({"text": "caf%E9", "bytes": hex(b"caf\xe9")})), "{children}");
    assert!(names.contains(&json!("caf%E9")), "{children}");
}
