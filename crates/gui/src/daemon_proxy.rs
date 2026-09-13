//! HTTP proxy to the metafolder daemon (spec-gui "Connection to the
//! daemon"). Panels and the shell go through this backend client: the
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

/// The outcome of one `/health` probe (spec-gui "Connection to the daemon").
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

pub struct DaemonProxy {
    client: reqwest::Client,
    base_url: Mutex<String>,
    /// Last known health; `None` until the first check.
    health: Mutex<Option<HealthOutcome>>,
    /// Cached daemon session token (spec-auth), read lazily from the token
    /// file. Stable across daemon restarts, so caching is safe; cleared and
    /// re-read once on a 401 (covers the daemon having regenerated it).
    token: Mutex<Option<String>>,
    /// Cursor into the daemon's diagnostics feed: the last entry id drained
    /// into the message panels. Starts at 0 so the first poll picks up whatever
    /// the daemon already warned about — including why a repository failed to
    /// load, which happens before the GUI is up.
    diagnostics_since: Mutex<u64>,
    /// Timing of the calls made through this proxy (spec-gui "Slow daemon
    /// calls"): what the *user* waited for, next to what the daemon spent.
    slow: crate::slow::SlowLog,
    /// Repository uuid → its `internal_dir`, as the daemon reported it. Only
    /// filled when a call was slow, so an ordinary session never asks.
    internal_dirs: Mutex<std::collections::HashMap<String, std::path::PathBuf>>,
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
                .build()
                .expect("reqwest client"),
            base_url: Mutex::new(base_url),
            health: Mutex::new(None),
            token: Mutex::new(None),
            diagnostics_since: Mutex::new(0),
            slow: crate::slow::SlowLog::new(slow_threshold_ms),
            internal_dirs: Mutex::new(std::collections::HashMap::new()),
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
        self.request_with_context(method, path, body, None).await
    }

    /// The same, plus what the user asked for in their own words — the DSL text
    /// of a query, the command that ran. The daemon receives the query IR and
    /// cannot reconstruct it, so the client is the only side that can say it
    /// (spec-slow-log "Correlating the GUI and the daemon").
    pub async fn request_with_context(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        client_context: Option<&str>,
    ) -> Result<ProxyResponse, String> {
        let started = std::time::Instant::now();
        let op_id = crate::slow::new_op_id();
        let out = self.request_inner(method, path, body, Some((&op_id, client_context))).await;
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
        let response = self.send(method, path, body.clone(), self.token(), timing).await?;
        // A 401 means our cached token is stale (the daemon regenerated it).
        // Drop it, re-read the file once and retry.
        if response.status == 401 {
            self.invalidate_token();
            if let Some(token) = self.token() {
                return self.send(method, path, body, Some(token), timing).await;
            }
        }
        Ok(response)
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
    ) -> Result<ProxyResponse, String> {
        let url = format!("{}{}", self.base_url(), path);
        let method: reqwest::Method =
            method.parse().map_err(|_| format!("invalid HTTP method: {method}"))?;

        let mut request = self.client.request(method, &url);
        if let Some(token) = token {
            request = request.bearer_auth(token);
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
    /// auto-name a workspace after the repo it loads (spec-gui "Workspace
    /// name"); the caller falls back to the plain "Workspace N" numbering.
    pub async fn repo_name(&self, uuid: &str) -> Option<String> {
        let response = self.request("GET", &format!("/repos/{uuid}"), None).await.ok()?;
        if response.status != 200 {
            return None;
        }
        response.body.get("name").and_then(Value::as_str).map(str::to_string)
    }

    /// Drains the daemon's diagnostics feed into every workspace's message log
    /// (spec-gui "Daemon diagnostics"). The daemon runs as its own process, so
    /// its stderr is invisible here; this is the only way its warnings reach
    /// the person using the GUI.
    ///
    /// Best effort: an unreachable or unexpected answer leaves the cursor where
    /// it was, so nothing is skipped and the next poll retries the same spot.
    pub async fn drain_diagnostics(&self, gui: &GuiState) {
        let since = *self.diagnostics_since.lock_recover();
        let Ok(ProxyResponse { status: 200, body }) =
            self.request("GET", &format!("/diagnostics?since={since}"), None).await
        else {
            return;
        };
        let (lines, next) = crate::diagnostics::lines_from_page(&body, since);
        *self.diagnostics_since.lock_recover() = next;
        if lines.is_empty() {
            return;
        }
        // Routed by repository. A daemon-wide line (no `repo`) concerns every
        // workspace and goes to each message log. A line *about* a repository —
        // a flush, a watch it could not place — goes only to the workspaces on
        // that repository: with two repositories loaded, A's flushes used to
        // fill B's message panel, which is noise indistinguishable from B's own.
        let workspaces = gui.workspaces();
        for line in &lines {
            let mut shown = false;
            for workspace in &workspaces {
                if line.concerns(workspace.active_repo.as_deref()) {
                    let _ = gui.append_message(&workspace.id, &line.text);
                    shown = true;
                }
            }
            // Nowhere to route it: a repository loaded with no workspace on it.
            // Showing it everywhere is better than dropping it — an error about
            // a repository nobody is looking at is exactly the one worth seeing,
            // and the message names its repository.
            if !shown {
                for workspace in &workspaces {
                    let _ = gui.append_message(&workspace.id, &line.text);
                }
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
            gui.notify(
                events::DAEMON_HEALTH_CHANGED,
                json!({
                    "connected": outcome.reachable,
                    "compatible": outcome.compatible,
                    "daemon_api_version": outcome.daemon_api,
                    "gui_api_version": metafolder_core::API_VERSION,
                }),
            );
        }
        outcome.reachable
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
