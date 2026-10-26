//! The blocking daemon client core's orchestration runs on (trash re-link,
//! ignore presets, repository init, ordering, cross-repo sync — all over
//! [`metafolder_core::daemon_client::DaemonClient`]). That orchestration is
//! synchronous, so it rides a blocking `ureq` client on a `spawn_blocking`
//! thread rather than the async [`crate::daemon_proxy::DaemonProxy`]. Mirrors
//! the CLI's client: auth token, `{"error": …}` bodies → the message with the
//! HTTP status, transport failures too.

use metafolder_core::daemon_client::{DaemonClient, DaemonError};
use serde_json::Value;

/// The daemon is on the loopback: no proxy from the environment, which ureq
/// would otherwise read; and an error status is an answer to read.
fn agent() -> ureq::Agent {
    ureq::Agent::new_with_config(
        ureq::Agent::config_builder().proxy(None).http_status_as_error(false).build(),
    )
}

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
            agent: agent(),
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
        let body_in = body;
        let url = format!("{}{}", self.base, path);
        let mut req = match method {
            "GET" => self.agent.get(&url).force_send_body(),
            "DELETE" => self.agent.delete(&url).force_send_body(),
            "POST" => self.agent.post(&url),
            "PUT" => self.agent.put(&url),
            "PATCH" => self.agent.patch(&url),
            other => {
                return Err(DaemonError {
                    status: None,
                    message: format!("unsupported HTTP method {other}"),
                })
            }
        };
        if let Some(token) = &self.token {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        if let Some(label) = crate::work_context::label_for(path) {
            req = req.header(metafolder_core::context::LABEL_HEADER, label.to_string());
        }
        if let Some((key, name)) = crate::daemon_proxy::current_command() {
            use metafolder_core::context;
            req = req
                .header(context::COMMAND_HEADER, &key)
                .header(context::COMMAND_NAME_HEADER, context::encode(&name));
        }
        let result = match body {
            Some(json) => req.send_json(json),
            None => req.send_empty(),
        };
        let mut response = result.map_err(|e| DaemonError {
            status: None,
            message: format!("cannot reach the daemon at {}: {e}", self.base),
        })?;
        let code = response.status().as_u16();
        // Whole: ureq stops at 10 MB by default, and a page or a log is larger.
        let body: Value =
            response.body_mut().with_config().limit(u64::MAX).read_json().unwrap_or(Value::Null);
        if response.status().is_success() {
            return Ok(body);
        }
        // The work context's label was pruned meanwhile: nothing was
        // written, and the request goes again outside the context.
        if crate::work_context::forget_if_gone(path, code, &body) {
            return self.request(method, path, body_in);
        }
        let message =
            crate::daemon_proxy::error_message(&body, || format!("daemon returned HTTP {code}"));
        Err(DaemonError { status: Some(code), message })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    /// Answers one request on a loopback port with `status` and `body`.
    fn serve_once(status: &str, body: String) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let status = status.to_string();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 4096];
            let _ = stream.read(&mut request);
            let head = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
            let _ = stream.write_all(body.as_bytes());
        });
        url
    }

    /// The daemon is on the loopback: no proxy from the environment.
    #[test]
    fn test_the_agent_takes_no_proxy() {
        assert!(agent().config().proxy().is_none());
    }

    /// A large answer is read whole, not cut at ureq's default 10 MB.
    #[test]
    fn test_a_large_answer_is_read_whole() {
        let big = "x".repeat(11 * 1024 * 1024);
        let url = serve_once("200 OK", serde_json::json!({ "v": big }).to_string());
        let answer = BlockingClient::new(url).request("GET", "/big", None).unwrap();
        assert_eq!(answer["v"].as_str().map(str::len), Some(big.len()));
    }

    /// An error status keeps its status and the daemon's message.
    #[test]
    fn test_an_error_answer_keeps_its_status_and_message() {
        let url = serve_once("409 Conflict", r#"{"error":"busy"}"#.into());
        let err = BlockingClient::new(url).request("GET", "/x", None).unwrap_err();
        assert_eq!((err.status, err.message.as_str()), (Some(409), "busy"));
    }
}
