//! HTTP proxy to the metafolder daemon (spec-gui "Connection to the
//! daemon"). Panels and the shell go through this backend client: the
//! WebView cannot call the daemon directly (no CORS there, and the
//! daemon must stay GUI-agnostic). Tracks reachability and emits
//! `daemon-health-changed` on transitions.

use metafolder_core::sync::MutexExt;
use crate::events;
use crate::state::GuiState;
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::Mutex;
use std::time::Duration;

#[derive(Serialize, Debug, PartialEq)]
pub struct ProxyResponse {
    pub status: u16,
    pub body: Value,
}

pub struct DaemonProxy {
    client: reqwest::Client,
    base_url: Mutex<String>,
    /// Last known reachability; `None` until the first check.
    connected: Mutex<Option<bool>>,
    /// Cached daemon session token (spec-auth), read lazily from the token
    /// file. Stable across daemon restarts, so caching is safe; cleared and
    /// re-read once on a 401 (covers the daemon having regenerated it).
    token: Mutex<Option<String>>,
}

impl DaemonProxy {
    pub fn new(base_url: String) -> Self {
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
            connected: Mutex::new(None),
            token: Mutex::new(None),
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
        validate_path(path)?;
        let response = self.send(method, path, body.clone(), self.token()).await?;
        // A 401 means our cached token is stale (the daemon regenerated it).
        // Drop it, re-read the file once and retry.
        if response.status == 401 {
            self.invalidate_token();
            if let Some(token) = self.token() {
                return self.send(method, path, body, Some(token)).await;
            }
        }
        Ok(response)
    }

    async fn send(
        &self,
        method: &str,
        path: &str,
        body: Option<Value>,
        token: Option<String>,
    ) -> Result<ProxyResponse, String> {
        let url = format!("{}{}", self.base_url(), path);
        let method: reqwest::Method = method
            .parse()
            .map_err(|_| format!("invalid HTTP method: {method}"))?;

        let mut request = self.client.request(method, &url);
        if let Some(token) = token {
            request = request.bearer_auth(token);
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

    /// Last health-check outcome; `None` before the first check.
    pub fn last_connected(&self) -> Option<bool> {
        *self.connected.lock_recover()
    }

    /// One health probe; emits `daemon-health-changed` when the state
    /// differs from the last known one. Returns the current state.
    pub async fn check_health(&self, gui: &GuiState) -> bool {
        let healthy = matches!(
            self.request("GET", "/health", None).await,
            Ok(ProxyResponse { status: 200, .. })
        );
        let mut connected = self.connected.lock_recover();
        if *connected != Some(healthy) {
            *connected = Some(healthy);
            gui.notify(events::DAEMON_HEALTH_CHANGED, json!({ "connected": healthy }));
        }
        healthy
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
