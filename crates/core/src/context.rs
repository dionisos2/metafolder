//! Write contexts on the wire (doc "Log contexts"): the headers a
//! client names its context and its command with, the environment variable a
//! context is passed on in, and the encoding of a name in a header.
//!
//! Shared by the daemon (which reads them), the CLI and the GUI (which send
//! them), so the three cannot spell them differently.

/// The log context a request writes in: a context id, `0` or absent for
/// none — the daemon then opens one of its own for the write. A client
/// creates its context before it writes (`POST /repos/:repo/contexts`) and
/// passes it on; leaving it is ceasing to send it.
pub const LOG_CONTEXT_HEADER: &str = "x-metafolder-log-context";

/// Whether a request counts as the user's activity, which holds the daemon's
/// folding of pending lists back (doc "Folding in idle time"): every request
/// to a repository does, unless it says `off`. A client that polls says it.
pub const ACTIVITY_HEADER: &str = "x-metafolder-activity";

/// The environment variable that makes an `mf` say its requests are not
/// activity (`METAFOLDER_ACTIVITY=off`): a script that polls sets it.
pub const ACTIVITY_ENV: &str = "METAFOLDER_ACTIVITY";

/// The environment variable a context is passed on in ([`Context`]): the
/// log context every `mf` started meanwhile writes in. Leaving the context is not
/// passing it on.
pub const LOG_CONTEXT_ENV: &str = "METAFOLDER_LOG_CONTEXT";

/// A context as it is passed on: a log context, and the repository it belongs to —
/// a log context id means nothing in another repository, so a client sends it only
/// with a request to that one. Spelt `<repo uuid>:<log_context id>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Context {
    pub repo: uuid::Uuid,
    pub log_context: i64,
}

impl Context {
    /// Reads `<repo uuid>:<log_context id>`; `None` when it is not that.
    pub fn parse(text: &str) -> Option<Self> {
        let (repo, log_context) = text.trim().split_once(':')?;
        Some(Self {
            repo: uuid::Uuid::parse_str(repo).ok()?,
            log_context: log_context.parse().ok()?,
        })
    }

    /// The log context to send with a request to `path`: this one when the path is
    /// one of its repository's (`/repos/<uuid>/…`).
    pub fn log_context_for(&self, path: &str) -> Option<i64> {
        (repo_of(path)? == self.repo).then_some(self.log_context)
    }
}

/// The repository a request is for: `<uuid>` in `/repos/<uuid>/…`.
pub fn repo_of(path: &str) -> Option<uuid::Uuid> {
    let rest = path.strip_prefix("/repos/")?;
    uuid::Uuid::parse_str(rest.split(['/', '?']).next()?).ok()
}

impl std::fmt::Display for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.repo.as_simple(), self.log_context)
    }
}

/// Percent-encodes `text` for a header value: every byte outside printable
/// ASCII, and `%`, becomes `%XX` — a header carries no UTF-8.
pub fn encode(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for b in text.bytes() {
        if (0x20..0x7f).contains(&b) && b != b'%' {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The text [`encode`] encoded; a malformed escape is kept as written, and
/// bytes that are not UTF-8 are replaced.
pub fn decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = |b: u8| (b as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hex(bytes[i + 1]), hex(bytes[i + 2])) {
                out.push((hi * 16 + lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Whether a request may write to the log, so that a command creates its
/// context before sending it — and a command that only reads creates none.
/// A `GET` never writes; neither do the `POST`s that only read (a query, a
/// path to resolve, a check). Every other request may.
pub fn may_write(method: &str, path: &str) -> bool {
    if method.eq_ignore_ascii_case("GET") {
        return false;
    }
    let path = path.split('?').next().unwrap_or(path);
    let Some(rest) = path.strip_prefix("/repos/") else { return true };
    let route = rest.split_once('/').map_or("", |(_, r)| r);
    !READ_ONLY_POSTS.contains(&route)
}

/// The `POST`s under `/repos/:repo/` that write nothing to the log.
const READ_ONLY_POSTS: &[&str] = &[
    "query",
    "query/profile",
    "query/fields/resolve-tree",
    "tree/resolve-path",
    "tree/resolve-paths",
    "schema/check",
    "activity",
    "eligibility",
    "watch/activity",
    "watch/check",
    "contexts",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_survives_the_header_whole() {
        for text in ["mf tag add été", "100% done", "a\nb", "plain", "", "日本"] {
            let encoded = encode(text);
            assert!(encoded.bytes().all(|b| (0x20..0x7f).contains(&b)), "{encoded}");
            assert_eq!(decode(&encoded), text);
        }
    }

    #[test]
    fn a_context_names_its_repository() {
        let repo = uuid::Uuid::new_v4();
        let context = Context { repo, log_context: 12 };
        assert_eq!(Context::parse(&context.to_string()), Some(context));
        assert_eq!(Context::parse(&format!("{}:12", repo.hyphenated())), Some(context));
        assert_eq!(Context::parse("12"), None);
        assert_eq!(Context::parse("x:12"), None);
        assert_eq!(
            context.log_context_for(&format!("/repos/{}/metarecords", repo.as_simple())),
            Some(12)
        );
        assert_eq!(context.log_context_for(&format!("/repos/{}", repo.hyphenated())), Some(12));
        assert_eq!(context.log_context_for(&format!("/repos/{}/log", uuid::Uuid::new_v4())), None);
        assert_eq!(context.log_context_for("/repos"), None);
    }

    #[test]
    fn only_a_request_that_may_write_needs_a_context() {
        let repo = "/repos/0123456789abcdef0123456789abcdef";
        assert!(!may_write("GET", &format!("{repo}/log")));
        assert!(!may_write("POST", &format!("{repo}/query")));
        assert!(!may_write("POST", &format!("{repo}/query?limit=3")));
        assert!(!may_write("POST", &format!("{repo}/tree/resolve-paths")));
        assert!(!may_write("POST", &format!("{repo}/contexts")));
        assert!(may_write("POST", &format!("{repo}/metarecords")));
        assert!(may_write("POST", &format!("{repo}/query/fields/set")));
        assert!(may_write("DELETE", &format!("{repo}/metarecords/x")));
        assert!(may_write("PATCH", &format!("{repo}/metarecords/x/fields/a")));
        assert!(may_write("POST", "/sync/a/b/links/commit"));
    }

    #[test]
    fn a_malformed_escape_is_kept() {
        assert_eq!(decode("50%"), "50%");
        assert_eq!(decode("%zz"), "%zz");
        assert_eq!(decode("%4"), "%4");
    }
}
