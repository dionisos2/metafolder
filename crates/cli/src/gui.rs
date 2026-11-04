//! `mf gui`: thin client over the GUI scripting HTTP API (doc
//! "GUI scripting endpoints"). Plain-text output designed for shell scripts.

use std::path::{Path, PathBuf};

use serde_json::{json, Value as Json};

use crate::client::{CliError, Client};

/// Resolves the GUI base URL: the explicit value (`--gui-url` /
/// `METAFOLDER_GUI_URL`) wins, else the `gui-port` of the GUI's `config.toml`
/// — the file the GUI itself starts from. No port is kept here (doc "No
/// runtime fallback"): that file missing or malformed, or without `gui-port`,
/// is a usage error naming it.
pub fn base_url(explicit: Option<String>, config: Option<&Path>) -> Result<String, CliError> {
    if let Some(url) = explicit {
        return Ok(url);
    }
    let path = config.ok_or_else(|| {
        CliError::Usage(
            "cannot determine the GUI configuration file: set HOME or use --gui-url".into(),
        )
    })?;
    let content = metafolder_core::config::read_required(path).map_err(CliError::Usage)?;
    let port = gui_port_from_config(&content)
        .map_err(|e| CliError::Usage(format!("in {}: {e}", path.display())))?;
    Ok(format!("http://127.0.0.1:{port}"))
}

/// The `gui-port` setting of a GUI `config.toml`, or why there is none.
fn gui_port_from_config(content: &str) -> Result<u16, String> {
    let table: toml::Table = toml::from_str(content).map_err(|e| e.to_string())?;
    let port = table.get("gui-port").ok_or("missing key `gui-port`")?;
    port.as_integer()
        .and_then(|n| u16::try_from(n).ok())
        .ok_or_else(|| format!("`gui-port` is not a port: {port}"))
}

/// The GUI config file location (doc "Configuration"):
/// `~/.config/metafolder/gui/config.toml`. The GUI writes no port discovery
/// file — its port lives in this config.
pub fn config_path() -> Option<PathBuf> {
    metafolder_core::config::crate_config_dir("gui").map(|dir| dir.join("config.toml"))
}

pub struct GuiCtx {
    pub client: Client,
}

impl GuiCtx {
    pub fn new(base_url: &str) -> Self {
        Self { client: Client::with_peer(base_url, "GUI") }
    }
}

const SLOTS: [&str; 2] = ["left", "right"];

/// This script's GUI run id, injected as `METAFOLDER_GUI_TASK` when the GUI
/// launched it. `None` outside the GUI, where every call carrying it is a
/// lenient no-op.
fn run_task() -> Option<String> {
    std::env::var("METAFOLDER_GUI_TASK").ok()
}

/// Adds this run's id to a request body, if there is one. Absent rather than
/// `null` outside the GUI, so a plain `mf gui …` sends exactly what it always
/// sent.
fn with_run_task(mut body: Json) -> Json {
    if let Some(task) = run_task() {
        body["task"] = json!(task);
    }
    body
}

fn check_slot(slot: &str) -> Result<(), CliError> {
    if SLOTS.contains(&slot) {
        Ok(())
    } else {
        Err(CliError::Usage(format!("invalid slot '{slot}' (expected 'left' or 'right')")))
    }
}

pub fn status(ctx: &GuiCtx) -> Result<i32, CliError> {
    let resp = ctx.client.get("/gui/status", &[])?;
    println!("{}", serde_json::to_string_pretty(&resp).expect("JSON serialization"));
    Ok(0)
}

/// The workspace shown in the focused slot, read from a `GET /gui/status` body.
fn focused_workspace(status: &Json) -> Result<&str, CliError> {
    status["layout"]
        .as_object()
        .into_iter()
        .flat_map(|layout| layout.values())
        .find(|slot| slot["focused"] == true)
        .and_then(|slot| slot["workspace_id"].as_str())
        .ok_or_else(|| CliError::Op("no focused workspace".into()))
}

/// Prints the active repository of the focused slot's workspace.
pub fn repo(ctx: &GuiCtx) -> Result<i32, CliError> {
    let status = ctx.client.get("/gui/status", &[])?;
    let workspace_id = focused_workspace(&status)?;
    let repo = status["workspaces"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|ws| ws["id"] == workspace_id)
        .and_then(|ws| ws["active_repo"].as_str())
        .ok_or_else(|| {
            CliError::Op(format!("workspace {workspace_id} has no active repository"))
        })?;
    println!("{repo}");
    Ok(0)
}

/// What a script should run on when it wants the list's query: the query the
/// metarecord list actually shows, finder included
/// (`metarecord-list:effective-query-text`) — the empty string being "match
/// all".
///
/// `None` when the list has not published one. That is *not* "match all" —
/// answering "everything" there would turn a script loose on the whole
/// repository. The checkbox selection has its own command (`mf gui selected`);
/// neither command second-guesses the other.
pub(crate) fn query_text(text: &Json) -> Option<String> {
    text.as_str().map(|text| text.trim().to_string())
}

/// `mf gui query [--workspace <id>]` — prints the query the metarecord list
/// shows (see [`query_text`]); an empty line means every metarecord.
pub fn query(ctx: &GuiCtx, workspace: Option<&str>) -> Result<i32, CliError> {
    let workspace = target_workspace(ctx, workspace)?;
    let resp = ctx.client.get(
        &format!("/gui/workspaces/{workspace}/vars/metarecord-list:effective-query-text"),
        &[],
    )?;
    match query_text(&resp["value"]) {
        Some(query) => {
            println!("{query}");
            Ok(0)
        }
        None => Err(CliError::Op(format!(
            "workspace {workspace} shows no query yet (its metarecord list has not run); \
             pass the query explicitly"
        ))),
    }
}

/// `mf gui selected [--workspace <id>]` — prints the checkbox selection's
/// UUIDs, one per line; no output when nothing is checked (a real state, not
/// a failure). The half of the old `mf gui query` precedence that grew into
/// its own command, so a script asks which it wants instead of inheriting one.
pub fn selected(ctx: &GuiCtx, workspace: Option<&str>) -> Result<i32, CliError> {
    let workspace = target_workspace(ctx, workspace)?;
    let resp =
        ctx.client.get(&format!("/gui/workspaces/{workspace}/vars/selected_metarecords"), &[])?;
    if let Some(uuid) = resp["value"].as_array() {
        for uuid in uuid.iter().filter_map(Json::as_str) {
            println!("{uuid}");
        }
    }
    Ok(0)
}

/// The workspace a command targets: `--workspace`, else the focused one.
fn target_workspace(ctx: &GuiCtx, workspace: Option<&str>) -> Result<String, CliError> {
    match workspace {
        Some(id) => Ok(id.to_string()),
        None => Ok(focused_workspace(&ctx.client.get("/gui/status", &[])?)?.to_string()),
    }
}

/// `mf gui messages [--workspace <id>] [--last <n>]` — prints the workspace's
/// message log, oldest first, each entry as it was written (a multi-line entry
/// stays multi-line). What the commands *said*, not how the shell shows it.
pub fn messages(
    ctx: &GuiCtx,
    workspace: Option<&str>,
    last: Option<usize>,
) -> Result<i32, CliError> {
    let workspace = target_workspace(ctx, workspace)?;
    let resp = ctx.client.get(&format!("/gui/workspaces/{workspace}/messages"), &[])?;
    let texts: Vec<&str> = resp["messages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| entry["text"].as_str())
        .collect();
    let skip = last.map_or(0, |n| texts.len().saturating_sub(n));
    for text in &texts[skip..] {
        println!("{text}");
    }
    Ok(0)
}

/// `mf gui status-bar [--workspace <id>]` — prints the last status-bar message
/// posted as `<kind>\t<text>` (`info`, `error`, `busy`…), whether or not the
/// bar still shows it; nothing at all when none was posted.
pub fn status_bar(ctx: &GuiCtx, workspace: Option<&str>) -> Result<i32, CliError> {
    let workspace = target_workspace(ctx, workspace)?;
    let resp = ctx.client.get(&format!("/gui/workspaces/{workspace}/messages"), &[])?;
    let status = &resp["status"];
    if let (Some(kind), Some(text)) = (status["kind"].as_str(), status["text"].as_str()) {
        println!("{kind}\t{text}");
    }
    Ok(0)
}

pub fn workspace_new(
    ctx: &GuiCtx,
    repo: Option<&str>,
    left: Option<&str>,
    right: Option<&str>,
) -> Result<i32, CliError> {
    let mut body = match repo {
        Some(repo) => json!({ "active_repo": repo }),
        None => json!({}),
    };
    // The panels the workspace opens on, so its default one is never mounted
    // (doc "Script sessions"). Absent rather than empty when none is named.
    for (slot, panel_type) in SLOTS.into_iter().zip([left, right]) {
        if let Some(panel_type) = panel_type {
            body["panels"][slot] = json!(panel_type);
        }
    }
    // The run id makes the new workspace belong to this script, so the GUI
    // scopes its questions and task entry to it (doc "Script sessions").
    let body = with_run_task(body);
    let resp = ctx.client.post("/gui/workspaces", &body)?;
    println!("{}", resp["id"].as_str().unwrap_or_default());
    Ok(0)
}

pub fn workspace_rm(ctx: &GuiCtx, id: &str) -> Result<i32, CliError> {
    ctx.client.request("DELETE", &format!("/gui/workspaces/{id}"), &[], None)?;
    Ok(0)
}

/// `mf gui layout` prints both slots ("-" = hidden); with a slot, prints
/// just that slot; with a slot and a value, assigns it ("-" = hide).
pub fn layout(ctx: &GuiCtx, slot: Option<&str>, value: Option<&str>) -> Result<i32, CliError> {
    if let Some(slot) = slot {
        check_slot(slot)?;
    }
    match (slot, value) {
        (None, _) => {
            let resp = ctx.client.get("/gui/layout", &[])?;
            for name in SLOTS {
                println!("{name} {}", resp[name].as_str().unwrap_or("-"));
            }
        }
        (Some(slot), None) => {
            let resp = ctx.client.get("/gui/layout", &[])?;
            println!("{}", resp[slot].as_str().unwrap_or("-"));
        }
        (Some(slot), Some(value)) => {
            let assigned = if value == "-" { Json::Null } else { json!(value) };
            ctx.client.request("PUT", "/gui/layout", &[], Some(&json!({slot: assigned})))?;
        }
    }
    Ok(0)
}

/// `mf gui view <slot>` prints the current panel type; with a type (and
/// optional `--path`), sets it.
/// How long `mf gui view` waits: a script must not hang on a GUI page that no
/// longer answers.
const VIEW_TIMEOUT_MS: u64 = 30_000;

pub fn view(
    ctx: &GuiCtx,
    slot: &str,
    panel_type: Option<&str>,
    path: Option<&str>,
) -> Result<i32, CliError> {
    check_slot(slot)?;
    let url = format!("/gui/panels/{slot}/view");
    match panel_type {
        None => {
            let resp = ctx.client.get(&url, &[])?;
            println!("{}", resp["type"].as_str().unwrap_or_default());
        }
        Some(panel_type) => {
            // A front for the GUI's own command, under its rules (doc "slot:show").
            let mut invocation = vec![json!("slot:show"), json!(slot), json!(panel_type)];
            if let Some(path) = path {
                invocation.push(json!(path));
            }
            let body = json!({"invocation": invocation, "timeout_ms": VIEW_TIMEOUT_MS});
            run_command(ctx, &body)?;
        }
    }
    Ok(0)
}

/// `mf gui progress` — a running script updates its own task-bar entry. The run
/// id defaults to `METAFOLDER_GUI_TASK` (injected by the GUI when it launched
/// the script); outside the GUI it is unset and the daemon-side call is a
/// harmless no-op, so scripts can call this unconditionally.
pub fn progress(
    ctx: &GuiCtx,
    done: Option<u64>,
    total: Option<u64>,
    phase: Option<&str>,
    task: Option<String>,
) -> Result<i32, CliError> {
    let task = task.or_else(run_task);
    let body = json!({"task": task, "done": done, "total": total, "phase": phase});
    ctx.client.request("POST", "/gui/progress", &[], Some(&body))?;
    Ok(0)
}

pub fn message(
    ctx: &GuiCtx,
    text: &str,
    workspace: Option<&str>,
    timeout_ms: Option<u64>,
) -> Result<i32, CliError> {
    let query: Vec<(&str, String)> =
        workspace.map(|ws| vec![("workspace_id", ws.to_string())]).unwrap_or_default();
    let body = json!({"text": text, "timeout_ms": timeout_ms});
    ctx.client.request("POST", "/gui/message", &query, Some(&body))?;
    Ok(0)
}

/// Prints the recorded bench measures as pretty JSON, or clears the buffer
/// (the driver script clears before each scenario, then reads after it).
pub fn bench(ctx: &GuiCtx, clear: bool) -> Result<i32, CliError> {
    if clear {
        ctx.client.post("/gui/bench/clear", &json!({}))?;
        return Ok(0);
    }
    let resp = ctx.client.get("/gui/bench", &[])?;
    println!("{}", serde_json::to_string_pretty(&resp).expect("JSON serialization"));
    Ok(0)
}

/// Runs an arbitrary command invocation through the GUI's dispatcher (the
/// same path as the command input). Blocks until the command resolves; exit 1
/// on error/timeout/close.
/// `mf gui command [--confirm yes|no] <invocation…>` — runs a command through
/// the GUI's dispatcher. With `confirm`, every confirmation the command asks
/// while it runs is answered so (`true` = accept) and its question printed, one
/// per line; without, the dialogs are the user's.
pub fn command(
    ctx: &GuiCtx,
    invocation: &str,
    timeout_ms: Option<u64>,
    confirm: Option<bool>,
) -> Result<i32, CliError> {
    let mut body = json!({"invocation": invocation, "timeout_ms": timeout_ms});
    if let Some(confirm) = confirm {
        body["confirm"] = json!(confirm);
    }
    run_command(ctx, &body)
}

/// Runs a `POST /gui/command` body; its outcome is the exit code or the error.
fn run_command(ctx: &GuiCtx, body: &Json) -> Result<i32, CliError> {
    let resp = ctx.client.post("/gui/command", body)?;
    for question in resp["confirmations"].as_array().into_iter().flatten() {
        println!("{}", question.as_str().unwrap_or_default());
    }
    match resp["event"].as_str() {
        Some("ok") => Ok(0),
        Some("error") => {
            Err(CliError::Op(resp["message"].as_str().unwrap_or("command failed").to_string()))
        }
        Some(other) => Err(CliError::Op(format!("command did not run: {other}"))),
        None => Err(CliError::Op("malformed GUI response".into())),
    }
}

/// Blocks until one of `keys` is pressed and prints it; exit 1 on
/// timeout or when the GUI closes the wait. `prompt` is the question shown in
/// the GUI's dedicated input bar while waiting (separate from status messages).
///
/// The GUI refuses a wait that asks for one of the keys it keeps for the user
/// (`escape`, and the combos that open the command input or toggle the script
/// keys — doc "Script sessions"); that comes back as a 400, so a script that
/// wants its own quit key offers something else (`q`, by convention).
pub fn input(
    ctx: &GuiCtx,
    keys: &[String],
    prompt: Option<&str>,
    timeout_ms: Option<u64>,
) -> Result<i32, CliError> {
    let body = with_run_task(json!({"keys": keys, "prompt": prompt, "timeout_ms": timeout_ms}));
    let resp = ctx.client.post("/gui/input", &body)?;
    match resp["event"].as_str() {
        Some("answer") => {
            println!("{}", resp["value"].as_str().unwrap_or_default());
            Ok(0)
        }
        Some(other) => Err(CliError::Op(format!("input wait ended: {other}"))),
        None => Err(CliError::Op("malformed GUI response".into())),
    }
}

/// Blocks until the prompt is confirmed and prints the text; exit 1 on
/// cancel/timeout/close. `completions` are offered by the command input
/// autocomplete; with `completions_stdin`, one more completion per stdin
/// line (stopping at the first empty line or EOF).
pub fn prompt(
    ctx: &GuiCtx,
    text: &str,
    completions: &[String],
    completions_stdin: bool,
    timeout_ms: Option<u64>,
) -> Result<i32, CliError> {
    let mut completions = completions.to_vec();
    if completions_stdin {
        for line in std::io::stdin().lines() {
            let line = line.map_err(|e| CliError::Op(format!("cannot read stdin: {e}")))?;
            if line.is_empty() {
                break;
            }
            completions.push(line);
        }
    }
    let body = with_run_task(
        json!({"prompt": text, "completions": completions, "timeout_ms": timeout_ms}),
    );
    let resp = ctx.client.post("/gui/prompt", &body)?;
    match resp["event"].as_str() {
        Some("confirm") => {
            println!("{}", resp["text"].as_str().unwrap_or_default());
            Ok(0)
        }
        Some(other) => Err(CliError::Op(format!("prompt ended: {other}"))),
        None => Err(CliError::Op("malformed GUI response".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_file(content: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("metafolder-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("mf_gui_cfg_{}.toml", uuid::Uuid::new_v4()));
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn test_base_url_explicit_wins() {
        let url = base_url(Some("http://127.0.0.1:9999".into()), None).unwrap();
        assert_eq!(url, "http://127.0.0.1:9999");
    }

    #[test]
    fn test_base_url_reads_the_gui_port_of_the_gui_config() {
        let path = config_file("gui-port = 7600\n");
        assert_eq!(base_url(None, Some(&path)).unwrap(), "http://127.0.0.1:7600");
        std::fs::remove_file(&path).unwrap();
    }

    /// No port in the code (doc "No runtime fallback"): a GUI configuration
    /// missing, malformed or without `gui-port` is a usage error naming it.
    #[test]
    fn test_base_url_has_no_built_in_port() {
        let missing = PathBuf::from("/nonexistent/config.toml");
        let err = base_url(None, Some(&missing)).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(err.message().contains("/nonexistent/config.toml"), "{}", err.message());

        for content in ["daemon-port = 7523\n", "gui-port = \"x\"\n", "gui-port = 70000\n", "= ="] {
            let path = config_file(content);
            let err = base_url(None, Some(&path)).unwrap_err();
            assert_eq!(err.exit_code(), 2, "{content}");
            assert!(err.message().contains(&path.display().to_string()), "{}", err.message());
            if content.contains("port") {
                assert!(err.message().contains("gui-port"), "{content}: {}", err.message());
            }
            std::fs::remove_file(&path).unwrap();
        }
        assert_eq!(base_url(None, None).unwrap_err().exit_code(), 2);
    }

    #[test]
    fn test_query_the_list_text_is_the_answer() {
        let text = query_text(&json!("  rating > 3  "));
        assert_eq!(text, Some("rating > 3".to_string()));
    }

    #[test]
    fn test_query_match_all_is_the_empty_text() {
        assert_eq!(query_text(&json!("")), Some(String::new()));
    }

    #[test]
    fn test_query_nothing_published_is_none_not_match_all() {
        assert_eq!(query_text(&Json::Null), None);
    }
}
