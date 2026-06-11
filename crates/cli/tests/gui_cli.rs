//! End-to-end tests for `mf gui`: run the real `mf` binary against a stub
//! GUI HTTP server recording the requests it receives.

use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use axum::extract::{Path, Query, RawQuery, State};
use axum::routing::{delete, get, post, put};
use axum::Json;
use serde_json::{json, Value as Json_};

// ── Stub GUI server ───────────────────────────────────────────────────────────

#[derive(Clone, Default)]
struct Recorded {
    requests: Arc<Mutex<Vec<(String, String, Json_)>>>,
}

impl Recorded {
    fn push(&self, method: &str, path: &str, body: Json_) {
        self.requests.lock().unwrap().push((method.into(), path.into(), body));
    }

    fn all(&self) -> Vec<(String, String, Json_)> {
        self.requests.lock().unwrap().clone()
    }
}

/// A stub GUI answering like the real `/gui/*` API, with configurable
/// input/prompt outcomes.
struct StubGui {
    url: String,
    recorded: Recorded,
}

fn start_stub(input_response: Json_, prompt_response: Json_) -> StubGui {
    let recorded = Recorded::default();
    let state = recorded.clone();
    let input_response = Arc::new(input_response);
    let prompt_response = Arc::new(prompt_response);

    let status = json!({
        "workspaces": [
            {"id": "ws-1", "name": "Workspace 1",
             "active_repo": "11111111111111111111111111111111"},
            {"id": "ws-2", "name": "Workspace 2", "active_repo": null}
        ],
        "layout": {
            "left":  {"workspace_id": "ws-1", "panel_type": "entry-list", "focused": true},
            "right": {"workspace_id": "ws-2", "panel_type": "file", "focused": false}
        },
        "daemon_connected": true,
        "input_wait_active": false
    });

    let app = axum::Router::new()
        .route("/gui/status", get(move || {
            let status = status.clone();
            async move { Json(status) }
        }))
        .route(
            "/gui/workspaces",
            post(|State(s): State<Recorded>, Json(body): Json<Json_>| async move {
                s.push("POST", "/gui/workspaces", body);
                Json(json!({"id": "ws-9"}))
            }),
        )
        .route(
            "/gui/workspaces/:id",
            delete(|State(s): State<Recorded>, Path(id): Path<String>| async move {
                s.push("DELETE", &format!("/gui/workspaces/{id}"), Json_::Null);
                axum::http::StatusCode::NO_CONTENT
            }),
        )
        .route(
            "/gui/layout",
            get(|| async { Json(json!({"left": "ws-1", "right": null})) }).put(
                |State(s): State<Recorded>, Json(body): Json<Json_>| async move {
                    s.push("PUT", "/gui/layout", body);
                    Json(json!({}))
                },
            ),
        )
        .route(
            "/gui/panels/:slot/view",
            put(|State(s): State<Recorded>, Path(slot): Path<String>, Json(body): Json<Json_>| async move {
                s.push("PUT", &format!("/gui/panels/{slot}/view"), body);
                Json(json!({}))
            })
            .get(|Path(slot): Path<String>| async move {
                Json(json!({"type": "entry-list", "status": "ready", "slot": slot}))
            }),
        )
        .route(
            "/gui/message",
            post(
                |State(s): State<Recorded>,
                 Query(q): Query<std::collections::HashMap<String, String>>,
                 RawQuery(_): RawQuery,
                 Json(body): Json<Json_>| async move {
                    let suffix = q
                        .get("workspace_id")
                        .map(|w| format!("?workspace_id={w}"))
                        .unwrap_or_default();
                    s.push("POST", &format!("/gui/message{suffix}"), body);
                    Json(json!({}))
                },
            ),
        )
        .route(
            "/gui/input",
            post(move |State(s): State<Recorded>, Json(body): Json<Json_>| {
                let resp = input_response.clone();
                async move {
                    s.push("POST", "/gui/input", body);
                    Json((*resp).clone())
                }
            }),
        )
        .route(
            "/gui/prompt",
            post(move |State(s): State<Recorded>, Json(body): Json<Json_>| {
                let resp = prompt_response.clone();
                async move {
                    s.push("POST", "/gui/prompt", body);
                    Json((*resp).clone())
                }
            }),
        )
        .with_state(state);

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let listener = tokio::net::TcpListener::from_std(listener).unwrap();
            axum::serve(listener, app).await.unwrap();
        });
    });
    StubGui { url: format!("http://127.0.0.1:{}", addr.port()), recorded }
}

fn stub() -> StubGui {
    start_stub(json!({"event": "answer", "value": "1"}), json!({"event": "confirm", "text": "jazz"}))
}

struct Out {
    code: i32,
    stdout: String,
    stderr: String,
}

fn mf_gui(gui: &StubGui, args: &[&str]) -> Out {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mf"));
    cmd.arg("gui").arg("--gui-url").arg(&gui.url).args(args);
    cmd.env_remove("METAFOLDER_GUI_URL");
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(Stdio::null());
    let output = cmd.output().unwrap();
    Out {
        code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    }
}

fn assert_ok(out: &Out) {
    assert_eq!(out.code, 0, "expected success.\nstdout: {}\nstderr: {}", out.stdout, out.stderr);
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[test]
fn test_gui_status_prints_json() {
    let gui = stub();
    let out = mf_gui(&gui, &["status"]);
    assert_ok(&out);
    let parsed: Json_ = serde_json::from_str(&out.stdout).expect("status should print JSON");
    assert_eq!(parsed["daemon_connected"], true);
}

#[test]
fn test_gui_repo_prints_the_focused_workspace_repo() {
    let gui = stub();
    let out = mf_gui(&gui, &["repo"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), "11111111111111111111111111111111");
}

#[test]
fn test_gui_url_from_environment() {
    let gui = stub();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mf"));
    cmd.args(["gui", "repo"]).env("METAFOLDER_GUI_URL", &gui.url);
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(Stdio::null());
    let output = cmd.output().unwrap();
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "11111111111111111111111111111111"
    );
}

#[test]
fn test_gui_workspace_new_and_rm() {
    let gui = stub();
    let out = mf_gui(&gui, &["workspace", "new", "--repo", "22222222222222222222222222222222"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), "ws-9");

    let out = mf_gui(&gui, &["workspace", "rm", "ws-9"]);
    assert_ok(&out);

    let requests = gui.recorded.all();
    assert_eq!(
        requests[0],
        (
            "POST".into(),
            "/gui/workspaces".into(),
            json!({"active_repo": "22222222222222222222222222222222"})
        )
    );
    assert_eq!(requests[1].1, "/gui/workspaces/ws-9");
}

#[test]
fn test_gui_layout_get_one_slot_and_set() {
    let gui = stub();
    let out = mf_gui(&gui, &["layout"]);
    assert_ok(&out);
    assert_eq!(out.stdout, "left ws-1\nright -\n");

    let out = mf_gui(&gui, &["layout", "left"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), "ws-1");

    let out = mf_gui(&gui, &["layout", "right"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), "-");

    let out = mf_gui(&gui, &["layout", "left", "ws-2"]);
    assert_ok(&out);
    let out = mf_gui(&gui, &["layout", "right", "-"]);
    assert_ok(&out);

    let requests = gui.recorded.all();
    assert_eq!(requests[0], ("PUT".into(), "/gui/layout".into(), json!({"left": "ws-2"})));
    assert_eq!(requests[1], ("PUT".into(), "/gui/layout".into(), json!({"right": null})));
}

#[test]
fn test_gui_view_set_and_get() {
    let gui = stub();
    let out = mf_gui(&gui, &["view", "left", "file", "--path", "/tmp/img.jpg"]);
    assert_ok(&out);
    let out = mf_gui(&gui, &["view", "right", "entry-detail"]);
    assert_ok(&out);

    let requests = gui.recorded.all();
    assert_eq!(
        requests[0],
        (
            "PUT".into(),
            "/gui/panels/left/view".into(),
            json!({"type": "file", "path": "/tmp/img.jpg"})
        )
    );
    assert_eq!(
        requests[1],
        ("PUT".into(), "/gui/panels/right/view".into(), json!({"type": "entry-detail"}))
    );

    let out = mf_gui(&gui, &["view", "left"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), "entry-list");
}

#[test]
fn test_gui_message_with_workspace_and_timeout() {
    let gui = stub();
    let out = mf_gui(&gui, &["message", "hello", "--workspace", "ws-1", "--timeout-ms", "500"]);
    assert_ok(&out);
    let requests = gui.recorded.all();
    assert_eq!(
        requests[0],
        (
            "POST".into(),
            "/gui/message?workspace_id=ws-1".into(),
            json!({"text": "hello", "timeout_ms": 500})
        )
    );
}

#[test]
fn test_gui_input_prints_the_answer() {
    let gui = stub();
    let out = mf_gui(&gui, &["input", "1", "2", "escape"]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), "1");
    let requests = gui.recorded.all();
    assert_eq!(
        requests[0],
        ("POST".into(), "/gui/input".into(), json!({"keys": ["1", "2", "escape"], "timeout_ms": null}))
    );
}

#[test]
fn test_gui_input_timeout_fails() {
    let gui = start_stub(json!({"event": "timeout"}), json!({"event": "confirm", "text": ""}));
    let out = mf_gui(&gui, &["input", "1", "--timeout-ms", "10"]);
    assert_eq!(out.code, 1, "stdout: {}", out.stdout);
    assert_eq!(out.stdout, "");
}

#[test]
fn test_gui_prompt_prints_the_text() {
    let gui = stub();
    let out = mf_gui(&gui, &["prompt", "Tag name: "]);
    assert_ok(&out);
    assert_eq!(out.stdout.trim(), "jazz");
    let requests = gui.recorded.all();
    assert_eq!(
        requests[0],
        (
            "POST".into(),
            "/gui/prompt".into(),
            json!({"prompt": "Tag name: ", "completions": [], "timeout_ms": null})
        )
    );
}

#[test]
fn test_gui_prompt_with_completions() {
    let gui = stub();
    let out = mf_gui(
        &gui,
        &["prompt", "Tag: ", "--completion", "jazz", "--completion", "rock"],
    );
    assert_ok(&out);
    let requests = gui.recorded.all();
    assert_eq!(
        requests[0],
        (
            "POST".into(),
            "/gui/prompt".into(),
            json!({"prompt": "Tag: ", "completions": ["jazz", "rock"], "timeout_ms": null})
        )
    );
}

#[test]
fn test_gui_prompt_with_completions_from_stdin() {
    let gui = stub();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_mf"));
    cmd.args(["gui", "--gui-url", &gui.url, "prompt", "Tag: ", "--completions-stdin"]);
    cmd.env_remove("METAFOLDER_GUI_URL");
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    use std::io::Write as _;
    child.stdin.take().unwrap().write_all(b"jazz\nrock\n\n").unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(0), "stderr: {}", String::from_utf8_lossy(&output.stderr));

    let requests = gui.recorded.all();
    assert_eq!(
        requests[0],
        (
            "POST".into(),
            "/gui/prompt".into(),
            json!({"prompt": "Tag: ", "completions": ["jazz", "rock"], "timeout_ms": null})
        )
    );
}

#[test]
fn test_gui_prompt_cancel_fails() {
    let gui = start_stub(json!({"event": "answer", "value": ""}), json!({"event": "cancel"}));
    let out = mf_gui(&gui, &["prompt", "Tag name: "]);
    assert_eq!(out.code, 1, "stdout: {}", out.stdout);
    assert_eq!(out.stdout, "");
}
