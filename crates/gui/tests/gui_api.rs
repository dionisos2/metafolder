//! The GUI scripting HTTP API on port 7524 (spec-gui "Scripting / GUI
//! API"), driven with oneshot like the daemon tests.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::routing::get;
use axum::Json;
use http_body_util::BodyExt;
use metafolder_gui::config::ConfigDir;
use metafolder_gui::daemon_proxy::DaemonProxy;
use metafolder_gui::events;
use metafolder_gui::notifier::RecordingNotifier;
use metafolder_gui::server::{self, ServerState};
use metafolder_gui::server::input_wait::InputWait;
use metafolder_gui::state::GuiState;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

struct Ctx {
    _guard: tempfile::TempDir,
    notifier: Arc<RecordingNotifier>,
    gui: Arc<GuiState>,
    input: Arc<InputWait>,
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
    let config = Arc::new(ConfigDir::at(guard.path().join("metafolder-gui")));
    config.install_defaults().unwrap();

    let notifier = Arc::new(RecordingNotifier::new());
    let gui = Arc::new(GuiState::new(notifier.clone()));
    let daemon = Arc::new(DaemonProxy::new(daemon_url.to_string()));
    let keybindings = Arc::new(std::sync::Mutex::new(config.load_keybindings().unwrap()));
    let input = Arc::new(InputWait::new());

    let state = ServerState {
        config,
        gui: gui.clone(),
        daemon,
        keybindings,
        input: input.clone(),
    };
    Ctx {
        _guard: guard,
        notifier,
        gui,
        input: input.clone(),
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
    assert_eq!(body, json!([{"id": "ws-1", "name": "Workspace 1", "active_repo": null}]));

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

// ── Layout ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_layout_get_and_partial_put() {
    let ctx = setup().await;

    let (status, body) = request(&ctx.router, "GET", "/gui/layout", None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"left": "ws-1", "right": null}));

    // Partial update: only the right slot.
    let (status, _) = request(
        &ctx.router,
        "PUT",
        "/gui/layout",
        Some(json!({"right": "ws-1"})),
    )
    .await;
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

    let (status, _) = request(
        &ctx.router,
        "PUT",
        "/gui/panels/left/view",
        Some(json!({"type": "message"})),
    )
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
        Some(json!({"type": "file", "path": "/tmp/picture.jpg"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        ctx.gui.get_var("ws-1", "selected_paths").unwrap(),
        json!(["/tmp/picture.jpg"])
    );
    // No active repo in ws-1: no entry lookup is possible.
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

    let (status, _) = request(
        &ctx.router,
        "POST",
        "/gui/message?workspace_id=ws-99",
        Some(json!({"text": "x"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

// ── Input wait ────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_input_wait_resolves_on_answer() {
    let ctx = setup().await;
    ctx.notifier.clear();

    let router = ctx.router.clone();
    let waiting = tokio::spawn(async move {
        request(&router, "POST", "/gui/input", Some(json!({"keys": ["left", "right"]}))).await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Temporary bindings were installed and pushed.
    let pushes = ctx.notifier.payloads(events::KEYBINDINGS_CHANGED);
    assert!(!pushes.is_empty());
    let last = pushes.last().unwrap()["bindings"].to_string();
    assert!(last.contains("answer:send left"), "missing temp binding: {last}");

    assert!(ctx.input.resolve_answer("left"));
    let (status, body) = waiting.await.unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"event": "answer", "value": "left"}));

    // Temp bindings removed after resolution.
    let last = ctx.notifier.payloads(events::KEYBINDINGS_CHANGED);
    assert!(!last.last().unwrap()["bindings"].to_string().contains("answer:send left"));
    // No more waiters: answers go nowhere.
    assert!(!ctx.input.resolve_answer("left"));
}

#[tokio::test]
async fn test_concurrent_input_waits_conflict() {
    let ctx = setup().await;
    let router = ctx.router.clone();
    let waiting = tokio::spawn(async move {
        request(&router, "POST", "/gui/input", Some(json!({}))).await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let (status, _) = request(&ctx.router, "POST", "/gui/input", Some(json!({}))).await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, _) = request(&ctx.router, "POST", "/gui/prompt", Some(json!({"prompt": "?"}))).await;
    assert_eq!(status, StatusCode::CONFLICT);

    ctx.input.close_all();
    let (_, body) = waiting.await.unwrap();
    assert_eq!(body, json!({"event": "closed"}));
}

#[tokio::test]
async fn test_input_wait_timeout() {
    let ctx = setup().await;
    let (status, body) = request(
        &ctx.router,
        "POST",
        "/gui/input",
        Some(json!({"timeout_ms": 50})),
    )
    .await;
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
        request(
            &router,
            "POST",
            "/gui/prompt",
            Some(json!({"prompt": "Enter a rating (1-5): "})),
        )
        .await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    let prompts = ctx.notifier.payloads(events::PROMPT_REQUESTED);
    assert_eq!(prompts[0]["prompt"], "Enter a rating (1-5): ");

    assert!(ctx.input.resolve_prompt(true, Some("4".into())));
    let (status, body) = waiting.await.unwrap();
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, json!({"event": "confirm", "text": "4"}));

    // Cancel path.
    let router = ctx.router.clone();
    let waiting = tokio::spawn(async move {
        request(&router, "POST", "/gui/prompt", Some(json!({"prompt": "? "}))).await
    });
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(ctx.input.resolve_prompt(false, None));
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

    assert!(ctx.input.resolve_prompt(true, Some("jazz".into())));
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
    assert!(ctx.input.resolve_prompt(false, None));
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
