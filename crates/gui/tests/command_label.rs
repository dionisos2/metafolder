//! A GUI command's writes are in a log context of its own (doc "Log
//! contexts"): the daemon proxy creates it, named after the command, at the
//! command's first request that may write to a repository, and sends it with
//! the command's later requests there.

use std::sync::{Arc, Mutex};

use axum::extract::Json;
use axum::http::HeaderMap;
use axum::routing::post;
use axum::Router;
use metafolder_gui::daemon_proxy::DaemonProxy;
use serde_json::{json, Value};

const REPO: &str = "0123456789abcdef0123456789abcdef";

#[derive(Default)]
struct Seen {
    /// The contexts created: their bodies.
    created: Vec<Value>,
    /// The log context each request named, in order: (route, header).
    requests: Vec<(&'static str, Option<String>)>,
}

async fn spawn_stub() -> (String, Arc<Mutex<Seen>>) {
    let seen: Arc<Mutex<Seen>> = Arc::default();
    let header = |headers: &HeaderMap| {
        headers
            .get(metafolder_core::context::LOG_CONTEXT_HEADER)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
    };
    let (on_create, on_write, on_query) = (seen.clone(), seen.clone(), seen.clone());
    let router = Router::new()
        .route(
            &format!("/repos/{REPO}/contexts"),
            post(move |Json(body): Json<Value>| {
                let seen = on_create.clone();
                async move {
                    let mut seen = seen.lock().unwrap();
                    seen.created.push(body);
                    Json(json!({"id": 100 + seen.created.len()}))
                }
            }),
        )
        .route(
            &format!("/repos/{REPO}/metarecords"),
            post(move |headers: HeaderMap| {
                let seen = on_write.clone();
                async move {
                    seen.lock().unwrap().requests.push(("write", header(&headers)));
                    "{}"
                }
            }),
        )
        .route(
            &format!("/repos/{REPO}/query"),
            post(move |headers: HeaderMap| {
                let seen = on_query.clone();
                async move {
                    seen.lock().unwrap().requests.push(("query", header(&headers)));
                    "{}"
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    (format!("http://127.0.0.1:{port}"), seen)
}

#[tokio::test]
async fn each_run_creates_its_context_at_its_first_write() {
    let (url, seen) = spawn_stub().await;
    let proxy = DaemonProxy::new(url);
    let write = format!("/repos/{REPO}/metarecords");
    let query = format!("/repos/{REPO}/query");
    let send = |run: Option<&str>, path: &str| {
        let (proxy, path, run) = (&proxy, path.to_string(), run.map(str::to_string));
        async move { proxy.in_run(run, proxy.request("POST", &path, None)).await.unwrap() }
    };

    let edit = proxy.command_begin("metarecord:field set rating 4", None);
    send(Some(&edit), &query).await;
    send(Some(&edit), &write).await;
    // A command run from inside another names it, and is part of it.
    assert_eq!(proxy.command_begin("nested", Some(&edit)), edit);
    send(Some(&edit), &write).await;
    proxy.command_end(&edit);
    // Two commands running at once each have their own (doc "Log
    // contexts"): a reconcile begun while the edit runs.
    let reconcile = proxy.command_begin("repo:reconcile", None);
    assert_ne!(reconcile, edit);
    send(Some(&reconcile), &write).await;
    send(Some(&edit), &write).await;
    // A request that names no run is the newest command's.
    send(None, &write).await;
    proxy.command_end(&reconcile);
    send(None, &write).await;
    proxy.command_end(&edit);
    send(None, &write).await;

    let seen = seen.lock().unwrap();
    assert_eq!(
        seen.created,
        [
            json!({"name": "metarecord:field set rating 4", "description": null, "parent": null}),
            json!({"name": "repo:reconcile", "description": null, "parent": null}),
        ],
        "a read creates nothing; one context per run"
    );
    let (one, two) = (Some("101".to_string()), Some("102".to_string()));
    assert_eq!(
        seen.requests,
        [
            ("query", None),
            ("write", one.clone()),
            ("write", one.clone()),
            ("write", two.clone()),
            ("write", one.clone()),
            ("write", two),
            ("write", one),
            ("write", None),
        ],
    );
    drop(seen);

    // Ending a command that is not running changes nothing.
    let other = proxy.command_begin("other", None);
    proxy.command_end("not-it");
    assert_eq!(proxy.command_begin("again", Some(&other)), other);
    proxy.command_end(&other);
    proxy.command_end(&other);
}
