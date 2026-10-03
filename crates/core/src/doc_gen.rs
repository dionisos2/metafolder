//! The generated half of the documentation wiki (doc "Writing documentation",
//! "The generated catalogs"): the tests that read a catalog out of
//! the code (the CLI's clap tree, the GUI's commands and panels) write it as
//! `.tid` data notes under `docs/wiki/tiddlers/generated/<catalog>/`, through
//! [`sync_generated`]. Normally they only *compare* — a stale file fails the
//! test, which is how `cargo test` keeps the wiki in step with the code — and
//! with `MF_DOC_UPDATE=1` (what `scripts/doc gen` sets) they rewrite it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// The environment variable that turns the comparison into a rewrite.
pub const UPDATE_ENV: &str = "MF_DOC_UPDATE";

/// A tiddler in the `.tid` format: one `name: value` line per field (a value
/// is flattened to one line), a blank line, the text.
pub fn tid(fields: &[(&str, &str)], text: &str) -> String {
    let mut out = String::new();
    for (name, value) in fields {
        let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
        out.push_str(&format!("{name}: {value}\n"));
    }
    out.push('\n');
    out.push_str(text);
    out
}

/// A file-name slug: lowercase ASCII letters, digits and `_`, a `:` inside a
/// name as `_` (a GUI command and the CLI command it mirrors — `mf:duplicate`,
/// `mf duplicate` — must not collide), every other run of characters one dash.
/// The same function as the wiki tooling's `slugify` for the names the code has
/// (ASCII).
pub fn slug(name: &str) -> String {
    let chars: Vec<char> = name.chars().flat_map(char::to_lowercase).collect();
    let mut out = String::new();
    for (i, &c) in chars.iter().enumerate() {
        let inside = |j: Option<usize>| {
            j.and_then(|j| chars.get(j)).is_some_and(char::is_ascii_alphanumeric)
        };
        if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c);
        } else if c == ':' && inside(i.checked_sub(1)) && inside(Some(i + 1)) {
            out.push('_');
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_string()
}

/// Inline wikitext code for any text: double backticks, which a single
/// backtick (a key name) cannot close.
pub fn code(text: &str) -> String {
    format!("``{text}``")
}

/// The directory of one catalog's generated notes, from a crate's manifest dir.
pub fn generated_dir(manifest_dir: &str, catalog: &str) -> PathBuf {
    Path::new(manifest_dir).join("../../docs/wiki/tiddlers/generated").join(slug(catalog))
}

/// Makes `dir` hold exactly `files` (file name → content) when
/// `MF_DOC_UPDATE` is set; otherwise checks that it already does, and names
/// every difference.
pub fn sync_generated(dir: &Path, files: &BTreeMap<String, String>) -> Result<(), String> {
    sync_generated_with(dir, files, std::env::var_os(UPDATE_ENV).is_some())
}

fn sync_generated_with(
    dir: &Path,
    files: &BTreeMap<String, String>,
    update: bool,
) -> Result<(), String> {
    let existing: BTreeMap<String, String> = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().into_owned();
                let content = std::fs::read_to_string(e.path()).ok()?;
                name.ends_with(".tid").then_some((name, content))
            })
            .collect(),
        Err(_) => BTreeMap::new(),
    };
    if update {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        for name in existing.keys().filter(|n| !files.contains_key(*n)) {
            std::fs::remove_file(dir.join(name)).map_err(|e| format!("{name}: {e}"))?;
        }
        for (name, content) in files {
            if existing.get(name) != Some(content) {
                std::fs::write(dir.join(name), content).map_err(|e| format!("{name}: {e}"))?;
            }
        }
        return Ok(());
    }
    let mut problems = Vec::new();
    for (name, content) in files {
        match existing.get(name) {
            None => problems.push(format!("missing {name}")),
            Some(old) if old != content => problems.push(format!("stale {name}")),
            Some(_) => {}
        }
    }
    for name in existing.keys().filter(|n| !files.contains_key(*n)) {
        problems.push(format!("obsolete {name}"));
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "{} is out of date with the code ({}); run scripts/doc gen",
            dir.display(),
            problems.join(", ")
        ))
    }
}

/// One method of one route of an Axum router, as its source declares it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Route {
    /// Upper case: `GET`, `POST`…
    pub method: String,
    /// The route pattern (`/repos/:repo/query`).
    pub path: String,
    /// The handler's path (`gui_api::post_command`), or `a closure`.
    pub handler: String,
}

#[derive(Debug, PartialEq)]
enum Token {
    Str(String),
    Ident(String),
    Punct(char),
}

/// Rust source as identifiers (with their `::` paths), string literals and
/// punctuation; comments and whitespace dropped. Just enough to read a router.
fn tokens(src: &str) -> Vec<Token> {
    let chars: Vec<char> = src.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '/' && chars.get(i + 1) == Some(&'/') {
            while i < chars.len() && chars[i] != '\n' {
                i += 1;
            }
        } else if c == '"' {
            let mut s = String::new();
            i += 1;
            while i < chars.len() && chars[i] != '"' {
                if chars[i] == '\\' {
                    i += 1;
                }
                if let Some(&c) = chars.get(i) {
                    s.push(c);
                }
                i += 1;
            }
            i += 1;
            out.push(Token::Str(s));
        } else if c.is_alphabetic() || c == '_' {
            let mut s = String::new();
            while i < chars.len() {
                let c = chars[i];
                if c.is_alphanumeric() || c == '_' {
                    s.push(c);
                    i += 1;
                } else if c == ':'
                    && chars.get(i + 1) == Some(&':')
                    && chars.get(i + 2).is_some_and(|c| c.is_alphabetic() || *c == '_')
                {
                    s.push_str("::");
                    i += 2;
                } else {
                    break;
                }
            }
            out.push(Token::Ident(s));
        } else {
            out.push(Token::Punct(c));
            i += 1;
        }
    }
    out
}

/// Every route of the Axum router `src` builds — each `.route("path",
/// get(handler).post(other)…)` call — one entry per method, in source order.
pub fn routes(src: &str) -> Vec<Route> {
    const METHODS: [&str; 5] = ["get", "post", "put", "patch", "delete"];
    let t = tokens(src);
    let mut out = Vec::new();
    let mut i = 0;
    while i + 3 < t.len() {
        let path = match (&t[i], &t[i + 1], &t[i + 2], &t[i + 3]) {
            (Token::Punct('.'), Token::Ident(r), Token::Punct('('), Token::Str(p))
                if r == "route" =>
            {
                p
            }
            _ => {
                i += 1;
                continue;
            }
        };
        let mut depth = 1;
        let mut j = i + 4;
        while j < t.len() && depth > 0 {
            match &t[j] {
                Token::Punct('(') => depth += 1,
                Token::Punct(')') => depth -= 1,
                Token::Ident(m)
                    if depth == 1
                        && METHODS.contains(&m.as_str())
                        && t.get(j + 1) == Some(&Token::Punct('(')) =>
                {
                    let handler = match (t.get(j + 2), t.get(j + 3)) {
                        (Some(Token::Ident(h)), Some(Token::Punct(')'))) => h.clone(),
                        _ => "a closure".to_string(),
                    };
                    out.push(Route { method: m.to_uppercase(), path: path.clone(), handler });
                }
                _ => {}
            }
            j += 1;
        }
        i = j;
    }
    out
}

/// The `HTTP endpoint` catalog's generated notes for the routes of one server
/// (`daemon`, `GUI`), keyed by file name: one per method and path.
pub fn endpoint_notes(server: &str, routes: &[Route]) -> BTreeMap<String, String> {
    routes
        .iter()
        .map(|r| {
            let target = format!("{} {}", r.method, r.path);
            let title = format!("$:/mf/gen/HTTP endpoint/{target}");
            let summary = format!("Served by the {server}.");
            let handler = code(&r.handler);
            let text = format!("!! Reference\n\n|!Server |{server} |\n|!Handler |{handler} |\n");
            let fields = [
                ("title", title.as_str()),
                ("catalog", "HTTP endpoint"),
                ("target", target.as_str()),
                ("summary", summary.as_str()),
            ];
            (format!("{}.tid", slug(&target)), tid(&fields, &text))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_endpoint_note_is_named_by_method_and_path() {
        let route = Route {
            method: "POST".into(),
            path: "/repos/:repo/query".into(),
            handler: "run_query".into(),
        };
        let notes = endpoint_notes("daemon", &[route]);
        let note = &notes["post-repos-repo-query.tid"];
        assert!(
            note.starts_with("title: $:/mf/gen/HTTP endpoint/POST /repos/:repo/query\n"),
            "{note}"
        );
        assert!(note.contains("catalog: HTTP endpoint\n"), "{note}");
        assert!(note.contains("target: POST /repos/:repo/query\n"), "{note}");
        assert!(note.contains("|!Handler |``run_query`` |"), "{note}");
    }

    #[test]
    fn routes_reads_every_method_of_every_route() {
        let src = r#"
            Router::new()
                .route("/health", get(health))
                .route("/repos/:repo", get(get_repo).patch(rename_repo))
                // a comment with .route("/not", get(a_route))
                .route(
                    "/repos/:repo/fields/:id",
                    get(by_id).patch(patch_by_id).delete(delete_by_id),
                )
                .route("/fsraw", get(fsraw::serve).layer(map_response(fsraw::inert)))
                .route(
                    "/__commands.js",
                    get(|State(state): State<S>| async move { state.get("x(") }),
                )
        "#;
        let found: Vec<(String, String, String)> =
            routes(src).into_iter().map(|r| (r.method, r.path, r.handler)).collect();
        let want = [
            ("GET", "/health", "health"),
            ("GET", "/repos/:repo", "get_repo"),
            ("PATCH", "/repos/:repo", "rename_repo"),
            ("GET", "/repos/:repo/fields/:id", "by_id"),
            ("PATCH", "/repos/:repo/fields/:id", "patch_by_id"),
            ("DELETE", "/repos/:repo/fields/:id", "delete_by_id"),
            ("GET", "/fsraw", "fsraw::serve"),
            ("GET", "/__commands.js", "a closure"),
        ];
        let want: Vec<(String, String, String)> =
            want.iter().map(|(m, p, h)| (m.to_string(), p.to_string(), h.to_string())).collect();
        assert_eq!(found, want);
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("metafolder-tests")
            .join(format!("doc-gen-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn tid_writes_one_line_per_field_then_the_text() {
        assert_eq!(
            tid(&[("title", "X"), ("summary", "two\n  lines")], "Body\n"),
            "title: X\nsummary: two lines\n\nBody\n"
        );
    }

    #[test]
    fn slug_matches_the_wiki_tooling() {
        assert_eq!(slug("trash:restore"), "trash_restore");
        assert_ne!(slug("mf:duplicate"), slug("mf duplicate"));
        assert_eq!(slug("mfr_path"), "mfr_path");
        assert_eq!(slug("POST /repos/:repo/query"), "post-repos-repo-query");
        assert_eq!(slug("mf trash restore"), "mf-trash-restore");
        assert_eq!(slug("lib/mf-gui.sh"), "lib-mf-gui-sh");
        assert_eq!(slug("--odd--"), "odd");
    }

    #[test]
    fn compare_names_every_difference() {
        let dir = scratch("compare");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.tid"), "old").unwrap();
        std::fs::write(dir.join("gone.tid"), "x").unwrap();
        let files =
            BTreeMap::from([("a.tid".to_string(), "new".into()), ("b.tid".into(), "b".into())]);
        let err = sync_generated_with(&dir, &files, false).unwrap_err();
        assert!(err.contains("stale a.tid"), "{err}");
        assert!(err.contains("missing b.tid"), "{err}");
        assert!(err.contains("obsolete gone.tid"), "{err}");
        assert!(err.contains("scripts/doc gen"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn update_makes_the_directory_exactly_the_files() {
        let dir = scratch("update");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("gone.tid"), "x").unwrap();
        let files = BTreeMap::from([("a.tid".to_string(), "a".to_string())]);
        sync_generated_with(&dir, &files, true).unwrap();
        assert!(!dir.join("gone.tid").exists());
        assert_eq!(std::fs::read_to_string(dir.join("a.tid")).unwrap(), "a");
        sync_generated_with(&dir, &files, false).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
    }
}
