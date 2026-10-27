//! Saved workspaces end to end below the shell (doc "Saved workspaces"): the
//! repository's `.metafolder/` resolved through a stub daemon's `GET /repos`,
//! a workspace saved there, opened back here or in a new workspace, and the
//! repository's default loaded into the workspaces that take the repository.

use axum::routing::get;
use axum::Json;
use metafolder_gui::daemon_proxy::DaemonProxy;
use metafolder_gui::notifier::RecordingNotifier;
use metafolder_gui::saved_workspaces::{self as saved, Kind, Target};
use metafolder_gui::state::layout::SlotId;
use metafolder_gui::state::saved::Apply;
use metafolder_gui::state::GuiState;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

mod common;

const REPO: &str = "3a12bd48ad584bdabf3ab83a7f391bb9";

async fn spawn_stub_repos(internal_dir: PathBuf) -> String {
    let router = axum::Router::new().route(
        "/repos",
        get(move || {
            let internal_dir = internal_dir.clone();
            async move {
                Json(json!([{
                    "repo_uuid": REPO,
                    "name": "photos",
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

/// A GUI whose first workspace is on the repository, a list on the left with
/// a layout variable and a work variable; the stub daemon; the `.metafolder/`.
async fn setup() -> (common::TempDir, PathBuf, GuiState, DaemonProxy) {
    let root = common::TempDir::new("gui_saved_workspaces");
    let metafolder = root.join(".metafolder");
    std::fs::create_dir_all(&metafolder).unwrap();
    let daemon = DaemonProxy::new(spawn_stub_repos(metafolder.join("internal")).await);
    let gui = GuiState::new(Arc::new(RecordingNotifier::new()));
    gui.adopt_repo("ws-1", REPO).unwrap();
    gui.set_panel_type(SlotId::Left, "metarecord-list").unwrap();
    gui.set_var("ws-1", "metarecord-list:columns", json!("name")).unwrap();
    gui.set_var("ws-1", "metarecord-list:query", json!("jpg")).unwrap();
    (root, metafolder, gui, daemon)
}

fn keys(list: &[&str]) -> Vec<String> {
    list.iter().map(|k| k.to_string()).collect()
}

async fn save_work(gui: &GuiState, daemon: &DaemonProxy, name: &str) {
    saved::save(
        gui,
        daemon,
        "ws-1",
        Kind::Work,
        name,
        &keys(&["metarecord-list:columns"]),
        &keys(&["metarecord-list:query"]),
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn test_save_writes_the_file_and_list_names_it_with_the_default() {
    let (_root, metafolder, gui, daemon) = setup().await;
    save_work(&gui, &daemon, "tri").await;
    assert!(metafolder.join("gui/workspaces/tri.json").exists());
    assert_eq!(
        saved::list_of(&gui, &daemon, "ws-1").await.unwrap(),
        json!([{"name": "tri", "kind": "work", "default": false}])
    );
    saved::set_default_of(&gui, &daemon, "ws-1", "tri").await.unwrap();
    assert_eq!(saved::list_of(&gui, &daemon, "ws-1").await.unwrap()[0]["default"], json!(true));
}

#[tokio::test]
async fn test_open_here_loads_into_the_workspace_and_new_into_a_new_one() {
    let (_root, _metafolder, gui, daemon) = setup().await;
    save_work(&gui, &daemon, "tri").await;
    gui.set_var("ws-1", "metarecord-list:query", json!("changed")).unwrap();

    let here = saved::open(&gui, &daemon, "ws-1", Target::Here, "tri").await.unwrap();
    assert_eq!(here, "ws-1");
    assert_eq!(gui.get_var("ws-1", "metarecord-list:query").unwrap(), json!("jpg"));

    gui.set_var("ws-1", "metarecord-list:query", json!("changed")).unwrap();
    let new = saved::open(&gui, &daemon, "ws-1", Target::New, "tri").await.unwrap();
    assert_ne!(new, "ws-1");
    assert_eq!(gui.get_var(&new, "active_repo").unwrap(), json!(REPO));
    assert_eq!(gui.get_var(&new, "metarecord-list:query").unwrap(), json!("jpg"));
    assert_eq!(gui.get_var("ws-1", "metarecord-list:query").unwrap(), json!("changed"));
    assert_eq!(gui.layout().left.workspace_id.as_deref(), Some(new.as_str()));
}

#[tokio::test]
async fn test_open_an_unknown_name_is_an_error() {
    let (_root, _metafolder, gui, daemon) = setup().await;
    let error = saved::open(&gui, &daemon, "ws-1", Target::Here, "nothing").await.unwrap_err();
    assert!(error.contains("nothing"), "{error}");
}

#[tokio::test]
async fn test_the_default_is_loaded_whole_or_its_layout_variables_only() {
    let (_root, _metafolder, gui, daemon) = setup().await;
    save_work(&gui, &daemon, "tri").await;
    saved::set_default_of(&gui, &daemon, "ws-1", "tri").await.unwrap();

    let whole = gui.workspace_new(Some(REPO.into()));
    let applied = saved::apply_default(&gui, &daemon, &whole, Apply::Whole).await.unwrap();
    assert_eq!(applied.as_deref(), Some("tri"));
    assert_eq!(gui.get_var(&whole, "metarecord-list:query").unwrap(), json!("jpg"));

    let scripted = gui.create_workspace(Some(REPO.into()));
    saved::apply_default(&gui, &daemon, &scripted, Apply::LayoutVars).await.unwrap();
    assert_eq!(gui.get_var(&scripted, "metarecord-list:columns").unwrap(), json!("name"));
    assert_eq!(gui.get_var(&scripted, "metarecord-list:query").unwrap(), Value::Null);
}

#[tokio::test]
async fn test_no_default_and_no_repository_load_nothing() {
    let (_root, _metafolder, gui, daemon) = setup().await;
    let ws = gui.workspace_new(Some(REPO.into()));
    assert_eq!(saved::apply_default(&gui, &daemon, &ws, Apply::Whole).await.unwrap(), None);
    let bare = gui.create_workspace(None);
    assert_eq!(saved::apply_default(&gui, &daemon, &bare, Apply::Whole).await.unwrap(), None);
}

#[tokio::test]
async fn test_delete_and_unset_default() {
    let (_root, metafolder, gui, daemon) = setup().await;
    save_work(&gui, &daemon, "tri").await;
    saved::set_default_of(&gui, &daemon, "ws-1", "tri").await.unwrap();
    assert!(saved::unset_default_of(&gui, &daemon, "ws-1").await.unwrap());
    assert!(saved::delete_of(&gui, &daemon, "ws-1", "tri").await.unwrap());
    assert!(!metafolder.join("gui/workspaces/tri.json").exists());
    assert!(!saved::delete_of(&gui, &daemon, "ws-1", "tri").await.unwrap());
}
