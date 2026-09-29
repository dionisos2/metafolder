//! The blocking daemon client core's orchestration runs on (trash re-link,
//! ignore presets, repository init, ordering, cross-repo sync — all over
//! [`metafolder_core::daemon_client::DaemonClient`]). That orchestration is
//! synchronous, so it rides a blocking `ureq` client on a `spawn_blocking`
//! thread rather than the async [`crate::daemon_proxy::DaemonProxy`]. Mirrors
//! the CLI's client: auth token, `{"error": …}` bodies → the message with the
//! HTTP status, transport failures too.

use metafolder_core::daemon_client::{DaemonClient, DaemonError};
use serde_json::Value;

pub(crate) struct BlockingClient {
    base: String,
    token: Option<String>,
    agent: ureq::Agent,
}

impl BlockingClient {
    pub(crate) fn new(base: String) -> Self {
        Self {
            base: base.trim_end_matches('/').to_string(),
            token: metafolder_core::auth::read_token("daemon").ok(),
            agent: ureq::Agent::new(),
        }
    }
}

impl DaemonClient for BlockingClient {
    fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, DaemonError> {
        let url = format!("{}{}", self.base, path);
        let mut req = self.agent.request(method, &url);
        if let Some(token) = &self.token {
            req = req.set("Authorization", &format!("Bearer {token}"));
        }
        let result = match body {
            Some(json) => req.send_json(json),
            None => req.call(),
        };
        match result {
            Ok(response) => Ok(response.into_json().unwrap_or(Value::Null)),
            Err(ureq::Error::Status(code, response)) => {
                let body: Value = response.into_json().unwrap_or(Value::Null);
                let message = crate::daemon_proxy::error_message(&body, || {
                    format!("daemon returned HTTP {code}")
                });
                Err(DaemonError { status: Some(code), message })
            }
            Err(ureq::Error::Transport(t)) => Err(DaemonError {
                status: None,
                message: format!("cannot reach the daemon at {}: {t}", self.base),
            }),
        }
    }
}
