//! Thin HTTP client over the daemon API, plus the CLI error type carrying
//! the spec exit codes (1 = operation failed, 2 = usage error).

use serde_json::Value as Json;

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
            agent: ureq::Agent::new(),
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
        let mut req = self.agent.request(method, &url);
        if let Some(token) = &self.token {
            req = req.set("Authorization", &format!("Bearer {token}"));
        }
        if let Some(command) = &self.command {
            use metafolder_core::context;
            req = req
                .set(context::COMMAND_HEADER, &command.key)
                .set(context::COMMAND_NAME_HEADER, &context::encode(&command.name));
            if let Some(description) = &command.description {
                req = req.set(context::COMMAND_DESCRIPTION_HEADER, &context::encode(description));
            }
            if let Some(label) = command.context.and_then(|c| c.label_for(path)) {
                req = req.set(context::LABEL_HEADER, &label.to_string());
            }
        }
        for (key, value) in query {
            req = req.query(key, value);
        }
        let result = match body {
            Some(json) => req.send_json(json),
            None => req.call(),
        };
        match result {
            Ok(response) => Ok(response.into_json().unwrap_or(Json::Null)),
            Err(ureq::Error::Status(code, response)) => {
                let body: Json = response.into_json().unwrap_or(Json::Null);
                let message = body["error"]
                    .as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| format!("{} returned HTTP {code}", self.peer));
                Err((Some(code), message))
            }
            Err(ureq::Error::Transport(t)) => {
                Err((None, format!("cannot reach the {} at {}: {t}", self.peer, self.base)))
            }
        }
    }

    pub fn get(&self, path: &str, query: &[(&str, String)]) -> Result<Json, CliError> {
        self.request("GET", path, query, None)
    }

    pub fn post(&self, path: &str, body: &Json) -> Result<Json, CliError> {
        self.request("POST", path, &[], Some(body))
    }
}
