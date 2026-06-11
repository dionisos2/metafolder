//! HTTP proxy to the metafolder daemon (spec-gui "Connection to the
//! daemon"). Panels and the shell go through this backend client: the
//! WebView cannot call the daemon directly (no CORS there, and the
//! daemon must stay GUI-agnostic). Tracks reachability and emits
//! `daemon-health-changed` on transitions.

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
}

impl DaemonProxy {
    pub fn new(base_url: String) -> Self {
        DaemonProxy {
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(2))
                .build()
                .expect("reqwest client"),
            base_url: Mutex::new(base_url),
            connected: Mutex::new(None),
        }
    }

    pub fn base_url(&self) -> String {
        self.base_url.lock().unwrap().clone()
    }

    pub fn set_url(&self, url: String) {
        *self.base_url.lock().unwrap() = url;
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
        let url = format!("{}{}", self.base_url(), path);
        let method: reqwest::Method = method
            .parse()
            .map_err(|_| format!("invalid HTTP method: {method}"))?;

        let mut request = self.client.request(method, &url);
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

    /// One health probe; emits `daemon-health-changed` when the state
    /// differs from the last known one. Returns the current state.
    pub async fn check_health(&self, gui: &GuiState) -> bool {
        let healthy = matches!(
            self.request("GET", "/health", None).await,
            Ok(ProxyResponse { status: 200, .. })
        );
        let mut connected = self.connected.lock().unwrap();
        if *connected != Some(healthy) {
            *connected = Some(healthy);
            gui.notify(events::DAEMON_HEALTH_CHANGED, json!({ "connected": healthy }));
        }
        healthy
    }
}
