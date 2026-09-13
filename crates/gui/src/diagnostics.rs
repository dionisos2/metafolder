//! Reading the daemon's diagnostics feed into the message panel.
//!
//! The daemon is a separate process, so the GUI has no handle on its stderr: a
//! warning from the watcher — a directory it could not watch, filesystem events
//! it had to drop — reached only whichever terminal started the daemon, and the
//! person using the GUI never saw it. The daemon therefore also keeps those
//! warnings in a ring (`GET /diagnostics?since=`), and this polls it into the
//! workspace message logs, next to the shell output and reconcile results.
//!
//! The parsing is kept pure here so the shapes that matter — a page that lost
//! entries, a malformed one, an empty one — are tested without a daemon.

use serde_json::Value;

/// One line to show, and the repository it is about.
///
/// `repo` is what makes routing possible: a flush on one repository has no
/// business in the message log of another one that merely happens to be open.
/// `None` means genuinely daemon-wide — it concerns every workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub repo: Option<String>,
    pub text: String,
}

impl Line {
    /// Whether this line belongs in a workspace whose active repository is
    /// `active`. A daemon-wide line belongs everywhere.
    pub fn concerns(&self, active: Option<&str>) -> bool {
        match &self.repo {
            None => true,
            Some(repo) => active == Some(repo.as_str()),
        }
    }
}

/// One page of the feed, turned into the lines to append, plus where to resume.
///
/// `since` is returned unchanged when the page carries no usable cursor, so a
/// daemon that answers something unexpected re-polls the same position instead
/// of skipping ahead or restarting from the beginning.
pub fn lines_from_page(page: &Value, since: u64) -> (Vec<Line>, u64) {
    let mut lines = Vec::new();
    // The ring dropped entries before we could read them: say so rather than
    // let them vanish, which would make the log quietly incomplete. Losing
    // entries is a fact about the feed, not about a repository, so it is shown
    // everywhere.
    match page.get("dropped").and_then(Value::as_u64) {
        Some(n) if n > 0 => {
            lines.push(Line {
                repo: None,
                text: format!("daemon: {n} earlier diagnostic(s) were lost (feed overflowed)"),
            });
        }
        _ => {}
    }
    for entry in page.get("entries").and_then(Value::as_array).into_iter().flatten() {
        if let Some(line) = format_entry(entry) {
            lines.push(line);
        }
    }
    let next = page.get("next_since").and_then(Value::as_u64).unwrap_or(since);
    (lines, next)
}

/// "daemon watcher: failed to watch …", or None when the entry carries no
/// message (nothing worth showing, and never a panic on a malformed page).
fn format_entry(entry: &Value) -> Option<Line> {
    let message = entry.get("message").and_then(Value::as_str)?;
    let scope = entry.get("scope").and_then(Value::as_str).unwrap_or("daemon");
    // The level is only spelled out when it is an error: a warning is the
    // common case and the prefix would be noise on every line.
    let level = match entry.get("level").and_then(Value::as_str) {
        Some("error") => "error: ",
        _ => "",
    };
    Some(Line {
        repo: entry.get("repo").and_then(Value::as_str).map(str::to_string),
        text: format!("daemon {scope}: {level}{message}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn texts(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|l| l.text.clone()).collect()
    }

    #[test]
    fn test_a_line_carries_the_repository_it_is_about() {
        // A flush concerns one repository. Without this the GUI had no way to
        // tell, and showed repository A's flushes in the message log of every
        // other repository that happened to be open.
        let page = json!({
            "entries": [
                { "level": "info", "scope": "executor", "repo": "aaaa",
                  "message": "flush on photos: 1 event" },
                { "level": "warning", "scope": "prune", "message": "daemon-wide" },
            ],
            "next_since": 2,
        });
        let (lines, _) = lines_from_page(&page, 0);
        assert_eq!(lines[0].repo.as_deref(), Some("aaaa"));
        assert_eq!(lines[1].repo, None);
    }

    #[test]
    fn test_a_repo_line_belongs_only_to_that_repos_workspaces() {
        let scoped = Line { repo: Some("aaaa".into()), text: "x".into() };
        assert!(scoped.concerns(Some("aaaa")));
        assert!(!scoped.concerns(Some("bbbb")));
        assert!(!scoped.concerns(None));

        // A daemon-wide line concerns every workspace, one with no repository
        // open included.
        let wide = Line { repo: None, text: "x".into() };
        assert!(wide.concerns(Some("aaaa")));
        assert!(wide.concerns(None));
    }

    #[test]
    fn test_an_empty_page_yields_nothing_and_keeps_the_cursor() {
        let page = json!({ "entries": [], "next_since": 7, "dropped": 0 });
        assert_eq!(lines_from_page(&page, 7), (vec![], 7));
    }

    #[test]
    fn test_entries_become_message_lines_naming_their_scope() {
        let page = json!({
            "entries": [
                { "id": 1, "at_ms": 1, "level": "warning", "scope": "watcher",
                  "message": "failed to watch /a/b", "repo": null },
            ],
            "next_since": 1,
            "dropped": 0,
        });
        let (lines, next) = lines_from_page(&page, 0);
        assert_eq!(texts(&lines), vec!["daemon watcher: failed to watch /a/b"]);
        assert_eq!(next, 1);
    }

    #[test]
    fn test_an_error_is_marked_but_a_warning_is_not() {
        let page = json!({
            "entries": [
                { "level": "warning", "scope": "prune", "message": "could not compact" },
                { "level": "error", "scope": "executor", "message": "flush failed" },
            ],
            "next_since": 2,
        });
        let (lines, _) = lines_from_page(&page, 0);
        assert_eq!(lines[0].text, "daemon prune: could not compact");
        assert_eq!(lines[1].text, "daemon executor: error: flush failed");
    }

    #[test]
    fn test_an_info_line_reads_like_any_other() {
        // What each watcher flush leaves behind (spec-file-tracking "What a
        // flush reports"): routine, and marked no differently from a warning.
        let page = json!({
            "entries": [
                { "level": "info", "scope": "executor",
                  "message": "flush on photos: 1 event in 3 ms -> 1 revision; create /a.txt" },
            ],
            "next_since": 9,
        });
        let (lines, next) = lines_from_page(&page, 0);
        assert_eq!(
            texts(&lines),
            vec!["daemon executor: flush on photos: 1 event in 3 ms -> 1 revision; create /a.txt"]
        );
        assert_eq!(next, 9);
    }

    #[test]
    fn test_a_page_that_lost_entries_says_so_before_the_rest() {
        let page = json!({
            "entries": [{ "level": "warning", "scope": "watcher", "message": "late" }],
            "next_since": 12,
            "dropped": 4,
        });
        let (lines, _) = lines_from_page(&page, 3);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].text.contains("4 earlier diagnostic(s) were lost"));
        assert_eq!(lines[1].text, "daemon watcher: late");
    }

    #[test]
    fn test_a_malformed_page_is_skipped_rather_than_fatal() {
        // No cursor: re-poll the same position instead of skipping or restarting.
        assert_eq!(lines_from_page(&json!({}), 5), (vec![], 5));
        assert_eq!(lines_from_page(&json!("nonsense"), 5), (vec![], 5));
        // An entry without a message has nothing to show.
        let page = json!({ "entries": [{ "scope": "watcher" }, { "message": "kept" }] });
        let (lines, next) = lines_from_page(&page, 5);
        assert_eq!(texts(&lines), vec!["daemon daemon: kept"]);
        assert_eq!(next, 5);
    }
}
