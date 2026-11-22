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
    /// The command this process runs, whose writes are in a log context of
    /// its own (doc "Log contexts"); `None` for a client that is no command.
    command: Option<Command>,
    /// Whether the requests say they are not the user's activity (doc
    /// "Folding in idle time").
    not_activity: bool,
}

/// A command's log context (doc "Log contexts"): the name and description
/// it is created with, the context it runs in (`METAFOLDER_LOG_CONTEXT`), and the
/// context it created in each repository — at its first request there that
/// may write, so that a command that only reads creates none.
#[derive(Debug)]
pub struct Command {
    name: String,
    description: Option<String>,
    context: Option<metafolder_core::context::Context>,
    created: std::sync::Mutex<std::collections::HashMap<uuid::Uuid, i64>>,
}

impl Command {
    pub fn new(
        name: String,
        description: Option<String>,
        context: Option<metafolder_core::context::Context>,
    ) -> Self {
        Self { name, description, context, created: Default::default() }
    }
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
            not_activity: false,
        }
    }

    /// The same client, saying its requests are not the user's activity: the
    /// daemon does not hold its folding back for them (doc "Folding in idle
    /// time").
    pub fn not_activity(mut self) -> Self {
        self.not_activity = true;
        self
    }

    /// The same client, writing in `command`'s context.
    pub fn in_command(mut self, command: Command) -> Self {
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
        if self.not_activity {
            req = req.header(metafolder_core::context::ACTIVITY_HEADER, "off");
        }
        if let Some(log_context) = self.log_context(method, path)? {
            req = req.header(metafolder_core::context::LOG_CONTEXT_HEADER, log_context.to_string());
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
        let message =
            body["error"].as_str().map(str::to_string).unwrap_or_else(|| self.unexplained(code));
        Err((Some(code), message))
    }

    /// The log context to send with a request: the command's own in the
    /// repository the request is for, created first when the request may
    /// write; else the context the command runs in, when it is that
    /// repository's.
    fn log_context(&self, method: &str, path: &str) -> Result<Option<i64>, (Option<u16>, String)> {
        use metafolder_core::context;
        let Some(command) = &self.command else { return Ok(None) };
        let Some(repo) = context::repo_of(path) else { return Ok(None) };
        let outer = command.context.filter(|c| c.repo == repo).map(|c| c.log_context);
        if let Some(&own) = command.created.lock().unwrap_or_else(|e| e.into_inner()).get(&repo) {
            return Ok(Some(own));
        }
        if !context::may_write(method, path) {
            return Ok(outer);
        }
        let body = serde_json::json!({
            "name": command.name,
            "description": command.description,
            "parent": outer,
        });
        let created =
            self.send("POST", &format!("/repos/{}/contexts", repo.as_simple()), &[], Some(&body))?;
        let id =
            created["id"].as_i64().ok_or((None, "the daemon named no log context".to_string()))?;
        command.created.lock().unwrap_or_else(|e| e.into_inner()).insert(repo, id);
        Ok(Some(id))
    }

    /// The message of an error status that came without a reason of the
    /// peer's own (`{"error": …}`): typically a route it does not have.
    pub fn unexplained(&self, code: u16) -> String {
        format!("{} returned HTTP {code}", self.peer)
    }

    pub fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Json, CliError> {
        self.request("GET", path, query, None)
    }

    pub fn post(&self, path: &str, body: &Json) -> Result<Json, CliError> {
        self.request("POST", path, &[], Some(body))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::io::{Read, Write};

    /// Answers one request on a loopback port with `status` and `body`.
    pub(crate) fn serve_once(status: &str, body: String) -> String {
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

    /// Answers one request and sends what it said back, for a test to look at.
    fn serve_once_capturing() -> (String, std::sync::mpsc::Receiver<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0u8; 4096];
            let n = stream.read(&mut request).unwrap_or(0);
            let _ = tx.send(String::from_utf8_lossy(&request[..n]).to_ascii_lowercase());
            let body = "{}";
            let head = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(head.as_bytes());
        });
        (url, rx)
    }

    /// A client that says its requests are not activity sends the header; the
    /// others say nothing, and so count.
    #[test]
    fn test_a_client_can_say_its_requests_are_not_activity() {
        let (url, seen) = serve_once_capturing();
        Client::new(&url).not_activity().get("/x", &[]).unwrap();
        assert!(seen.recv().unwrap().contains("x-metafolder-activity: off"));
        let (url, seen) = serve_once_capturing();
        Client::new(&url).get("/x", &[]).unwrap();
        assert!(!seen.recv().unwrap().contains("x-metafolder-activity"));
    }

    /// Answers `answers.len()` requests in turn, each with its answer, and
    /// sends each request back whole (head and body), lowercased.
    fn serve_capturing(answers: Vec<&'static str>) -> (String, std::sync::mpsc::Receiver<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for body in answers {
                let (mut stream, _) = listener.accept().unwrap();
                let mut seen = Vec::new();
                let mut chunk = [0u8; 4096];
                loop {
                    let n = stream.read(&mut chunk).unwrap_or(0);
                    seen.extend_from_slice(&chunk[..n]);
                    let text = String::from_utf8_lossy(&seen).to_ascii_lowercase();
                    let Some(end) = text.find("\r\n\r\n") else {
                        if n == 0 {
                            break;
                        }
                        continue;
                    };
                    let length = text
                        .lines()
                        .find_map(|l| l.strip_prefix("content-length: "))
                        .and_then(|v| v.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if n == 0 || seen.len() >= end + 4 + length {
                        break;
                    }
                }
                let _ = tx.send(String::from_utf8_lossy(&seen).to_ascii_lowercase());
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = stream.write_all(head.as_bytes());
            }
        });
        (url, rx)
    }

    /// A command creates its context at its first request that may write —
    /// inside the context it runs in — and sends it with its later requests
    /// to that repository; what it only reads creates nothing (doc "Log
    /// contexts").
    #[test]
    fn test_a_command_creates_its_context_at_its_first_write() {
        use metafolder_core::context::Context;
        let repo = uuid::Uuid::from_u128(0xabc);
        let base = format!("/repos/{}", repo.as_simple());
        let (url, seen) = serve_capturing(vec!["{}", r#"{"id": 7}"#, "{}", "{}"]);
        let command = Command::new(
            "mf metarecord add".into(),
            Some("query: été".into()),
            Some(Context { repo, log_context: 3 }),
        );
        let client = Client::new(&url).in_command(command);
        client.post(&format!("{base}/query"), &serde_json::json!({})).unwrap();
        client.post(&format!("{base}/metarecords"), &serde_json::json!({})).unwrap();
        client.post(&format!("{base}/metarecords"), &serde_json::json!({})).unwrap();

        let read = seen.recv().unwrap();
        assert!(read.starts_with(&format!("post {base}/query ")), "{read}");
        assert!(read.contains("x-metafolder-log-context: 3"), "{read}");
        let created = seen.recv().unwrap();
        assert!(created.starts_with(&format!("post {base}/contexts ")), "{created}");
        let body: Json = serde_json::from_str(created.split("\r\n\r\n").nth(1).unwrap()).unwrap();
        assert_eq!(
            body,
            serde_json::json!({"name": "mf metarecord add", "description": "query: été", "parent": 3})
        );
        for _ in 0..2 {
            let write = seen.recv().unwrap();
            assert!(write.starts_with(&format!("post {base}/metarecords ")), "{write}");
            assert!(write.contains("x-metafolder-log-context: 7"), "{write}");
        }
    }

    fn json_of(v: &str) -> Json {
        serde_json::json!({ "v": v })
    }
}
