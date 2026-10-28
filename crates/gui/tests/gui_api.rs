//! The GUI scripting HTTP API on port 7524 (doc "GUI scripting endpoints"), driven with oneshot
//! like the daemon tests.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use axum::Json;
use http_body_util::BodyExt;
use metafolder_gui::config::ConfigDir;
use metafolder_gui::daemon_proxy::DaemonProxy;
use metafolder_gui::events;
use metafolder_gui::notifier::RecordingNotifier;
use metafolder_gui::server::bench::BenchBuffer;
use metafolder_gui::server::command_wait::{CommandOutcome, CommandWait};
use metafolder_gui::server::input_wait::InputWait;
use metafolder_gui::server::{self, ServerState};
use metafolder_gui::state::GuiState;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

mod common;

struct Ctx {
    _guard: tempfile::TempDir,
    notifier: Arc<RecordingNotifier>,
    gui: Arc<GuiState>,
    input: Arc<InputWait>,
    commands: Arc<CommandWait>,
    bench: Arc<BenchBuffer>,
    router: axum::Router,
}

/// Stub daemon exposing GET /repos and a query endpoint with no matches.
async fn spawn_stub_daemon() -> String {
    let router = axum::Router::new()
        .route("/health", get(|| async { Json(json!({"status": "ok"})) }))
        .route(
            "/repos",
            get(|| async {
                Json(json!([{"repo_uuid": "feedfacefeedfacefeedfacefeedface",
                             "root": "/tmp/stub", "name": "stub"}]))
            }),
        )
        .fallback(axum::routing::any(|| async {
            Json(json!({"results": [], "next_cursor": null}))
        }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://127.0.0.1:{port}")
}

async fn setup_with_daemon(daemon_url: &str) -> Ctx {
    let guard = tempfile::tempdir().unwrap();
    let config = Arc::new(ConfigDir::at(guard.path().join("gui")));
    common::install_defaults(&config);

    let notifier = Arc::new(RecordingNotifier::new());
    let gui = Arc::new(GuiState::new(notifier.clone()));
    let daemon = Arc::new(DaemonProxy::new(daemon_url.to_string()));
    let keybindings = Arc::new(std::sync::Mutex::new(config.load_keybindings().unwrap()));
    let input = Arc::new(InputWait::new());
    let commands = Arc::new(CommandWait::new());
    let bench = Arc::new(BenchBuffer::new());

    let state = ServerState {
        config,
        gui: gui.clone(),
        daemon,
        keybindings,
        input: input.clone(),
        commands: commands.clone(),
        bench: bench.clone(),
        repo_list_cache_ttl: std::time::Duration::from_secs(3),
        video_thumbnail_percent: 10.0,
    };
    Ctx {
        _guard: guard,
        notifier,
        gui,
        input: input.clone(),
        commands: commands.clone(),
        bench: bench.clone(),
        router: server::build_router(state),
    }
}

async fn setup() -> Ctx {
    let url = spawn_stub_daemon().await;
    setup_with_daemon(&url).await
}

async fn request(
    router: &axum::Router,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let builder = Request::builder().method(method).uri(uri);
    let request = match body {
        Some(value) => builder
            .header("content-type", "application/json")
            .body(Body::from(value.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let response = router.clone().oneshot(request).await.unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(Value::Null)
    };
    (status, value)
}

use tower::util::ServiceExt;

// ── Workspaces ────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_workspaces_list_create_delete() {
    let ctx = setup().await;

    let (status, body) = request(&ctx.router, "GET", "/gui/workspaces", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!([{"id": "ws-1", "name": "Workspace 1", "active_repo": null, "repo_name": null,
                "work_context": null}])
    );

    // Explicit repo.
    let (status, body) = request(
        &ctx.router,
        "POST",
        "/gui/workspaces",
        Some(json!({"active_repo": "cafe0000000000000000000000000000"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"id": "ws-2"}));

    // No repo given: the daemon's first loaded repository.
    let (status, body) = request(&ctx.router, "POST", "/gui/workspaces", Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK);
    let id = body["id"].as_str().unwrap().to_string();
    let workspaces = ctx.gui.workspaces();
    let created = workspaces.iter().find(|w| w.id == id).unwrap();
    assert_eq!(created.active_repo.as_deref(), Some("feedfacefeedfacefeedfacefeedface"));

    let (status, _) = request(&ctx.router, "DELETE", "/gui/workspaces/ws-2", None).await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    assert!(ctx.gui.workspaces().iter().all(|w| w.id != "ws-2"));

    let (status, _) = request(&ctx.router, "DELETE", "/gui/workspaces/ws-99", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_workspace_create_without_daemon_gets_null_repo() {
    let ctx = setup_with_daemon("http://127.0.0.1:1").await;
    let (status, body) = request(&ctx.router, "POST", "/gui/workspaces", Some(json!({}))).await;
    assert_eq!(status, StatusCode::OK);
    let id = body["id"].as_str().unwrap().to_string();
    let workspaces = ctx.gui.workspaces();
    assert_eq!(workspaces.iter().find(|w| w.id == id).unwrap().active_repo, None);
}

/// A script that opens a scratch workspace names the panels it wants there.
/// Without that the workspace is first shown with its default panel — the
/// metarecord list — which loads the whole repository and publishes its first
/// row as the selection, some time later: over the file the script had just
/// shown for its first question.
#[tokio::test]
async fn test_workspace_create_with_panels_never_shows_the_default_panel() {
    let ctx = setup().await;
    let (status, body) = request(
        &ctx.router,
        "POST",
        "/gui/workspaces",
        Some(json!({
            "active_repo": "cafe0000000000000000000000000000",
            "panels": {"left": "viewer", "right": "metarecord-detail"},
        })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let id = body["id"].as_str().unwrap().to_string();

    for slot in ["left", "right"] {
        let (status, _) =
            request(&ctx.router, "PUT", "/gui/layout", Some(json!({slot: id.as_str()}))).await;
        assert_eq!(status, StatusCode::OK);
    }
    let layout = ctx.gui.layout();
    assert_eq!(layout.left.workspace_id.as_deref(), Some(id.as_str()));
    assert_eq!(layout.left.panel_type.as_deref(), Some("viewer"));
    assert_eq!(layout.right.panel_type.as_deref(), Some("metarecord-detail"));

    // An unknown slot name is a mistake to report, not a key to ignore.
    let (status, _) = request(
        &ctx.router,
        "POST",
        "/gui/workspaces",
        Some(json!({"panels": {"middle": "viewer"}})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

// ── Layout ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_layout_get_and_partial_put() {
    let ctx = setup().await;

    let (status, body) = request(&ctx.router, "GET", "/gui/layout", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"left": "ws-1", "right": null}));

    // Partial update: only the right slot.
    let (status, _) =
        request(&ctx.router, "PUT", "/gui/layout", Some(json!({"right": "ws-1"}))).await;
    assert_eq!(status, StatusCode::OK);
    let (_, body) = request(&ctx.router, "GET", "/gui/layout", None).await;
    assert_eq!(body, json!({"left": "ws-1", "right": "ws-1"}));

    // null hides a slot.
    let (status, _) = request(&ctx.router, "PUT", "/gui/layout", Some(json!({"left": null}))).await;
    assert_eq!(status, StatusCode::OK);
    let (_, body) = request(&ctx.router, "GET", "/gui/layout", None).await;
    assert_eq!(body["left"], Value::Null);

    // Unknown workspace.
    let (status, _) =
        request(&ctx.router, "PUT", "/gui/layout", Some(json!({"left": "ws-99"}))).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ── Panel views ───────────────────────────────────────────────────────────

#[tokio::test]
async fn test_panel_view_set_and_get() {
    let ctx = setup().await;

    let (status, _) =
        request(&ctx.router, "PUT", "/gui/panels/left/view", Some(json!({"type": "message"})))
            .await;
    assert_eq!(status, StatusCode::OK);

    let (status, body) = request(&ctx.router, "GET", "/gui/panels/left/view", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["type"], "message");
    assert_eq!(body["status"], "loading");

    ctx.gui.set_panel_ready("ws-1", "message").unwrap();
    let (_, body) = request(&ctx.router, "GET", "/gui/panels/left/view", None).await;
    assert_eq!(body["status"], "ready");

    // A hidden slot is shown first (it inherits the focused workspace).
    let (status, _) = request(
        &ctx.router,
        "PUT",
        "/gui/panels/right/view",
        Some(json!({"type": "workspace-info"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let layout = ctx.gui.layout();
    assert!(layout.right.visible);
    assert_eq!(layout.right.panel_type.as_deref(), Some("workspace-info"));

    let (status, _) = request(&ctx.router, "GET", "/gui/panels/middle/view", None).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_panel_view_file_with_path_sets_selection() {
    let ctx = setup().await;
    let (status, _) = request(
        &ctx.router,
        "PUT",
        "/gui/panels/left/view",
        Some(json!({"type": "viewer", "path": "/tmp/picture.jpg"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ctx.gui.get_var("ws-1", "selected_paths").unwrap(), json!(["/tmp/picture.jpg"]));
    // No active repo in ws-1: no entry lookup is possible.
    assert_eq!(ctx.gui.get_var("ws-1", "selected_metarecord").unwrap(), Value::Null);
}

/// A stub daemon that records every query body it receives and answers each
/// with a single uuid, so a path→metarecord lookup can be asserted end to end:
/// its outcome (`selected_metarecord` set) AND the exact query shape it sends.
async fn spawn_recording_daemon(seen: Arc<std::sync::Mutex<Vec<Value>>>) -> String {
    let router = axum::Router::new()
        .route("/health", get(|| async { Json(json!({"status": "ok"})) }))
        .route(
            "/repos",
            get(|| async {
                Json(json!([{"repo_uuid": "feedfacefeedfacefeedfacefeedface",
                             "root": "/tmp/stub", "name": "stub"}]))
            }),
        )
        .route(
            "/repos/{repo}/query",
            axum::routing::post(move |Json(body): Json<Value>| {
                let seen = seen.clone();
                async move {
                    seen.lock().unwrap().push(body);
                    Json(json!(["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"]))
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

/// `view viewer --path` on a **top-level** folder (a child directly under the
/// repo root) must resolve its metarecord. The daemon's tree convention names
/// the repo root the EMPTY string and every descendant with a single leading
/// slash, so the exact-node lookup must query `mfr_path = "/projets"` — not a
/// `follows mfr_path -> "/"`, whose "/" resolves to no node (the root is "").
#[tokio::test]
async fn test_panel_view_file_top_level_folder_resolves_metarecord() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let url = spawn_recording_daemon(seen.clone()).await;
    let ctx = setup_with_daemon(&url).await;

    // A workspace bound to the stub repo, shown in the left slot.
    let ws = ctx.gui.create_workspace(Some("feedfacefeedfacefeedfacefeedface".into()));
    ctx.gui.tab_assign(&ws, metafolder_gui::state::layout::SlotId::Left).unwrap();

    let (status, _) = request(
        &ctx.router,
        "PUT",
        "/gui/panels/left/view",
        Some(json!({"type": "viewer", "path": "/tmp/stub/projets"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // The lookup succeeded (a real daemon would have matched the node).
    assert_eq!(
        ctx.gui.get_var(&ws, "selected_metarecord").unwrap(),
        json!({"uuid": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
               "repo": "feedfacefeedfacefeedfacefeedface"})
    );
    // And it asked in the daemon's exact-node convention (leading slash, no
    // "/"-rooted follows).
    let queries = seen.lock().unwrap();
    assert_eq!(
        queries[0]["query"],
        json!({"type": "eq", "field": "mfr_path",
               "value": {"type": "string", "value": "/projets"}})
    );
}

/// The selection is a PAIR, and it is published metarecord-first. A consumer
/// reacting to `selected_paths` reads `selected_metarecord` right then (the
/// `file` panel does exactly that, to know which metarecord holds the playback
/// position of what it is about to show), so publishing the paths first hands
/// it the PREVIOUS metarecord — it shows one file and believes it is another.
/// `gui-tag-folder` walks a whole folder through this endpoint, one
/// `view viewer --path` per question, so every question inherited the answer
/// before it.
#[tokio::test]
async fn test_panel_view_file_publishes_the_metarecord_before_the_paths() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let url = spawn_recording_daemon(seen.clone()).await;
    let ctx = setup_with_daemon(&url).await;

    let ws = ctx.gui.create_workspace(Some("feedfacefeedfacefeedfacefeedface".into()));
    ctx.gui.tab_assign(&ws, metafolder_gui::state::layout::SlotId::Left).unwrap();
    ctx.notifier.clear();

    let (status, _) = request(
        &ctx.router,
        "PUT",
        "/gui/panels/left/view",
        Some(json!({"type": "viewer", "path": "/tmp/stub/projets/note.txt"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let keys: Vec<String> = ctx
        .notifier
        .payloads(events::WORKSPACE_VAR_CHANGED)
        .iter()
        .filter_map(|payload| payload["key"].as_str().map(str::to_string))
        .collect();
    assert_eq!(keys, vec!["selected_metarecord".to_string(), "selected_paths".to_string()]);
}

/// An EMPTY `path` clears the selection instead of leaving the previous one on
/// screen. A shipped script shows the entry it is asking about with one
/// `mf gui view left viewer --path <abs>` per question, and an entry may have no
/// file to show (a record with no `mfr_path`, a path that no longer resolves).
/// Omitting `path` means "leave it alone", which in that spot is the previous
/// *question's* file still on screen, presented as the answer to this one —
/// so "no file" has to be sayable.
#[tokio::test]
async fn test_panel_view_file_with_an_empty_path_clears_the_selection() {
    let ctx = setup().await;
    ctx.gui.set_var("ws-1", "selected_paths", json!(["/tmp/previous.jpg"])).unwrap();
    ctx.gui.set_var("ws-1", "selected_metarecord", json!({"uuid": "aa", "repo": "r"})).unwrap();

    let (status, _) = request(
        &ctx.router,
        "PUT",
        "/gui/panels/left/view",
        Some(json!({"type": "viewer", "path": ""})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ctx.gui.get_var("ws-1", "selected_paths").unwrap(), json!([]));
    assert_eq!(ctx.gui.get_var("ws-1", "selected_metarecord").unwrap(), Value::Null);
}

// ── Messages ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_message_targets_focused_or_explicit_workspace() {
    let ctx = setup().await;
    ctx.gui.create_workspace(None); // ws-2, unassigned
    ctx.notifier.clear();

    let (status, _) = request(
        &ctx.router,
        "POST",
        "/gui/message",
        Some(json!({"text": "hello", "timeout_ms": 1000})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let statuses = ctx.notifier.payloads(events::STATUS_MESSAGE);
    assert_eq!(statuses[0]["workspace_id"], "ws-1"); // focused slot's ws

    let (status, _) = request(
        &ctx.router,
        "POST",
        "/gui/message?workspace_id=ws-2",
        Some(json!({"text": "direct", "timeout_ms": null})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ctx.gui.messages("ws-2").unwrap().len(), 1);

    let (status, _) =
        request(&ctx.router, "POST", "/gui/message?workspace_id=ws-99", Some(json!({"text": "x"})))
            .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn test_messages_returns_the_log_and_the_last_status() {
    let ctx = setup().await;

    let (status, body) = request(&ctx.router, "GET", "/gui/workspaces/ws-1/messages", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"messages": [], "status": null}));

    ctx.gui.post_status("ws-1", "Deleted 2 metarecords.", "info", Some(5000)).unwrap();
    ctx.gui.append_message("ws-1", "detail").unwrap();
    let (status, body) = request(&ctx.router, "GET", "/gui/workspaces/ws-1/messages", None).await;
    assert_eq!(status, StatusCode::OK);
    let texts: Vec<&str> =
        body["messages"].as_array().unwrap().iter().map(|m| m["text"].as_str().unwrap()).collect();
    assert_eq!(texts, ["Deleted 2 metarecords.", "detail"]);
    assert_eq!(body["status"]["text"], "Deleted 2 metarecords.");
    assert_eq!(body["status"]["kind"], "info");
    assert!(body["status"]["ts_ms"].as_u64().is_some());

    let (status, _) = request(&ctx.router, "GET", "/gui/workspaces/ws-99/messages", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ── Script progress ───────────────────────────────────────────────────────

#[tokio::test]
async fn test_progress_updates_running_script() {
    let ctx = setup().await;
    // A running script, as run_shell registers it (with the injected run id).
    ctx.gui.script_begin("script-1", "ws-1", "gui-tag-pair.sh");

    let (status, _) = request(
        &ctx.router,
        "POST",
        "/gui/progress",
        Some(json!({"task": "script-1", "done": 3, "total": 10, "phase": "/music/x.mp3"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let payloads = ctx.notifier.payloads("script-task-changed");
    let task = &payloads.last().unwrap()["tasks"][0];
    assert_eq!(task["done"], 3);
    assert_eq!(task["total"], 10);
    assert_eq!(task["phase"], "/music/x.mp3");

    // An unknown run id is a lenient no-op: still 200, and no new broadcast.
    let before = ctx.notifier.payloads("script-task-changed").len();
    let (status, _) =
        request(&ctx.router, "POST", "/gui/progress", Some(json!({"task": "nope", "done": 1})))
            .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(ctx.notifier.payloads("script-task-changed").len(), before);
}

// ── Input wait ────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_input_wait_refuses_the_reserved_keys() {
    // A script may not take the user's ways out of its own question (doc
    // "Script sessions"): escape, the command input, F6 (which walks to the
    // question and back). Tab is no longer one: it is left to the panels.
    let ctx = setup().await;
    for key in ["escape", "ctrl+escape", ":", "x", "f6"] {
        let (status, body) =
            request(&ctx.router, "POST", "/gui/input", Some(json!({"keys": ["y", key]}))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "'{key}' should be refused");
        assert!(
            body["error"].as_str().unwrap().contains(key),
            "the error should name the offending key: {body}"
        );
    }
    // Refused before the lock is taken: a legitimate wait still starts.
    let router = ctx.router.clone();
    let waiting = tokio::spawn(async move {
        request(&router, "POST", "/gui/input", Some(json!({"keys": ["y"]}))).await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(ctx.input.resolve_answer(None, "y"));
    let (status, body) = waiting.await.unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"event": "answer", "value": "y"}));
}

#[tokio::test]
async fn test_input_wait_payload_carries_the_task_and_the_script_keys_flag() {
    let ctx = setup().await;
    ctx.gui.script_begin("script-9", "ws-1", "gui-tag-folder.sh");
    ctx.notifier.clear();

    let router = ctx.router.clone();
    let waiting = tokio::spawn(async move {
        request(
            &router,
            "POST",
            "/gui/input",
            Some(json!({"keys": ["y"], "task": "script-9", "prompt": "tag it?"})),
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let payload = ctx.notifier.payloads(events::INPUT_WAIT_CHANGED).last().unwrap().clone();
    assert_eq!(payload["questions"][0]["task"], "script-9");
    assert_eq!(payload["script_keys"], true);
    assert_eq!(payload["questions"][0]["prompt"], "tag it?");

    assert!(ctx.input.resolve_answer(None, "y"));
    waiting.await.unwrap();
}

#[tokio::test]
async fn test_a_question_installs_no_keybinding() {
    // The shell gives an awaited key to the question *on screen* before any
    // binding (doc "Script sessions"); a binding in the table would also fire
    // for a question put away in another workspace, and answer it unseen.
    let ctx = setup().await;
    ctx.notifier.clear();
    let router = ctx.router.clone();
    let waiting = tokio::spawn(async move {
        request(&router, "POST", "/gui/input", Some(json!({"keys": ["y"]}))).await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    for push in ctx.notifier.payloads(events::KEYBINDINGS_CHANGED) {
        assert!(!push["bindings"].to_string().contains("answer:send"));
    }

    // The checkbox still flips the flag the question bar shows.
    ctx.gui.set_script_keys(false);
    metafolder_gui::server::gui_api::push_questions(&ctx.gui);
    let payload = ctx.notifier.payloads(events::INPUT_WAIT_CHANGED).last().unwrap().clone();
    assert_eq!(payload["script_keys"], false);
    assert_eq!(payload["questions"].as_array().unwrap().len(), 1, "the question stays on screen");

    assert!(ctx.input.resolve_answer(None, "y"));
    waiting.await.unwrap();
}

#[tokio::test]
async fn test_input_wait_resolves_on_answer() {
    let ctx = setup().await;
    ctx.notifier.clear();

    let router = ctx.router.clone();
    let waiting = tokio::spawn(async move {
        request(&router, "POST", "/gui/input", Some(json!({"keys": ["left", "right"]}))).await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert!(ctx.input.resolve_answer(None, "left"));
    let (status, body) = waiting.await.unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"event": "answer", "value": "left"}));

    // No more waiters: answers go nowhere.
    assert!(!ctx.input.resolve_answer(None, "left"));
}

#[tokio::test]
async fn test_input_wait_broadcasts_its_prompt() {
    let ctx = setup().await;
    ctx.notifier.clear();

    let router = ctx.router.clone();
    let waiting = tokio::spawn(async move {
        request(
            &router,
            "POST",
            "/gui/input",
            Some(json!({"keys": ["a", "r"], "prompt": "Which action?"})),
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    // The active input wait carries its question so the frontend can show it
    // in a dedicated bar, separate from the transient status/error line.
    let waits = ctx.notifier.payloads(events::INPUT_WAIT_CHANGED);
    let active = waits.last().expect("an input-wait event");
    assert_eq!(active["questions"][0]["prompt"], json!("Which action?"));
    assert_eq!(active["questions"][0]["keys"], json!(["a", "r"]));

    assert!(ctx.input.resolve_answer(None, "a"));
    let _ = waiting.await.unwrap();

    // The question is cleared when the wait ends.
    let waits = ctx.notifier.payloads(events::INPUT_WAIT_CHANGED);
    assert_eq!(waits.last().unwrap()["questions"], json!([]));
}

#[tokio::test]
async fn test_input_wait_belongs_to_the_scripts_latest_workspace_and_marks_it_waiting() {
    // A script's question belongs to the workspace it is asked in — the last one
    // the script opened, not the one it was launched from: the shell shows the
    // question bar only while it is on screen (doc "Script sessions"). The task
    // entry also stops spinning while the script is blocked, so "still working"
    // and "your turn" are distinguishable.
    let ctx = setup().await;
    ctx.gui.script_begin("script-7", "ws-1", "gui-tag-folder.sh");
    let (status, body) =
        request(&ctx.router, "POST", "/gui/workspaces", Some(json!({"task": "script-7"}))).await;
    assert_eq!(status, StatusCode::OK);
    let scratch = body["id"].as_str().unwrap().to_string();
    ctx.notifier.clear();

    let router = ctx.router.clone();
    let waiting = tokio::spawn(async move {
        request(
            &router,
            "POST",
            "/gui/input",
            Some(json!({"keys": ["y", "n"], "prompt": "tagged?", "task": "script-7"})),
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let waits = ctx.notifier.payloads(events::INPUT_WAIT_CHANGED);
    assert_eq!(waits.last().unwrap()["questions"][0]["workspace"], json!(scratch));

    let tasks = ctx.notifier.payloads(events::SCRIPT_TASK_CHANGED);
    assert_eq!(tasks.last().unwrap()["tasks"][0]["waiting"], json!(true));

    assert!(ctx.input.resolve_answer(None, "y"));
    let _ = waiting.await.unwrap();

    let waits = ctx.notifier.payloads(events::INPUT_WAIT_CHANGED);
    assert_eq!(waits.last().unwrap()["questions"], json!([]));
    let tasks = ctx.notifier.payloads(events::SCRIPT_TASK_CHANGED);
    assert_eq!(tasks.last().unwrap()["tasks"][0]["waiting"], json!(false));
}

#[tokio::test]
async fn test_a_question_asked_before_any_workspace_opens_belongs_to_the_launching_one() {
    let ctx = setup().await;
    ctx.gui.script_begin("script-7", "ws-1", "gui-tag-folder.sh");
    let router = ctx.router.clone();
    let waiting = tokio::spawn(async move {
        request(&router, "POST", "/gui/input", Some(json!({"keys": ["s"], "task": "script-7"})))
            .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let waits = ctx.notifier.payloads(events::INPUT_WAIT_CHANGED);
    assert_eq!(waits.last().unwrap()["questions"][0]["workspace"], json!("ws-1"));
    assert!(ctx.input.resolve_answer(None, "s"));
    waiting.await.unwrap();
}

#[tokio::test]
async fn test_two_workspaces_each_hold_a_question() {
    // One question per workspace (doc "GUI design decisions"): two scripts
    // asking from two workspaces both wait, and each answer reaches its own.
    let ctx = setup().await;
    let (_, body) = request(&ctx.router, "POST", "/gui/workspaces", Some(json!({}))).await;
    let other = body["id"].as_str().unwrap().to_string();
    ctx.gui.script_begin("script-a", "ws-1", "a.sh");
    ctx.gui.script_begin("script-b", &other, "b.sh");
    ctx.notifier.clear();

    let router = ctx.router.clone();
    let a = tokio::spawn(async move {
        request(&router, "POST", "/gui/input", Some(json!({"keys": ["y"], "task": "script-a"})))
            .await
    });
    let router = ctx.router.clone();
    let b = tokio::spawn(async move {
        request(&router, "POST", "/gui/prompt", Some(json!({"prompt": "?", "task": "script-b"})))
            .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let questions = ctx.notifier.payloads(events::INPUT_WAIT_CHANGED).last().unwrap().clone();
    let question = &questions["questions"][0];
    assert_eq!(question["workspace"], json!("ws-1"));
    let prompt = ctx.notifier.payloads(events::PROMPT_REQUESTED).last().unwrap().clone();
    assert_eq!(prompt["workspace"], json!(other));

    // Each is resolved by its id; the wrong kind of wait is not touched.
    let prompt_id = prompt["id"].as_u64().unwrap();
    let question_id = question["id"].as_u64().unwrap();
    assert!(!ctx.input.resolve_answer(Some(prompt_id), "y"));
    assert!(ctx.input.resolve_prompt(Some(prompt_id), true, Some("jazz".into())));
    assert!(ctx.input.resolve_answer(Some(question_id), "y"));
    assert_eq!(a.await.unwrap().1, json!({"event": "answer", "value": "y"}));
    assert_eq!(b.await.unwrap().1, json!({"event": "confirm", "text": "jazz"}));
}

#[tokio::test]
async fn test_a_workspace_created_without_a_task_is_owned_by_nobody() {
    let ctx = setup().await;
    ctx.gui.script_begin("script-7", "ws-1", "gui-tag-folder.sh");
    let (_, _) = request(&ctx.router, "POST", "/gui/workspaces", Some(json!({}))).await;
    assert_eq!(ctx.gui.script_workspaces("script-7"), vec!["ws-1".to_string()]);
}

#[tokio::test]
async fn test_prompt_belongs_to_the_scripts_latest_workspace() {
    // A script's text prompt belongs to the workspace it is asked in, exactly
    // like its question bar: the command input gives the line back to the user
    // while it is not on screen (doc "Script sessions"), and shows the prompt
    // again when it returns.
    let ctx = setup().await;
    ctx.gui.script_begin("script-9", "ws-1", "gui-tag-folder.sh");
    let (status, body) =
        request(&ctx.router, "POST", "/gui/workspaces", Some(json!({"task": "script-9"}))).await;
    assert_eq!(status, StatusCode::OK);
    let scratch = body["id"].as_str().unwrap().to_string();
    ctx.notifier.clear();

    let router = ctx.router.clone();
    let waiting = tokio::spawn(async move {
        request(
            &router,
            "POST",
            "/gui/prompt",
            Some(json!({"prompt": "Tag: ", "task": "script-9"})),
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let prompts = ctx.notifier.payloads(events::PROMPT_REQUESTED);
    let raised = prompts.last().expect("a prompt-requested event");
    assert_eq!(raised["workspace"], json!(scratch));
    assert_eq!(raised["task"], json!("script-9"));

    assert!(ctx.input.resolve_prompt(None, true, Some("jazz".into())));
    let _ = waiting.await.unwrap();
}

#[tokio::test]
async fn test_a_prompt_with_no_run_id_is_owned_by_nobody() {
    // A prompt the GUI never launched (a script run from a terminal) belongs to
    // no workspace and is therefore always shown.
    let ctx = setup().await;
    ctx.notifier.clear();
    let router = ctx.router.clone();
    let waiting = tokio::spawn(async move {
        request(&router, "POST", "/gui/prompt", Some(json!({"prompt": "Tag: "}))).await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let prompts = ctx.notifier.payloads(events::PROMPT_REQUESTED);
    let raised = prompts.last().expect("a prompt-requested event");
    assert_eq!(raised["workspace"], json!(null));
    assert_eq!(raised["task"], json!(null));

    assert!(ctx.input.resolve_prompt(None, false, None));
    let _ = waiting.await.unwrap();
}

#[tokio::test]
async fn test_prompt_marks_the_script_waiting_too() {
    let ctx = setup().await;
    ctx.gui.script_begin("script-8", "ws-1", "gui-tag-folder.sh");
    ctx.notifier.clear();

    let router = ctx.router.clone();
    let waiting = tokio::spawn(async move {
        request(
            &router,
            "POST",
            "/gui/prompt",
            Some(json!({"prompt": "Tag: ", "task": "script-8"})),
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let tasks = ctx.notifier.payloads(events::SCRIPT_TASK_CHANGED);
    assert_eq!(tasks.last().unwrap()["tasks"][0]["waiting"], json!(true));

    assert!(ctx.input.resolve_prompt(None, true, Some("jazz".into())));
    let _ = waiting.await.unwrap();
    let tasks = ctx.notifier.payloads(events::SCRIPT_TASK_CHANGED);
    assert_eq!(tasks.last().unwrap()["tasks"][0]["waiting"], json!(false));
}

#[tokio::test]
async fn test_concurrent_input_waits_of_one_workspace_conflict() {
    let ctx = setup().await;
    let router = ctx.router.clone();
    let waiting =
        tokio::spawn(async move { request(&router, "POST", "/gui/input", Some(json!({}))).await });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let (status, _) = request(&ctx.router, "POST", "/gui/input", Some(json!({}))).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) =
        request(&ctx.router, "POST", "/gui/prompt", Some(json!({"prompt": "?"}))).await;
    assert_eq!(status, StatusCode::CONFLICT);

    ctx.input.close_all();
    let (_, body) = waiting.await.unwrap();
    assert_eq!(body, json!({"event": "closed"}));
}

#[tokio::test]
async fn test_a_dropped_input_request_releases_the_lock() {
    // A script killed mid-question (or a `mf` that gives up) drops the HTTP
    // request; the handler's future is then cancelled. If it left the lock held,
    // EVERY later wait would answer 409 for the rest of the GUI's life — the
    // next script would appear to "just stop" at its first question, with
    // nothing on screen to say why (doc "Script sessions").
    let ctx = setup().await;
    let router = ctx.router.clone();
    let waiting = tokio::spawn(async move {
        request(&router, "POST", "/gui/input", Some(json!({"keys": ["y"]}))).await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(ctx.input.is_active());

    waiting.abort();
    let _ = waiting.await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!ctx.input.is_active(), "the lock is released when the request is dropped");

    // The question went with it, and a new wait can start.
    let waits = ctx.notifier.payloads(events::INPUT_WAIT_CHANGED);
    assert_eq!(waits.last().unwrap()["questions"], json!([]));
    let (status, body) =
        request(&ctx.router, "POST", "/gui/input", Some(json!({"timeout_ms": 30}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["event"], json!("timeout"));
}

#[tokio::test]
async fn test_a_dropped_prompt_request_releases_the_lock() {
    let ctx = setup().await;
    let router = ctx.router.clone();
    let waiting = tokio::spawn(async move {
        request(&router, "POST", "/gui/prompt", Some(json!({"prompt": "Tag: "}))).await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(ctx.input.is_active());

    waiting.abort();
    let _ = waiting.await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!ctx.input.is_active(), "the lock is released when the request is dropped");
}

#[tokio::test]
async fn test_input_wait_timeout() {
    let ctx = setup().await;
    let (status, body) =
        request(&ctx.router, "POST", "/gui/input", Some(json!({"timeout_ms": 50}))).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"event": "timeout"}));
    // The lock is released after a timeout.
    assert!(!ctx.input.is_active());
}

// ── Prompt ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_prompt_confirm_and_cancel() {
    let ctx = setup().await;
    ctx.notifier.clear();

    let router = ctx.router.clone();
    let waiting = tokio::spawn(async move {
        request(&router, "POST", "/gui/prompt", Some(json!({"prompt": "Enter a rating (1-5): "})))
            .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let prompts = ctx.notifier.payloads(events::PROMPT_REQUESTED);
    assert_eq!(prompts[0]["prompt"], "Enter a rating (1-5): ");

    assert!(ctx.input.resolve_prompt(None, true, Some("4".into())));
    let (status, body) = waiting.await.unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"event": "confirm", "text": "4"}));

    // Cancel path.
    let router = ctx.router.clone();
    let waiting = tokio::spawn(async move {
        request(&router, "POST", "/gui/prompt", Some(json!({"prompt": "? "}))).await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(ctx.input.resolve_prompt(None, false, None));
    let (_, body) = waiting.await.unwrap();
    assert_eq!(body, json!({"event": "cancel"}));
}

#[tokio::test]
async fn test_prompt_forwards_completions() {
    let ctx = setup().await;
    ctx.notifier.clear();

    let router = ctx.router.clone();
    let waiting = tokio::spawn(async move {
        request(
            &router,
            "POST",
            "/gui/prompt",
            Some(json!({"prompt": "Tag: ", "completions": ["jazz", "rock"]})),
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let prompts = ctx.notifier.payloads(events::PROMPT_REQUESTED);
    assert_eq!(prompts[0]["prompt"], "Tag: ");
    assert_eq!(prompts[0]["completions"], json!(["jazz", "rock"]));

    assert!(ctx.input.resolve_prompt(None, true, Some("jazz".into())));
    let (status, _) = waiting.await.unwrap();
    assert_eq!(status, StatusCode::OK);

    // Without the field, completions default to an empty list.
    ctx.notifier.clear();
    let router = ctx.router.clone();
    let waiting = tokio::spawn(async move {
        request(&router, "POST", "/gui/prompt", Some(json!({"prompt": "? "}))).await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    let prompts = ctx.notifier.payloads(events::PROMPT_REQUESTED);
    assert_eq!(prompts[0]["completions"], json!([]));
    assert!(ctx.input.resolve_prompt(None, false, None));
    waiting.await.unwrap();
}

// ── Status ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_status_snapshot() {
    let ctx = setup().await;
    let (status, body) = request(&ctx.router, "GET", "/gui/status", None).await;
    assert_eq!(status, StatusCode::OK);

    assert_eq!(body["workspaces"][0]["id"], "ws-1");
    assert_eq!(body["layout"]["left"]["workspace_id"], "ws-1");
    assert_eq!(body["layout"]["left"]["focused"], true);
    assert_eq!(body["layout"]["right"], Value::Null);
    assert_eq!(body["input_wait_active"], false);
    assert!(body["daemon_connected"].is_boolean());
}

// ── Media support ─────────────────────────────────────────────────────────

#[tokio::test]
async fn test_media_support_endpoint() {
    let ctx = setup().await;
    let (status, body) = request(&ctx.router, "GET", "/__media-support", None).await;
    assert_eq!(status, StatusCode::OK);

    // The values depend on the host's GStreamer installation: only check
    // the shape and the internal consistency of the answer.
    let missing: Vec<String> = body["missing"]
        .as_array()
        .expect("'missing' must be an array")
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect();
    let has = |element: &str| !missing.iter().any(|m| m == element);
    assert_eq!(body["audio"], json!(has("autoaudiosink")));
    assert_eq!(body["video"], json!(has("autoaudiosink") && has("autovideosink")));
}

// ── Command dispatch (POST /gui/command) ──────────────────────────────────

/// Reads the single `command-requested` payload and returns its
/// (invocation_id, invocation).
fn last_command_request(ctx: &Ctx) -> (uuid::Uuid, String) {
    let payloads = ctx.notifier.payloads(events::COMMAND_REQUESTED);
    let last = payloads.last().expect("a command-requested event");
    let id = last["invocation_id"].as_str().expect("invocation_id");
    let invocation = last["invocation"].as_str().expect("invocation");
    (uuid::Uuid::parse_str(id).unwrap(), invocation.to_string())
}

#[tokio::test]
async fn test_command_dispatch_resolves_ok() {
    let ctx = setup().await;
    ctx.notifier.clear();

    let router = ctx.router.clone();
    let waiting = tokio::spawn(async move {
        request(&router, "POST", "/gui/command", Some(json!({"invocation": "panel:split"}))).await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    // The invocation string was forwarded verbatim to the frontend.
    let (id, invocation) = last_command_request(&ctx);
    assert_eq!(invocation, "panel:split");

    assert!(ctx.commands.resolve(id, CommandOutcome::Ok));
    let (status, body) = waiting.await.unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"event": "ok"}));

    // The wait is gone after resolution.
    assert!(!ctx.commands.resolve(id, CommandOutcome::Ok));
}

/// `confirm` answers the confirmations the command asks while it runs (doc
/// "GUI scripting endpoints"): forwarded to the shell, which reports back the
/// questions it answered.
#[tokio::test]
async fn test_command_dispatch_with_confirm_reports_the_questions_answered() {
    let ctx = setup().await;
    ctx.notifier.clear();

    let router = ctx.router.clone();
    let waiting = tokio::spawn(async move {
        let body = json!({"invocation": "metarecord:trash", "confirm": true});
        request(&router, "POST", "/gui/command", Some(body)).await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let payloads = ctx.notifier.payloads(events::COMMAND_REQUESTED);
    assert_eq!(payloads.last().unwrap()["confirm"], json!(true));
    let (id, _) = last_command_request(&ctx);
    let questions = vec!["Send the selected metarecord's file to the trash?".to_string()];
    assert!(ctx.commands.resolve_with(id, CommandOutcome::Ok, questions));
    let (status, body) = waiting.await.unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({"event": "ok",
               "confirmations": ["Send the selected metarecord's file to the trash?"]})
    );
}

#[tokio::test]
async fn test_command_dispatch_without_confirm_leaves_the_dialogs_to_the_user() {
    let ctx = setup().await;
    ctx.notifier.clear();

    let router = ctx.router.clone();
    let waiting = tokio::spawn(async move {
        request(&router, "POST", "/gui/command", Some(json!({"invocation": "panel:split"}))).await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let payloads = ctx.notifier.payloads(events::COMMAND_REQUESTED);
    assert_eq!(payloads.last().unwrap()["confirm"], Value::Null);
    let (id, _) = last_command_request(&ctx);
    ctx.commands.resolve(id, CommandOutcome::Ok);
    assert_eq!(waiting.await.unwrap().1, json!({"event": "ok"}), "no confirmations member");
}

#[tokio::test]
async fn test_command_dispatch_reports_error() {
    let ctx = setup().await;
    ctx.notifier.clear();

    let router = ctx.router.clone();
    let waiting = tokio::spawn(async move {
        request(&router, "POST", "/gui/command", Some(json!({"invocation": "bogus:cmd"}))).await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let (id, _) = last_command_request(&ctx);
    assert!(ctx.commands.resolve(id, CommandOutcome::Error("unknown command: bogus:cmd".into())));
    let (status, body) = waiting.await.unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"event": "error", "message": "unknown command: bogus:cmd"}));
}

#[tokio::test]
async fn test_command_dispatch_missing_invocation_is_400() {
    let ctx = setup().await;
    let (status, _) = request(&ctx.router, "POST", "/gui/command", Some(json!({}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn test_command_dispatch_timeout() {
    let ctx = setup().await;
    let (status, body) = request(
        &ctx.router,
        "POST",
        "/gui/command",
        Some(json!({"invocation": "panel:split", "timeout_ms": 50})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"event": "timeout"}));
}

#[tokio::test]
async fn test_concurrent_command_dispatch_allowed() {
    let ctx = setup().await;
    ctx.notifier.clear();

    let r1 = ctx.router.clone();
    let w1 = tokio::spawn(async move {
        request(&r1, "POST", "/gui/command", Some(json!({"invocation": "a:one"}))).await
    });
    let r2 = ctx.router.clone();
    let w2 = tokio::spawn(async move {
        request(&r2, "POST", "/gui/command", Some(json!({"invocation": "b:two"}))).await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Two independent waits, distinct ids — no 409.
    let payloads = ctx.notifier.payloads(events::COMMAND_REQUESTED);
    assert_eq!(payloads.len(), 2);
    let ids: Vec<uuid::Uuid> = payloads
        .iter()
        .map(|p| uuid::Uuid::parse_str(p["invocation_id"].as_str().unwrap()).unwrap())
        .collect();
    assert_ne!(ids[0], ids[1]);

    for id in ids {
        assert!(ctx.commands.resolve(id, CommandOutcome::Ok));
    }
    for w in [w1, w2] {
        let (status, body) = w.await.unwrap();
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, json!({"event": "ok"}));
    }
}

// ── Bench buffer (GET /gui/bench, POST /gui/bench/clear) ───────────────────

#[tokio::test]
async fn test_bench_lists_recorded_measures() {
    let ctx = setup().await;
    ctx.bench.record("mf:list:fetch", 12.5);
    ctx.bench.record("mf:detail:load", 4.0);

    let (status, body) = request(&ctx.router, "GET", "/gui/bench", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        body,
        json!({"records": [
            {"name": "mf:list:fetch", "duration_ms": 12.5},
            {"name": "mf:detail:load", "duration_ms": 4.0},
        ]})
    );
}

#[tokio::test]
async fn test_bench_clear_empties_the_buffer() {
    let ctx = setup().await;
    ctx.bench.record("mf:list:render", 1.0);

    let (status, _) = request(&ctx.router, "POST", "/gui/bench/clear", None).await;
    assert_eq!(status, StatusCode::OK);

    let (_, body) = request(&ctx.router, "GET", "/gui/bench", None).await;
    assert_eq!(body, json!({"records": []}));
}

// ── Reading a workspace variable (doc "GUI scripting endpoints") ───────────
// A script is a subprocess: it sees nothing of the GUI's state and can only ask
// through `mf gui …`. This is how it asks — and it is what `mf gui query` reads
// to start on the query the panel is showing.

#[tokio::test]
async fn test_a_variable_is_read_back_by_name() {
    let ctx = setup().await;
    let ws = ctx.gui.create_workspace(None);
    ctx.gui.set_var(&ws, "metarecord-list:effective-query-text", json!("rating > 3")).unwrap();

    let (status, body) = request(
        &ctx.router,
        "GET",
        &format!("/gui/workspaces/{ws}/vars/metarecord-list:effective-query-text"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"], json!("rating > 3"));
}

#[tokio::test]
async fn test_an_unset_variable_reads_as_null_not_as_an_error() {
    // "Nothing published yet" is an ordinary answer, not a failure: a script
    // asking before the list has run must fall back, not abort.
    let ctx = setup().await;
    let ws = ctx.gui.create_workspace(None);

    let (status, body) =
        request(&ctx.router, "GET", &format!("/gui/workspaces/{ws}/vars/never-set"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"], Value::Null);
}

#[tokio::test]
async fn test_reading_a_variable_of_an_unknown_workspace_is_a_404() {
    let ctx = setup().await;
    let (status, _) =
        request(&ctx.router, "GET", "/gui/workspaces/ws-does-not-exist/vars/anything", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ── Reading the global GUI variables (doc "Global GUI variables") ─────────

#[tokio::test]
async fn test_a_global_variable_is_read_back_by_name() {
    let ctx = setup().await;
    let clip = json!({ "paths": ["/tmp/a"], "mode": "copy" });
    ctx.gui.set_global("clipboard", clip.clone());

    let (status, body) = request(&ctx.router, "GET", "/gui/vars/clipboard", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"], clip);
    let (status, body) = request(&ctx.router, "GET", "/gui/vars/never-set", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"], Value::Null);
}

#[tokio::test]
async fn test_every_global_variable_is_listed() {
    let ctx = setup().await;
    ctx.gui.set_global("clipboard", json!(null));
    ctx.gui.set_global("other", json!(2));

    let (status, body) = request(&ctx.router, "GET", "/gui/vars", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["vars"], json!({ "clipboard": null, "other": 2 }));
}

#[tokio::test]
async fn test_a_variable_holding_a_list_survives_the_round_trip() {
    // `selected_metarecords` is a list of uuids — the other half of what
    // `mf gui query` reads, and it must come back as a list, not stringified.
    let ctx = setup().await;
    let ws = ctx.gui.create_workspace(None);
    ctx.gui.set_var(&ws, "selected_metarecords", json!(["aa", "bb"])).unwrap();

    let (status, body) = request(
        &ctx.router,
        "GET",
        &format!("/gui/workspaces/{ws}/vars/selected_metarecords"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["value"], json!(["aa", "bb"]));
}
