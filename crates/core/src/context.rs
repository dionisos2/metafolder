//! Write contexts on the wire (doc "Labels and contexts"): the headers a
//! client names its context and its command with, the environment variable a
//! context is passed on in, and the encoding of a name in a header.
//!
//! Shared by the daemon (which reads them), the CLI and the GUI (which send
//! them), so the three cannot spell them differently.

/// The label a request writes in: a label id.
pub const LABEL_HEADER: &str = "x-metafolder-label";

/// The command a request is one step of: a key the client draws for the
/// command (32 hex digits), the same on all its requests. The command's label
/// is opened at its first write, inside [`LABEL_HEADER`]'s, and its later
/// writes go in it — so a command that only reads opens nothing.
pub const COMMAND_HEADER: &str = "x-metafolder-command";

/// The name of the command's label, [`encode`]d: the command line.
pub const COMMAND_NAME_HEADER: &str = "x-metafolder-command-name";

/// Its description, [`encode`]d: what it acted on (a query), when it says.
pub const COMMAND_DESCRIPTION_HEADER: &str = "x-metafolder-command-description";

/// The environment variable a context is passed on in ([`Context`]): the
/// label every `mf` started meanwhile writes in. Leaving the context is not
/// passing it on.
pub const LABEL_ENV: &str = "METAFOLDER_LABEL";

/// A context as it is passed on: a label, and the repository it belongs to —
/// a label id means nothing in another repository, so a client sends it only
/// with a request to that one. Spelt `<repo uuid>:<label id>`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Context {
    pub repo: uuid::Uuid,
    pub label: i64,
}

impl Context {
    /// Reads `<repo uuid>:<label id>`; `None` when it is not that.
    pub fn parse(text: &str) -> Option<Self> {
        let (repo, label) = text.trim().split_once(':')?;
        Some(Self { repo: uuid::Uuid::parse_str(repo).ok()?, label: label.parse().ok()? })
    }

    /// The label to send with a request to `path`: this one when the path is
    /// one of its repository's (`/repos/<uuid>/…`).
    pub fn label_for(&self, path: &str) -> Option<i64> {
        let rest = path.strip_prefix("/repos/")?;
        let repo = rest.split(['/', '?']).next()?;
        (uuid::Uuid::parse_str(repo).ok()? == self.repo).then_some(self.label)
    }
}

impl std::fmt::Display for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.repo.as_simple(), self.label)
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

/// A fresh command key, 32 lowercase hex digits.
pub fn new_command_key() -> String {
    uuid::Uuid::new_v4().as_simple().to_string()
}

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
        let context = Context { repo, label: 12 };
        assert_eq!(Context::parse(&context.to_string()), Some(context));
        assert_eq!(Context::parse(&format!("{}:12", repo.hyphenated())), Some(context));
        assert_eq!(Context::parse("12"), None);
        assert_eq!(Context::parse("x:12"), None);
        assert_eq!(
            context.label_for(&format!("/repos/{}/metarecords", repo.as_simple())),
            Some(12)
        );
        assert_eq!(context.label_for(&format!("/repos/{}", repo.hyphenated())), Some(12));
        assert_eq!(context.label_for(&format!("/repos/{}/log", uuid::Uuid::new_v4())), None);
        assert_eq!(context.label_for("/repos"), None);
    }

    #[test]
    fn a_malformed_escape_is_kept() {
        assert_eq!(decode("50%"), "50%");
        assert_eq!(decode("%zz"), "%zz");
        assert_eq!(decode("%4"), "%4");
    }
}
