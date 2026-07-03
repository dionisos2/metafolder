//! Input-history repo resolution (spec-gui "Input history"): the GUI turns a
//! repository uuid into its `.metafolder/` location through the daemon's
//! `GET /repos` (parent of `internal_dir`), then reads/writes the history
//! files itself — no daemon endpoint is involved. A stub daemon on an
//! ephemeral port serves the repo listing.

use axum::routing::get;
use axum::Json;
use metafolder_gui::daemon_proxy::DaemonProxy;
use metafolder_gui::history;
use serde_json::json;
use std::path::PathBuf;

async fn spawn_stub_repos(internal_dir: PathBuf) -> String {
    let router = axum::Router::new().route(
        "/repos",
        get(move || {
            let internal_dir = internal_dir.clone();
            async move {
                Json(json!([{
                    "repo_uuid": "3a12bd48ad584bdabf3ab83a7f391bb9",
                    "name": "smoke",
                    "root": "/somewhere",
                    "internal_dir": internal_dir.to_str().unwrap(),
                    "created_at": 0,
                }]))
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://127.0.0.1:{port}")
}

fn temp_metafolder() -> PathBuf {
    let dir = std::env::temp_dir()
        .join(format!("metafolder_gui_history_resolve_{}", uuid::Uuid::new_v4()))
        .join(".metafolder");
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[tokio::test]
async fn test_resolves_metafolder_dir_and_roundtrips_history() {
    let metafolder = temp_metafolder();
    let url = spawn_stub_repos(metafolder.join("internal")).await;
    let proxy = DaemonProxy::new(url);

    // Dashed and 32-hex forms both resolve to internal_dir's parent.
    for repo in ["3a12bd48-ad58-4bda-bf3a-b83a7f391bb9", "3a12bd48ad584bdabf3ab83a7f391bb9"] {
        let dir = history::metafolder_dir_of(&proxy, repo).await.unwrap();
        assert_eq!(dir, metafolder);
    }

    // End-to-end: resolve, append, read, and the file is where the spec says.
    let dir = history::metafolder_dir_of(&proxy, "3a12bd48ad584bdabf3ab83a7f391bb9").await.unwrap();
    assert!(history::append(&dir, "shell:command", "repo:list").unwrap());
    assert_eq!(history::read(&dir, "shell:command", None).unwrap(), vec!["repo:list"]);
    let file = metafolder.join("gui/history/shell:command");
    assert_eq!(std::fs::read_to_string(file).unwrap(), "repo:list\n");
}

#[tokio::test]
async fn test_unknown_repo_is_an_error() {
    let metafolder = temp_metafolder();
    let url = spawn_stub_repos(metafolder.join("internal")).await;
    let proxy = DaemonProxy::new(url);
    let err = history::metafolder_dir_of(&proxy, "00000000000000000000000000000000")
        .await
        .unwrap_err();
    assert!(err.contains("not loaded"), "{err}");
}
