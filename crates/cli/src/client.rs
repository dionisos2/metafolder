//! Thin HTTP client over the daemon API, plus the CLI error type carrying
//! the spec exit codes (1 = operation failed, 2 = usage error).

use serde_json::Value as Json;

/// The HTTP agent every client of this crate uses. Its peers (the daemon, the
/// GUI) are on the loopback: no proxy from the environment, which ureq would
/// otherwise read; and an error status is an answer to read, not a failure.
pub(crate) fn agent() -> ureq::Agent {
    ureq::Agent::new_with_config(
        ureq::Agent::config_builder().proxy(None).http_status_as_error(false).build(),
    )
}

#[derive(Debug)]
pub enum CliError {
    /// Bad arguments, unparsable DSL/field spec, missing `--repo` (exit 2).
    Usage(String),
    /// Daemon error, transport failure, no match, ... (exit 1).
    Op(String),
}

impl CliError {
    pub fn message(&self) -> &str {
        match self {
            CliError::Usage(msg) | CliError::Op(msg) => msg,
        }
    }

    pub fn exit_code(&self) -> i32 {
        match self {
            CliError::Usage(_) => 2,
            CliError::Op(_) => 1,
        }
    }
}

pub struct Client {
    base: String,
    /// Peer name used in transport error messages ("daemon" or "GUI").
    peer: &'static str,
    agent: ureq::Agent,
    /// Session token (doc "Session tokens"), read best-effort from the peer's runtime
    /// token file. `None` when the file is missing — typically because the
    /// peer is not running, in which case the request fails at transport.
    token: Option<String>,
    /// The command this process runs, told to the daemon with every request
    /// (doc "Labels and contexts"); `None` for a client that is no command.
    command: Option<CommandHeaders>,
}

/// What a command tells the daemon about itself: the key its label is found
/// by, the name and description it is opened with at the first write, and the
/// context it runs in.
#[derive(Debug, Clone)]
pub struct CommandHeaders {
    pub key: String,
    pub name: String,
    pub description: Option<String>,
    pub context: Option<metafolder_core::context::Context>,
}

/// The client every piece of core orchestration drives (trash, ignore, order,
/// repository init, sync): `request_daemon`, which keeps the HTTP status.
impl metafolder_core::daemon_client::DaemonClient for Client {
    fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&Json>,
    ) -> Result<Json, metafolder_core::daemon_client::DaemonError> {
        self.request_daemon(method, path, body)
    }
}

impl Client {
    pub fn new(base_url: &str) -> Self {
        Self::with_peer(base_url, "daemon")
    }

    pub fn with_peer(base_url: &str, peer: &'static str) -> Self {
        // "daemon" -> "daemon", "GUI" -> "gui": the doc "Session tokens" service name.
        let service = peer.to_ascii_lowercase();
        Self {
            base: base_url.trim_end_matches('/').to_string(),
            peer,
            agent: agent(),
            token: metafolder_core::auth::read_token(&service).ok(),
            command: None,
        }
    }

    /// The same client, telling the daemon its requests are steps of
    /// `command`.
    pub fn in_command(mut self, command: CommandHeaders) -> Self {
        self.command = Some(command);
        self
    }

    /// The context the command runs in, if any.
    pub fn context(&self) -> Option<metafolder_core::context::Context> {
        self.command.as_ref().and_then(|c| c.context)
    }

    /// Sends a request and returns the parsed JSON body (Null when empty,
    /// e.g. 204 responses). Daemon `{"error": ...}` bodies become
    /// `CliError::Op` with the daemon's message.
    pub fn request(
        &self,
        method: &str,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Json>,
    ) -> Result<Json, CliError> {
        self.send(method, path, query, body).map_err(|(_, message)| CliError::Op(message))
    }

    /// Like [`Self::request`] but preserving the HTTP status, for callers that
    /// classify an error by status rather than message (core's `DaemonClient`).
    pub fn request_daemon(
        &self,
        method: &str,
        path: &str,
        body: Option<&Json>,
    ) -> Result<Json, metafolder_core::daemon_client::DaemonError> {
        self.send(method, path, &[], body).map_err(|(status, message)| {
            metafolder_core::daemon_client::DaemonError { status, message }
        })
    }

    /// The raw request: `Ok(body)` on 2xx, else `Err((status, message))` — the
    /// HTTP status (None on transport failure) and the daemon's error text.
    fn send(
        &self,
        method: &str,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Json>,
    ) -> Result<Json, (Option<u16>, String)> {
        let url = format!("{}{}", self.base, path);
        let mut req = match method {
            "GET" => self.agent.get(&url).force_send_body(),
            "DELETE" => self.agent.delete(&url).force_send_body(),
            "POST" => self.agent.post(&url),
            "PUT" => self.agent.put(&url),
            "PATCH" => self.agent.patch(&url),
            other => return Err((None, format!("unsupported HTTP method {other}"))),
        };
        if let Some(token) = &self.token {
            req = req.header("Authorization", format!("Bearer {token}"));
        }
        if let Some(command) = &self.command {
            use metafolder_core::context;
            req = req
                .header(context::COMMAND_HEADER, &command.key)
                .header(context::COMMAND_NAME_HEADER, context::encode(&command.name));
            if let Some(description) = &command.description {
                req = req.header(context::COMMAND_DESCRIPTION_HEADER, context::encode(description));
            }
            if let Some(label) = command.context.and_then(|c| c.label_for(path)) {
                req = req.header(context::LABEL_HEADER, label.to_string());
            }
        }
        for (key, value) in query {
            req = req.query(key, value);
        }
        let result = match body {
            Some(json) => req.send_json(json),
            None => req.send_empty(),
        };
        let mut response = match result {
            Ok(response) => response,
            Err(e) => {
                return Err((None, format!("cannot reach the {} at {}: {e}", self.peer, self.base)))
            }
        };
        let code = response.status().as_u16();
        // Whole: ureq stops at 10 MB by default, and a page or a log is larger.
        let body: Json =
            response.body_mut().with_config().limit(u64::MAX).read_json().unwrap_or(Json::Null);
        if response.status().is_success() {
            return Ok(body);
        }
        let message = body["error"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| format!("{} returned HTTP {code}", self.peer));
        Err((Some(code), message))
    }

    pub fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Json, CliError> {
        self.request("GET", path, query, None)
    }

    pub fn post(&self, path: &str, body: &Json) -> Result<Json, CliError> {
        self.request("POST", path, &[], Some(body))
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

    /// The daemon is on the loopback: a proxy from the environment (which
    /// the HTTP client would otherwise read) has no business in between.
    #[test]
    fn test_the_agent_takes_no_proxy() {
        assert!(agent().config().proxy().is_none());
    }

    /// A large answer (a long page, a whole log) is read whole, not cut at
    /// the HTTP client's default limit of 10 MB.
    #[test]
    fn test_a_large_answer_is_read_whole() {
        let big = "x".repeat(11 * 1024 * 1024);
        let url = serve_once("200 OK", serde_json::to_string(&json_of(&big)).unwrap());
        let answer = Client::new(&url).get("/big", &[]).unwrap();
        assert_eq!(answer["v"].as_str().map(str::len), Some(big.len()));
    }

    /// An error status keeps its status and the daemon's message.
    #[test]
    fn test_an_error_answer_keeps_its_status_and_message() {
        let url = serve_once("409 Conflict", r#"{"error":"busy"}"#.into());
        let err = Client::new(&url).request_daemon("GET", "/x", None).unwrap_err();
        assert_eq!((err.status, err.message.as_str()), (Some(409), "busy"));
    }

    fn json_of(v: &str) -> Json {
        serde_json::json!({ "v": v })
    }
}
