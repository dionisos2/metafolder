//! HTTP proxy to the metafolder daemon (doc "Connection to the daemon").
//! Panels and the shell go through this backend client: the
//! WebView cannot call the daemon directly (no CORS there, and the
//! daemon must stay GUI-agnostic). Tracks reachability and emits
//! `daemon-health-changed` on transitions.

use crate::events;
use crate::state::GuiState;
use metafolder_core::sync::MutexExt;
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::Mutex;
use std::time::Duration;

#[derive(Serialize, Debug, PartialEq)]
pub struct ProxyResponse {
    pub status: u16,
    pub body: Value,
}

/// The message of a daemon (or task) `{"error": …}` body, else `fallback()`.
pub fn error_message(body: &Value, fallback: impl FnOnce() -> String) -> String {
    body["error"].as_str().map_or_else(fallback, str::to_string)
}

/// The outcome of one `/health` probe (doc "Connection to the daemon").
///
/// `reachable` alone drives the "daemon unreachable" banner; `compatible`
/// additionally distinguishes a *reachable but wrong-version* daemon (the GUI
/// and daemon were built from sources whose wire contract differs — see
/// [`metafolder_core::API_VERSION`]) so the shell can warn instead of silently
/// serving broken data.
#[derive(Clone, Copy, PartialEq, Debug)]
struct HealthOutcome {
    reachable: bool,
    /// Whether the daemon's reported `api_version` matches ours. Meaningful
    /// only when `reachable`; `false` when unreachable.
    compatible: bool,
    /// The daemon's reported `api_version`, if any (absent on a pre-versioning
    /// daemon or when unreachable).
    daemon_api: Option<u32>,
}

impl HealthOutcome {
    /// The `daemon-health-changed` payload announcing this outcome.
    fn payload(&self) -> Value {
        json!({
            "connected": self.reachable,
            "compatible": self.compatible,
            "daemon_api_version": self.daemon_api,
            "gui_api_version": metafolder_core::API_VERSION,
        })
    }
}

pub struct DaemonProxy {
    client: reqwest::Client,
    base_url: Mutex<String>,
    /// Last known health; `None` until the first check.
    health: Mutex<Option<HealthOutcome>>,
    /// Cached daemon session token (doc "Session tokens"), read lazily from the token
    /// file. Stable across daemon restarts, so caching is safe; cleared and
    /// re-read once on a 401 (covers the daemon having regenerated it).
    token: Mutex<Option<String>>,
    /// Cursor into the daemon's diagnostics feed: the last entry id drained
    /// into the message panels. Starts at 0 so the first poll picks up whatever
    /// the daemon already warned about — including why a repository failed to
    /// load, which happens before the GUI is up. With the run of the daemon
    /// it belongs to (`run` of the page): ids start again at 1 with every run.
    diagnostics_since: Mutex<(Option<String>, u64)>,
    /// Timing of the calls made through this proxy (doc "Correlating the GUI and the daemon"): what
    /// the *user* waited for, next to what the daemon spent.
    slow: crate::slow::SlowLog,
    /// Repository uuid → its `internal_dir`, as the daemon reported it. Only
    /// filled when a call was slow, so an ordinary session never asks.
    internal_dirs: Mutex<std::collections::HashMap<String, std::path::PathBuf>>,
    /// The calls in flight that the frontend may abort, by the id it gave
    /// them: firing one drops its request, and with it the connection — which
    /// the daemon takes as a cancel of the query it was running (doc "Query limits").
    aborts: Mutex<Aborts>,
}

tokio::task_local! {
    /// Set around a request that is not the user's activity.
    static NOT_ACTIVITY: bool;
}

/// The command the shell is running, whose writes — this proxy's and the
/// blocking client's — are in a log context of its own (doc "Log
/// contexts"). The process's: one GUI, one shell.
static COMMAND: Mutex<Option<RunningCommand>> = Mutex::new(None);

/// The command under way: its key (the shell's handle on it) and name, how
/// many commands deep the shell is in it — one run from inside it is part of
/// it —, and the context it created in each repository, at its first request
/// there that may write.
struct RunningCommand {
    key: String,
    name: String,
    depth: usize,
    created: std::collections::HashMap<uuid::Uuid, i64>,
}

/// What a request is to name as its log context.
pub(crate) enum LogContext {
    /// This one (`None`: none).
    Send(Option<i64>),
    /// The command's own, to create first: `POST /repos/<repo>/labels` with
    /// `body`, then [`context_created`].
    Create { key: String, repo: uuid::Uuid, body: Value, parent: Option<i64> },
}

/// The log context of a request to `path`: the running command's own in that
/// repository — to create when the request may write and it has none yet —,
/// else the work context in force there (doc "The GUI's work context").
pub(crate) fn log_context_for(method: &str, path: &str) -> LogContext {
    use metafolder_core::context;
    let work = crate::work_context::label_for(path);
    let Some(repo) = context::repo_of(path) else { return LogContext::Send(work) };
    let guard = COMMAND.lock_recover();
    let Some(running) = guard.as_ref() else { return LogContext::Send(work) };
    if let Some(&own) = running.created.get(&repo) {
        return LogContext::Send(Some(own));
    }
    if !context::may_write(method, path) {
        return LogContext::Send(work);
    }
    LogContext::Create {
        key: running.key.clone(),
        repo,
        body: serde_json::json!({"name": running.name, "description": null, "parent": work}),
        parent: work,
    }
}

/// The command `key` created context `id` in `repo`: the context its
/// requests there name from now on — the first one created, should two
/// requests have raced to create it.
pub(crate) fn context_created(key: &str, repo: uuid::Uuid, id: i64) -> i64 {
    let mut guard = COMMAND.lock_recover();
    match guard.as_mut().filter(|r| r.key == key) {
        Some(running) => *running.created.entry(repo).or_insert(id),
        None => id,
    }
}

/// How long an abort that found no call in flight is kept for the call it may
/// have overtaken. The frontend invokes `daemon_abort` and `daemon_request`
/// separately, so the abort can arrive first; a call that really did finish
/// leaves an entry that only has to outlive the race, not the session.
const EARLY_ABORT_TTL: std::time::Duration = std::time::Duration::from_secs(30);

#[derive(Default)]
struct Aborts {
    /// The calls in flight, by id.
    in_flight: std::collections::HashMap<String, tokio::sync::oneshot::Sender<()>>,
    /// Aborts that arrived before their call, by id, with when they did.
    early: std::collections::HashMap<String, std::time::Instant>,
}

impl DaemonProxy {
    /// A proxy that times nothing (tests, and any construction site that has no
    /// configuration to hand).
    pub fn new(base_url: String) -> Self {
        Self::with_slow_threshold(base_url, 0)
    }

    pub fn with_slow_threshold(base_url: String, slow_threshold_ms: u64) -> Self {
        DaemonProxy {
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(2))
                // The daemon never redirects; following one would let a
                // crafted path/response steer the request to another host
                // (SSRF). Refuse redirects outright.
                .redirect(reqwest::redirect::Policy::none())
                // The daemon is on loopback, always. reqwest otherwise follows
                // `HTTP_PROXY`/`ALL_PROXY` — loopback included — and would hand
                // the session token and repository data to that proxy.
                .no_proxy()
                .build()
                .expect("reqwest client"),
            base_url: Mutex::new(base_url),
            health: Mutex::new(None),
            token: Mutex::new(None),
            diagnostics_since: Mutex::new((None, 0)),
            slow: crate::slow::SlowLog::new(slow_threshold_ms),
            internal_dirs: Mutex::new(std::collections::HashMap::new()),
            aborts: Mutex::new(Aborts::default()),
        }
    }

    /// A command starts: its key, which [`Self::command_end`] takes back. One
    /// started while another runs is part of it, and gets the outer key — the
    /// approximation of a shell that knows which command runs, not which call
    /// each command made: two commands running at once share a context.
    pub fn command_begin(&self, name: &str) -> String {
        let mut guard = COMMAND.lock_recover();
        match guard.as_mut() {
            Some(running) => {
                running.depth += 1;
                running.key.clone()
            }
            None => {
                let key = uuid::Uuid::new_v4().as_simple().to_string();
                *guard = Some(RunningCommand {
                    key: key.clone(),
                    name: name.into(),
                    depth: 1,
                    created: Default::default(),
                });
                key
            }
        }
    }

    /// The command `key` ended; a key that is not the current one changes
    /// nothing.
    pub fn command_end(&self, key: &str) {
        let mut guard = COMMAND.lock_recover();
        if let Some(running) = guard.as_mut().filter(|r| r.key == key) {
            running.depth -= 1;
            if running.depth == 0 {
                *guard = None;
            }
        }
    }

    /// The daemon token, read from the token file and cached. `None` when the
    /// file is missing (daemon not running, or not as this user).
    fn token(&self) -> Option<String> {
        let mut guard = self.token.lock_recover();
        if guard.is_none() {
            *guard = metafolder_core::auth::read_token("daemon").ok();
        }
        guard.clone()
    }

    fn invalidate_token(&self) {
        *self.token.lock_recover() = None;
    }

    pub fn base_url(&self) -> String {
        self.base_url.lock_recover().clone()
    }

    pub fn set_url(&self, url: String) {
        *self.base_url.lock_recover() = url;
    }

    /// Forwards one request to the daemon. Daemon-level errors (4xx/5xx)
    /// are passed through with their status; only transport failures
    /// are `Err`.
    pub async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> Result<ProxyResponse, String> {
        self.request_with_context(method, path, body, None, None).await
    }

    /// [`Self::request`] for a poll, which says it is not the user's activity
    /// (doc "Folding in idle time"): the daemon folds its pending lists only
    /// after a quiet while, and a poll every few seconds is not the user.
    pub async fn request_polling(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
    ) -> Result<ProxyResponse, String> {
        self.not_activity(self.request(method, path, body)).await
    }

    /// Runs `call` so that the requests it makes say they are not activity.
    pub async fn not_activity<F: std::future::Future>(&self, call: F) -> F::Output {
        NOT_ACTIVITY.scope(true, call).await
    }

    /// Aborts the call in flight registered as `id`; `false` when there is
    /// none (it finished, or has not reached the proxy yet — in which case it
    /// is dropped on arrival).
    pub fn abort(&self, id: &str) -> bool {
        let mut aborts = self.aborts.lock_recover();
        match aborts.in_flight.remove(id) {
            Some(fire) => fire.send(()).is_ok(),
            None => {
                let now = std::time::Instant::now();
                aborts.early.retain(|_, at| now.duration_since(*at) < EARLY_ABORT_TTL);
                aborts.early.insert(id.to_string(), now);
                false
            }
        }
    }

    /// The same, plus what the user asked for in their own words — the DSL text
    /// of a query, the command that ran. The daemon receives the query IR and
    /// cannot reconstruct it, so the client is the only side that can say it
    /// (doc "Correlating the GUI and the daemon").
    ///
    /// With an `abort_id`, the call can be dropped while in flight by
    /// [`Self::abort`] — a query the user has already replaced — and then
    /// answers `Err("aborted")`.
    pub async fn request_with_context(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        client_context: Option<&str>,
        abort_id: Option<&str>,
    ) -> Result<ProxyResponse, String> {
        let started = std::time::Instant::now();
        let op_id = crate::slow::new_op_id();
        let request = self.request_inner(method, path, body, Some((&op_id, client_context)));
        let out = match abort_id {
            None => request.await,
            Some(id) => {
                let (fire, fired) = tokio::sync::oneshot::channel();
                {
                    let mut aborts = self.aborts.lock_recover();
                    if aborts.early.remove(id).is_some() {
                        return Err("aborted".to_string());
                    }
                    aborts.in_flight.insert(id.to_string(), fire);
                }
                let out = tokio::select! {
                    out = request => out,
                    Ok(()) = fired => Err("aborted".to_string()),
                };
                self.aborts.lock_recover().in_flight.remove(id);
                out
            }
        };
        self.observe(method, path, started.elapsed(), &op_id, client_context).await;
        out
    }

    async fn request_inner(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        timing: Option<(&str, Option<&str>)>,
    ) -> Result<ProxyResponse, String> {
        validate_path(path)?;
        let context = match self.log_context(method, path).await? {
            Ok(context) => context,
            Err(refused) => return Ok(refused),
        };
        let response = self.send(method, path, body.clone(), self.token(), timing, context).await?;
        // A 401 means our cached token is stale (the daemon regenerated it).
        // Drop it, re-read the file once and retry.
        if response.status == 401 {
            self.invalidate_token();
            if let Some(token) = self.token() {
                return self.send(method, path, body, Some(token), timing, context).await;
            }
        }
        // The work context's label was pruned meanwhile: the daemon wrote
        // nothing, the context is closed, and the request goes again outside it.
        if crate::work_context::forget_if_gone(path, response.status, &response.body) {
            let context = match self.log_context(method, path).await? {
                Ok(context) => context,
                Err(refused) => return Ok(refused),
            };
            return self.send(method, path, body, self.token(), timing, context).await;
        }
        Ok(response)
    }

    /// The log context to send a request with ([`log_context_for`]), the
    /// command's own created first if it is to be; `Err` with the daemon's
    /// answer when it refused to create it.
    async fn log_context(
        &self,
        method: &str,
        path: &str,
    ) -> Result<Result<Option<i64>, ProxyResponse>, String> {
        let mut attempts = 0;
        loop {
            attempts += 1;
            let (key, repo, body, parent) = match log_context_for(method, path) {
                LogContext::Send(context) => return Ok(Ok(context)),
                LogContext::Create { key, repo, body, parent } => (key, repo, body, parent),
            };
            let labels = format!("/repos/{}/labels", repo.as_simple());
            let created = Box::pin(self.request_inner("POST", &labels, Some(body), None)).await?;
            if let Some(id) = created.body["id"].as_i64().filter(|_| created.status < 300) {
                return Ok(Ok(Some(context_created(&key, repo, id))));
            }
            // Its parent, the work context, was closed as gone meanwhile:
            // once more, outside it.
            if attempts == 1 && crate::work_context::label_for(path) != parent {
                continue;
            }
            return Ok(Err(created));
        }
    }

    /// Writes the call to the repository's slow-operation log if it was slow.
    /// Everything expensive here — resolving the repository's directory,
    /// touching the disk — happens *after* the threshold test, so a call that
    /// was quick (all of them, normally) costs one comparison.
    async fn observe(
        &self,
        method: &str,
        path: &str,
        elapsed: std::time::Duration,
        op_id: &str,
        client_context: Option<&str>,
    ) {
        let ms = elapsed.as_millis() as u64;
        if !self.slow.worth_recording(ms) {
            return;
        }
        let Some(repo) = crate::slow::repo_of(path) else { return };
        let Some(dir) = self.internal_dir(&repo).await else { return };
        let op = format!("{} {}", method.to_uppercase(), crate::slow::route_shape(path));
        self.slow.record(&repo, &dir, op, ms, op_id, client_context);
    }

    /// A repository's `internal_dir`, cached. The lookup is itself untimed:
    /// timing it would log the call made to log a call.
    async fn internal_dir(&self, repo_uuid: &str) -> Option<std::path::PathBuf> {
        if let Some(dir) = self.internal_dirs.lock_recover().get(repo_uuid) {
            return Some(dir.clone());
        }
        let response = self.request_inner("GET", &format!("/repos/{repo_uuid}"), None, None).await;
        let dir = crate::slow::internal_dir_of(&response.ok()?.body)?;
        self.internal_dirs.lock_recover().insert(repo_uuid.to_string(), dir.clone());
        Some(dir)
    }

    async fn send(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        token: Option<String>,
        timing: Option<(&str, Option<&str>)>,
        log_context: Option<i64>,
    ) -> Result<ProxyResponse, String> {
        let url = format!("{}{}", self.base_url(), path);
        let method: reqwest::Method =
            method.parse().map_err(|_| format!("invalid HTTP method: {method}"))?;

        let mut request = self.client.request(method, &url);
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        if let Some(label) = log_context {
            request =
                request.header(metafolder_core::context::LOG_CONTEXT_HEADER, label.to_string());
        }
        if NOT_ACTIVITY.try_with(|v| *v).unwrap_or(false) {
            request = request.header(metafolder_core::context::ACTIVITY_HEADER, "off");
        }
        if let Some((op_id, client_context)) = timing {
            request = request.header(crate::slow::OP_ID_HEADER, op_id);
            if let Some(context) = client_context {
                request = request.header(crate::slow::CONTEXT_HEADER, context);
            }
        }
        if let Some(body) = body {
            request = request.json(&body);
        }
        let response = request
            .send()
            .await
            .map_err(|e| format!("daemon unreachable at {}: {e}", self.base_url()))?;

        let status = response.status().as_u16();
        let bytes = response
            .bytes()
            .await
            .map_err(|e| format!("error reading the daemon response: {e}"))?;
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        Ok(ProxyResponse { status, body })
    }

    /// Last health-check reachability; `None` before the first check.
    pub fn last_connected(&self) -> Option<bool> {
        self.health.lock_recover().map(|h| h.reachable)
    }

    /// The repository's human name (`GET /repos/:uuid` → `name`), best-effort:
    /// `None` if the daemon is unreachable or the repo is unknown. Used to
    /// auto-name a workspace after the repo it loads (doc "Workspaces"); the
    /// caller falls back to the plain "Workspace N" numbering.
    pub async fn repo_name(&self, uuid: &str) -> Option<String> {
        let response = self.request("GET", &format!("/repos/{uuid}"), None).await.ok()?;
        if response.status != 200 {
            return None;
        }
        response.body.get("name").and_then(Value::as_str).map(str::to_string)
    }

    /// Drains the daemon's diagnostics feed into the workspace message logs,
    /// routed by repository (doc "Connection to the daemon"). The daemon runs
    /// as its own process, so its stderr is invisible here; this is the only
    /// way its warnings reach the person using the GUI.
    ///
    /// Best effort: an unreachable or unexpected answer leaves the cursor where
    /// it was, so nothing is skipped and the next poll retries the same spot.
    pub async fn drain_diagnostics(&self, gui: &GuiState) {
        let (run, mut since) = self.diagnostics_since.lock_recover().clone();
        let mut body = None;
        // At most twice: once more from 0 when the page is another run's.
        for _ in 0..2 {
            let Ok(ProxyResponse { status: 200, body: page }) =
                self.request("GET", &format!("/diagnostics?since={since}"), None).await
            else {
                return;
            };
            let page_run = page["run"].as_str().map(str::to_string);
            let restarted = since > 0 && run.is_some() && page_run.is_some() && page_run != run;
            body = Some((page_run, page));
            if !restarted {
                break;
            }
            since = 0;
        }
        let Some((run, body)) = body else { return };
        let (lines, next) = crate::diagnostics::lines_from_page(&body, since);
        *self.diagnostics_since.lock_recover() = (run, next);
        if lines.is_empty() {
            return;
        }
        // Routed by repository: a daemon-wide line (no `repo`) goes to every
        // workspace, a line about a repository only to the workspaces on it —
        // and nowhere when no workspace is on it (`diagnostics::recipients`).
        let workspaces: Vec<(String, Option<String>)> =
            gui.workspaces().into_iter().map(|w| (w.id, w.active_repo)).collect();
        for line in &lines {
            for id in crate::diagnostics::recipients(line, &workspaces) {
                let _ = gui.append_message(id, &line.text);
            }
        }
    }

    /// One health probe; emits `daemon-health-changed` when the state differs
    /// from the last known one. Returns whether the daemon is reachable.
    ///
    /// A reachable daemon whose `/health` reports an `api_version` other than
    /// our [`metafolder_core::API_VERSION`] (or none at all — a daemon predating
    /// the field) is flagged `compatible: false`: the shell shows a distinct
    /// "incompatible daemon" banner rather than silently serving requests the
    /// two sides may disagree about.
    pub async fn check_health(&self, gui: &GuiState) -> bool {
        let outcome = match self.request("GET", "/health", None).await {
            Ok(ProxyResponse { status: 200, body }) => {
                let daemon_api = body.get("api_version").and_then(Value::as_u64).map(|v| v as u32);
                HealthOutcome {
                    reachable: true,
                    compatible: daemon_api == Some(metafolder_core::API_VERSION),
                    daemon_api,
                }
            }
            _ => HealthOutcome { reachable: false, compatible: false, daemon_api: None },
        };
        let mut health = self.health.lock_recover();
        if *health != Some(outcome) {
            *health = Some(outcome);
            gui.notify(events::DAEMON_HEALTH_CHANGED, outcome.payload());
        }
        outcome.reachable
    }

    /// The `daemon-health-changed` payload of the last probe; `None` before
    /// the first. What the shell starts from: the first probe is announced
    /// before the WebView listens, and a later one only announces a change.
    pub fn health_payload(&self) -> Option<Value> {
        self.health.lock_recover().map(|outcome| outcome.payload())
    }
}

/// Rejects forwarded paths that could alter the request's host. The URL is
/// built as `base_url + path`; a path must begin with `/` so the base's
/// authority is terminated before `path`. A path like `@evil.com` (or anything
/// not starting with `/`) would extend the authority into `userinfo@host` and
/// reparse to another host — an SSRF. A leading `//` is safe: the first `/`
/// after the existing authority still terminates it.
fn validate_path(path: &str) -> Result<(), String> {
    if path.starts_with('/') {
        Ok(())
    } else {
        Err(format!("invalid daemon path (must start with '/'): {path}"))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn an_error_message_is_the_body_s_or_the_fallback() {
        let fallback = || "HTTP 500".to_string();
        assert_eq!(
            super::error_message(&json!({"error": "no such repo"}), fallback),
            "no such repo"
        );
        assert_eq!(super::error_message(&json!({}), fallback), "HTTP 500");
        assert_eq!(super::error_message(&Value::Null, fallback), "HTTP 500");
    }

    use super::*;

    #[test]
    fn validate_path_accepts_normal_paths() {
        assert!(validate_path("/health").is_ok());
        assert!(validate_path("/repos/abc/query").is_ok());
        // A leading `//` stays on the same host (the authority is already set).
        assert!(validate_path("//evil.com/x").is_ok());
    }

    #[test]
    fn validate_path_rejects_authority_injection() {
        assert!(validate_path("@evil.com/x").is_err());
        assert!(validate_path("evil.com").is_err());
        assert!(validate_path("").is_err());
    }

    #[tokio::test]
    async fn request_rejects_host_injecting_path() {
        let proxy = DaemonProxy::new("http://127.0.0.1:7523".into());
        let err = proxy.request("GET", "@evil.com/steal", None).await.unwrap_err();
        assert!(err.contains("must start with '/'"), "got: {err}");
    }
}
