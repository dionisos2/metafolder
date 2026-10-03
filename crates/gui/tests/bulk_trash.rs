//! Trashing a set of metarecords at once — the checked selection or the list's
//! query (doc "Sending files to the trash"). One revision deletes every
//! metarecord of the set, then each file's bytes move into the trash as an
//! entry of its own; a file under a directory of the set travels with the
//! directory, and the repository root is never trashed. Tests run a stub daemon
//! on an ephemeral port.

mod common;

use axum::extract::{Path, State};
use axum::routing::{get, post};
use axum::Json;
use common::TempDir;
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

const ROOT: &str = "00000000000000000000000000000000";
const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const D: &str = "dddddddddddddddddddddddddddddddd";
const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const N: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";

#[derive(Clone)]
struct Stub {
    root: String,
    internal: String,
    /// Bodies of the received POST /repos/:repo/metarecords/trash calls.
    deletes: Arc<Mutex<Vec<Value>>>,
}

/// A metarecord body whose `mfr_path` is `name` under `parent`.
fn record(uuid: &str, parent: Option<&str>, name: &str) -> Value {
    json!({
        "uuid": uuid,
        "version": 7,
        "fields": [{
            "name": "mfr_path",
            "value": {"type": "tree_ref", "value": {"parent": parent, "name": name}}
        }],
    })
}

fn record_of(uuid: &str) -> Value {
    match uuid {
        ROOT => record(ROOT, None, ""),
        A => record(A, Some(ROOT), "a.txt"),
        D => record(D, Some(ROOT), "dir"),
        B => record(B, Some(D), "b.txt"),
        other => json!({"uuid": other, "version": 1, "fields": []}),
    }
}

async fn spawn_stub(root: String, internal: String) -> (String, Stub) {
    let stub = Stub { root, internal, deletes: Arc::new(Mutex::new(Vec::new())) };
    let router = axum::Router::new()
        .route(
            "/repos/:repo",
            get(|State(stub): State<Stub>| async move {
                Json(json!({"root": stub.root, "internal_dir": stub.internal}))
            }),
        )
        .route(
            "/repos/:repo/query/fields/resolve-tree",
            // The set the query names: two files, a directory and the file
            // under it, a metarecord with no file, and the repository root.
            post(|| async move {
                Json(json!({
                    A: ["/a.txt"],
                    D: ["/dir"],
                    B: ["/dir/b.txt"],
                    N: [],
                    ROOT: [""],
                }))
            }),
        )
        .route(
            "/repos/:repo/metarecords/:uuid",
            get(
                |Path((_repo, uuid)): Path<(String, String)>| async move { Json(record_of(&uuid)) },
            ),
        )
        .route(
            "/repos/:repo/query",
            post(|Json(body): Json<Value>| async move {
                // What lives under a path: only `dir` has a descendant.
                let hits = match body["query"]["target"].as_str() {
                    Some("/dir") => json!([record_of(B)]),
                    _ => json!([]),
                };
                Json(json!({"results": hits, "next_cursor": null}))
            }),
        )
        .route(
            "/repos/:repo/metarecords/trash",
            post(|State(stub): State<Stub>, Json(body): Json<Value>| async move {
                stub.deletes.lock().unwrap().push(body);
                Json(json!({"deleted": 3}))
            }),
        )
        .with_state(stub.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    (format!("http://{addr}"), stub)
}

#[tokio::test]
async fn trashing_a_set_deletes_its_metarecords_in_one_call_and_moves_each_file() {
    let dir = TempDir::new("gui_bulk_trash");
    let root = dir.path().to_path_buf();
    let internal = root.join(".metafolder/internal");
    std::fs::create_dir_all(&internal).unwrap();
    std::fs::create_dir_all(root.join("dir")).unwrap();
    std::fs::write(root.join("a.txt"), b"a").unwrap();
    std::fs::write(root.join("dir/b.txt"), b"b").unwrap();
    let (base, stub) =
        spawn_stub(root.to_string_lossy().into_owned(), internal.to_string_lossy().into_owned())
            .await;

    let query = json!({"type": "uuid_in", "uuids": [A, D, B, N, ROOT]});
    let outcome = tokio::task::spawn_blocking(move || {
        metafolder_gui::trash::trash_query(&base, "repo", &query)
    })
    .await
    .unwrap()
    .expect("the bulk trashing succeeds");

    assert_eq!(outcome.trashed, 2, "a.txt and dir are entries of their own: {outcome:?}");
    assert_eq!(outcome.inside, 1, "dir/b.txt travels with dir: {outcome:?}");
    assert_eq!(outcome.without_file, 1, "the metarecord with no file is left alone");
    assert!(outcome.root_kept, "the repository root is never trashed");
    assert!(outcome.failed.is_empty(), "{:?}", outcome.failed);

    let deletes = stub.deletes.lock().unwrap();
    assert_eq!(deletes.len(), 1, "one call, so one revision for the whole set");
    let uuids: BTreeSet<&str> =
        deletes[0]["uuids"].as_array().unwrap().iter().filter_map(Value::as_str).collect();
    assert_eq!(uuids, BTreeSet::from([A, D, B]));
    assert_eq!(deletes[0]["force"], json!(false));

    assert!(!root.join("a.txt").exists());
    assert!(!root.join("dir").exists());
    let entries = metafolder_core::trash::TrashDir::new(internal.join("trash")).entries().unwrap();
    assert_eq!(entries.len(), 2);
}
