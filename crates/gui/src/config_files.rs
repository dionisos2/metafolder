//! The configuration files the config panel lists (doc "config panel"):
//! every file of the user configuration repository — but its `.git/` and the
//! rendered `docs/`, which are not configuration — and every file of the
//! active repository's `.metafolder/config/`.
//!
//! The list is read from disk, not declared, so a file the user adds (a
//! script, a panel type) shows up without the panel being told about it.

use std::path::Path;

use serde::Serialize;

/// One file, as the panel shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConfigFile {
    /// The block it is listed under: `gui`, `panel-types`, `daemon`, …, or
    /// `repository` for the active repository's own files.
    pub category: String,
    /// Its path relative to the directory listed, `/`-separated.
    pub name: String,
    /// Its absolute path.
    pub path: String,
    /// The `config:reload` target that re-reads it, when there is one.
    pub reload: Option<&'static str>,
}

/// The top-level directories of the configuration repository that hold no
/// configuration: the rendered wiki.
const NOT_CONFIGURATION: [&str; 1] = ["docs"];

/// The file each `config:reload` target re-reads, relative to the
/// configuration repository's root.
const RELOADED: [(&str, &str); 6] = [
    ("gui/keybindings.toml", "keybindings"),
    ("gui/style.css", "style"),
    ("gui/commands.js", "commands"),
    ("gui/user-commands.js", "commands"),
    ("gui/macros.js", "commands"),
    ("core/query-grammar", "grammar"),
];

/// Every file of the user configuration repository at `root`, sorted by name.
pub fn global_files(root: &Path) -> Vec<ConfigFile> {
    let mut names = Vec::new();
    walk(root, "", &mut names);
    names.retain(|name| {
        let top = name.split('/').next().unwrap_or_default();
        !(name.contains('/') && NOT_CONFIGURATION.contains(&top))
    });
    names
        .into_iter()
        .map(|name| ConfigFile {
            category: category_of(&name),
            path: root.join(&name).display().to_string(),
            reload: RELOADED.iter().find(|(file, _)| *file == name).map(|(_, target)| *target),
            name,
        })
        .collect()
}

/// Every file of a repository's `.metafolder/config/` at `dir`, sorted by
/// name; none when it does not exist.
pub fn repo_files(dir: &Path) -> Vec<ConfigFile> {
    let mut names = Vec::new();
    walk(dir, "", &mut names);
    names
        .into_iter()
        .map(|name| ConfigFile {
            category: "repository".to_string(),
            path: dir.join(&name).display().to_string(),
            reload: None,
            name,
        })
        .collect()
}

/// `gui/panel-types/…` is a block of its own: the panel files would bury the
/// GUI's few. A file at the root is `root`.
fn category_of(name: &str) -> String {
    match name.split_once('/') {
        Some(("gui", rest)) if rest.starts_with("panel-types/") => "panel-types".to_string(),
        Some((top, _)) => top.to_string(),
        None => "root".to_string(),
    }
}

/// The files under `dir`, as paths relative to the walk's start, skipping
/// `.git`. An unreadable directory is left out rather than failing the list.
fn walk(dir: &Path, prefix: &str, out: &mut Vec<String>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let file_name = entry.file_name().to_string_lossy().into_owned();
        if file_name == ".git" {
            continue;
        }
        let name = format!("{prefix}{file_name}");
        // `file_type` does not follow a symlink: a linked file is listed, a
        // linked directory is not walked into.
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => walk(&entry.path(), &format!("{name}/"), out),
            Ok(_) => out.push(name),
            Err(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;

    fn temp_root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("metafolder-tests")
            .join(format!("config-files-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn touch(root: &Path, name: &str) {
        let path = root.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, "").unwrap();
    }

    #[test]
    fn test_global_files_lists_every_file_but_git_and_docs() {
        let root = temp_root("global");
        for name in [
            "gui/keybindings.toml",
            "gui/config.toml",
            "gui/panel-types/viewer/main.js",
            "daemon/repo-default/schema.json",
            "scripts/lib/mf-gui.sh",
            ".gitignore",
            ".git/HEAD",
            "docs/index.html",
        ] {
            touch(&root, name);
        }
        let files = global_files(&root);
        let listed: Vec<(&str, &str)> =
            files.iter().map(|f| (f.category.as_str(), f.name.as_str())).collect();
        assert_eq!(
            listed,
            vec![
                ("root", ".gitignore"),
                ("daemon", "daemon/repo-default/schema.json"),
                ("gui", "gui/config.toml"),
                ("gui", "gui/keybindings.toml"),
                ("panel-types", "gui/panel-types/viewer/main.js"),
                ("scripts", "scripts/lib/mf-gui.sh"),
            ]
        );
        assert_eq!(files[3].path, root.join("gui/keybindings.toml").display().to_string());
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn test_only_the_reloadable_files_name_a_reload_target() {
        let root = temp_root("reload");
        for name in
            ["gui/keybindings.toml", "gui/macros.js", "core/query-grammar", "cli/config.toml"]
        {
            touch(&root, name);
        }
        let reloads: Vec<(String, Option<&str>)> =
            global_files(&root).into_iter().map(|f| (f.name, f.reload)).collect();
        assert_eq!(
            reloads,
            vec![
                ("cli/config.toml".to_string(), None),
                ("core/query-grammar".to_string(), Some("grammar")),
                ("gui/keybindings.toml".to_string(), Some("keybindings")),
                ("gui/macros.js".to_string(), Some("commands")),
            ]
        );
        fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn test_repo_files_lists_the_repository_configuration() {
        let root = temp_root("repo");
        for name in ["schema.json", "metadata-map.toml", ".git/HEAD"] {
            touch(&root, name);
        }
        let names: Vec<(String, String)> =
            repo_files(&root).into_iter().map(|f| (f.category, f.name)).collect();
        assert_eq!(
            names,
            vec![
                ("repository".to_string(), "metadata-map.toml".to_string()),
                ("repository".to_string(), "schema.json".to_string()),
            ]
        );
        assert!(repo_files(&root.join("missing")).is_empty());
        fs::remove_dir_all(&root).unwrap();
    }
}
