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

/// One page of the feed, turned into the lines to append, plus where to resume.
///
/// `since` is returned unchanged when the page carries no usable cursor, so a
/// daemon that answers something unexpected re-polls the same position instead
/// of skipping ahead or restarting from the beginning.
pub fn lines_from_page(page: &Value, since: u64) -> (Vec<String>, u64) {
    let mut lines = Vec::new();
    // The ring dropped entries before we could read them: say so rather than
    // let them vanish, which would make the log quietly incomplete.
    match page.get("dropped").and_then(Value::as_u64) {
        Some(n) if n > 0 => {
            lines.push(format!("daemon: {n} earlier diagnostic(s) were lost (feed overflowed)"));
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
fn format_entry(entry: &Value) -> Option<String> {
    let message = entry.get("message").and_then(Value::as_str)?;
    let scope = entry.get("scope").and_then(Value::as_str).unwrap_or("daemon");
    // The level is only spelled out when it is an error: a warning is the
    // common case and the prefix would be noise on every line.
    let level = match entry.get("level").and_then(Value::as_str) {
        Some("error") => "error: ",
        _ => "",
    };
    Some(format!("daemon {scope}: {level}{message}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
        assert_eq!(lines, vec!["daemon watcher: failed to watch /a/b"]);
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
        assert_eq!(lines[0], "daemon prune: could not compact");
        assert_eq!(lines[1], "daemon executor: error: flush failed");
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
        assert!(lines[0].contains("4 earlier diagnostic(s) were lost"));
        assert_eq!(lines[1], "daemon watcher: late");
    }

    #[test]
    fn test_a_malformed_page_is_skipped_rather_than_fatal() {
        // No cursor: re-poll the same position instead of skipping or restarting.
        assert_eq!(lines_from_page(&json!({}), 5), (vec![], 5));
        assert_eq!(lines_from_page(&json!("nonsense"), 5), (vec![], 5));
        // An entry without a message has nothing to show.
        let page = json!({ "entries": [{ "scope": "watcher" }, { "message": "kept" }] });
        let (lines, next) = lines_from_page(&page, 5);
        assert_eq!(lines, vec!["daemon daemon: kept"]);
        assert_eq!(next, 5);
    }
}
