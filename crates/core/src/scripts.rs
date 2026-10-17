//! Discovery of the installed shipped scripts (doc "Shipped defaults in the source tree").
//!
//! The user-facing helper scripts live in `~/.config/metafolder/scripts/`
//! (installed there by `metafolder-sync-config`). A *launchable* script carries
//! a one-line `# Summary:` header comment right after the shebang; this module
//! finds exactly those top-level `*.sh` files, so the GUI's `script:run`
//! launcher and any future `mf script` command share one definition of "the
//! scripts a user can run" and their descriptions. Sourced-only libraries
//! (`lib/mf-gui.sh`, `gui-tag-next.sh`) carry no `# Summary:` and are skipped.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// One runnable script: its base name, one-line summary, and absolute path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScriptInfo {
    /// File name including the `.sh` extension (e.g. `gui-tag-classify.sh`).
    pub name: String,
    /// The text after `# Summary:` in the header, trimmed.
    pub summary: String,
    /// Absolute path to the script file.
    pub path: PathBuf,
}

/// Extracts the `# Summary:` header value from a script's source, if present.
/// The marker is matched anywhere in a leading comment line (so `#Summary:` and
/// `#   Summary:` both work); scanning stops at the first non-comment,
/// non-shebang, non-blank line so only the header block is considered.
pub fn parse_summary(source: &str) -> Option<String> {
    for line in source.lines() {
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with("#!") {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix('#') {
            let rest = rest.trim_start();
            if let Some(summary) = rest.strip_prefix("Summary:") {
                let summary = summary.trim();
                return (!summary.is_empty()).then(|| summary.to_string());
            }
            // Another comment line in the header block: keep scanning.
            continue;
        }
        // First line of actual code — no summary in the header.
        break;
    }
    None
}

/// Lists the launchable scripts directly under `dir` (non-recursive, so the
/// `lib/` subdirectory is ignored): every top-level `*.sh` file carrying a
/// `# Summary:` header, sorted by name. A missing directory yields an empty
/// list (scripts are optional — sync-config may not have run).
pub fn list_scripts_in(dir: &Path) -> Vec<ScriptInfo> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() || path.extension().and_then(|e| e.to_str()) != Some("sh") {
            continue;
        }
        let Ok(source) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Some(summary) = parse_summary(&source) else {
            continue;
        };
        let name = match path.file_name().and_then(|n| n.to_str()) {
            Some(n) => n.to_string(),
            None => continue,
        };
        out.push(ScriptInfo { name, summary, path });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Lists the launchable scripts installed under the user config
/// (`~/.config/metafolder/scripts/`). Empty when the config root cannot be
/// resolved or the directory is absent.
pub fn list_scripts() -> Vec<ScriptInfo> {
    match crate::config::scripts_dir() {
        Some(dir) => list_scripts_in(&dir),
        None => Vec::new(),
    }
}

/// The subdirectory of the scripts directory holding the templates: scripts
/// to copy and adapt (`mf script new`), not to run — one level down, so the
/// launcher, which lists the top level only, does not offer them.
pub const TEMPLATES: &str = "templates";

/// Lists the templates under `scripts_dir`'s `templates/`, as
/// [`list_scripts_in`] lists scripts (those with a `# Summary:`).
pub fn list_templates_in(scripts_dir: &Path) -> Vec<ScriptInfo> {
    list_scripts_in(&scripts_dir.join(TEMPLATES))
}

/// A script file name: `name` with `.sh`, refused when it is not a plain name
/// (a path separator, `..`, empty), so nothing lands outside the directory.
fn script_file_name(name: &str) -> Result<String, String> {
    let base = name.strip_suffix(".sh").unwrap_or(name);
    if base.is_empty() || base.contains('/') || base.contains('\\') || base.starts_with('.') {
        return Err(format!("\"{name}\" is not a script name (a plain file name, no path)"));
    }
    Ok(format!("{base}.sh"))
}

/// Copies the template `template` (with or without `.sh`) into `scripts_dir`,
/// as `name` or under the template's own name, and returns the new script's
/// path. Never overwrites: a script of that name is an error, left untouched.
pub fn new_from_template(
    scripts_dir: &Path,
    template: &str,
    name: Option<&str>,
) -> Result<PathBuf, String> {
    let template_file = script_file_name(template)?;
    let source = scripts_dir.join(TEMPLATES).join(&template_file);
    if !source.is_file() {
        let known: Vec<_> = list_templates_in(scripts_dir).into_iter().map(|t| t.name).collect();
        return Err(format!(
            "no template \"{template}\" (templates: {})",
            if known.is_empty() { "none installed".to_string() } else { known.join(", ") }
        ));
    }
    let target = scripts_dir.join(script_file_name(name.unwrap_or(&template_file))?);
    let body =
        std::fs::read(&source).map_err(|e| format!("cannot read {}: {e}", source.display()))?;
    // `create_new`: the existence check and the write are one step.
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&target).map_err(
        |e| match e.kind() {
            std::io::ErrorKind::AlreadyExists => {
                format!("{} already exists (choose another name with --name)", target.display())
            }
            _ => format!("cannot create {}: {e}", target.display()),
        },
    )?;
    file.write_all(&body).map_err(|e| format!("cannot write {}: {e}", target.display()))?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn scratch() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join("metafolder-tests").join(format!(
            "mf-scripts-{}-{}",
            std::process::id(),
            n
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn parses_summary_after_shebang() {
        let src = "#!/usr/bin/env bash\n# Summary: Do the thing.\n# more text\nset -e\n";
        assert_eq!(parse_summary(src).as_deref(), Some("Do the thing."));
    }

    #[test]
    fn summary_tolerates_spacing() {
        assert_eq!(
            parse_summary("#!/bin/sh\n#   Summary:   spaced out  \n").as_deref(),
            Some("spaced out"),
        );
    }

    #[test]
    fn no_summary_returns_none() {
        assert_eq!(parse_summary("#!/bin/sh\n# just a comment\nset -e\n"), None);
    }

    #[test]
    fn summary_after_code_is_ignored() {
        // A `# Summary:` appearing past the header block does not count.
        assert_eq!(parse_summary("#!/bin/sh\nset -e\n# Summary: nope\n"), None);
    }

    #[test]
    fn empty_summary_returns_none() {
        assert_eq!(parse_summary("#!/bin/sh\n# Summary:   \n"), None);
    }

    #[test]
    fn lists_only_summarised_top_level_scripts() {
        let dir = scratch();
        std::fs::write(dir.join("b-tool.sh"), "#!/bin/sh\n# Summary: B.\n").unwrap();
        std::fs::write(dir.join("a-tool.sh"), "#!/bin/sh\n# Summary: A.\n").unwrap();
        std::fs::write(dir.join("noheader.sh"), "#!/bin/sh\necho hi\n").unwrap();
        std::fs::write(dir.join("notes.txt"), "# Summary: not a script\n").unwrap();
        std::fs::create_dir_all(dir.join("lib")).unwrap();
        std::fs::write(dir.join("lib/helper.sh"), "#!/bin/sh\n# Summary: L.\n").unwrap();

        let got = list_scripts_in(&dir);
        let names: Vec<_> = got.iter().map(|s| s.name.as_str()).collect();
        // Sorted, summary-only, top-level (no lib/, no .txt, no headerless).
        assert_eq!(names, vec!["a-tool.sh", "b-tool.sh"]);
        assert_eq!(got[0].summary, "A.");
        assert_eq!(got[0].path, dir.join("a-tool.sh"));
    }

    #[test]
    fn missing_dir_is_empty() {
        let dir = scratch();
        let missing = dir.join("does-not-exist");
        assert!(list_scripts_in(&missing).is_empty());
    }

    #[test]
    fn templates_live_one_level_down_and_are_not_offered_to_run() {
        let dir = scratch();
        std::fs::create_dir_all(dir.join(TEMPLATES)).unwrap();
        std::fs::write(dir.join("templates/sort-folder.sh"), "#!/bin/sh\n# Summary: S.\n").unwrap();
        assert!(list_scripts_in(&dir).is_empty());
        let got = list_templates_in(&dir);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "sort-folder.sh");
        assert_eq!(got[0].summary, "S.");
    }

    #[test]
    fn a_new_script_is_a_copy_of_its_template() {
        let dir = scratch();
        std::fs::create_dir_all(dir.join(TEMPLATES)).unwrap();
        let body = "#!/bin/sh\n# Summary: S.\necho sort\n";
        std::fs::write(dir.join("templates/sort-folder.sh"), body).unwrap();

        // The name works with or without `.sh`; the copy keeps it by default.
        let made = new_from_template(&dir, "sort-folder", None).unwrap();
        assert_eq!(made, dir.join("sort-folder.sh"));
        assert_eq!(std::fs::read_to_string(&made).unwrap(), body);

        // Another name, given with or without `.sh`.
        let mine = new_from_template(&dir, "sort-folder.sh", Some("my-sort")).unwrap();
        assert_eq!(mine, dir.join("my-sort.sh"));
        assert!(list_scripts_in(&dir).iter().any(|s| s.name == "my-sort.sh"));
    }

    #[test]
    fn a_new_script_never_overwrites_one() {
        let dir = scratch();
        std::fs::create_dir_all(dir.join(TEMPLATES)).unwrap();
        std::fs::write(dir.join("templates/sort-folder.sh"), "#!/bin/sh\n").unwrap();
        std::fs::write(dir.join("sort-folder.sh"), "mine\n").unwrap();
        let err = new_from_template(&dir, "sort-folder", None).unwrap_err();
        assert!(err.contains("already exists"), "{err}");
        assert_eq!(std::fs::read_to_string(dir.join("sort-folder.sh")).unwrap(), "mine\n");
    }

    #[test]
    fn an_unknown_template_names_the_ones_there_are() {
        let dir = scratch();
        std::fs::create_dir_all(dir.join(TEMPLATES)).unwrap();
        std::fs::write(dir.join("templates/sort-folder.sh"), "#!/bin/sh\n# Summary: S.\n").unwrap();
        let err = new_from_template(&dir, "nope", None).unwrap_err();
        assert!(err.contains("no template \"nope\"") && err.contains("sort-folder"), "{err}");
    }

    #[test]
    fn a_name_cannot_leave_the_scripts_directory() {
        let dir = scratch();
        std::fs::create_dir_all(dir.join(TEMPLATES)).unwrap();
        std::fs::write(dir.join("templates/sort-folder.sh"), "#!/bin/sh\n").unwrap();
        assert!(new_from_template(&dir, "sort-folder", Some("../evil")).is_err());
        assert!(new_from_template(&dir, "../templates/sort-folder", None).is_err());
    }
}
