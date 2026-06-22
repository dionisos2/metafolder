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
    /// Session token (spec-auth), read best-effort from the peer's runtime
    /// token file. `None` when the file is missing — typically because the
    /// peer is not running, in which case the request fails at transport.
    token: Option<String>,
}

impl Client {
    pub fn new(base_url: &str) -> Self {
        Self::with_peer(base_url, "daemon")
    }

    pub fn with_peer(base_url: &str, peer: &'static str) -> Self {
        // "daemon" -> "daemon", "GUI" -> "gui": the spec-auth service name.
        let service = peer.to_ascii_lowercase();
        Self {
            base: base_url.trim_end_matches('/').to_string(),
            peer,
            agent: ureq::Agent::new(),
            token: metafolder_core::auth::read_token(&service).ok(),
        }
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
        let url = format!("{}{}", self.base, path);
        let mut req = self.agent.request(method, &url);
        if let Some(token) = &self.token {
            req = req.set("Authorization", &format!("Bearer {token}"));
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
                Err(CliError::Op(message))
            }
            Err(ureq::Error::Transport(t)) => {
                Err(CliError::Op(format!("cannot reach the {} at {}: {t}", self.peer, self.base)))
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
