//! `~/.config/metafolder-gui/` management (spec-gui "Panel type system",
//! "Style and theming"): first-run installation of editable defaults,
//! the always-refreshed `panel-types-defaults/` mirror users can diff
//! against after upgrades, keybinding loading, and the port discovery
//! file for scripts.

use crate::keybindings::KeybindingSet;
use include_dir::{include_dir, Dir};
use std::path::{Path, PathBuf};

static PANEL_TYPES: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/panel-types");
const DEFAULT_KEYBINDINGS: &str = include_str!("../default-config/keybindings.toml");
const DEFAULT_STYLE: &str = include_str!("../default-config/style.css");

pub struct ConfigDir {
    root: PathBuf,
    /// Where `gui.port` is written; the config root unless the platform
    /// runtime dir is used (see [`ConfigDir::default_location`]).
    port_dir: PathBuf,
}

impl ConfigDir {
    /// The real user config dir: `~/.config/metafolder-gui` (respecting
    /// `$XDG_CONFIG_HOME`); the port file goes to
    /// `$XDG_RUNTIME_DIR/metafolder/` when available.
    pub fn default_location() -> Result<Self, String> {
        let root = dirs::config_dir()
            .ok_or("cannot determine the user configuration directory")?
            .join("metafolder-gui");
        let port_dir = std::env::var_os("XDG_RUNTIME_DIR")
            .map(|dir| PathBuf::from(dir).join("metafolder"))
            .unwrap_or_else(|| root.clone());
        Ok(ConfigDir { root, port_dir })
    }

    /// A config dir at an explicit location (tests).
    pub fn at(root: PathBuf) -> Self {
        let port_dir = root.clone();
        ConfigDir { root, port_dir }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    // ── Installation ─────────────────────────────────────────────────────

    /// Installs the shipped defaults: `keybindings.toml`, `style.css` and
    /// `panel-types/*` are written only when missing (user edits are never
    /// overwritten); `panel-types-defaults/*` is always rewritten so users
    /// can diff their edits against the current defaults.
    pub fn install_defaults(&self) -> Result<(), String> {
        let io = |e: std::io::Error| format!("config install failed: {e}");
        std::fs::create_dir_all(&self.root).map_err(io)?;

        for (name, content) in [
            ("keybindings.toml", DEFAULT_KEYBINDINGS),
            ("style.css", DEFAULT_STYLE),
        ] {
            let path = self.root.join(name);
            if !path.exists() {
                std::fs::write(&path, content).map_err(io)?;
            }
        }

        install_dir(&PANEL_TYPES, &self.root.join("panel-types"), false)?;
        install_dir(&PANEL_TYPES, &self.root.join("panel-types-defaults"), true)?;
        Ok(())
    }

    // ── Keybindings ──────────────────────────────────────────────────────

    /// Shipped defaults merged with the user's `keybindings.toml` (which
    /// may not exist).
    pub fn load_keybindings(&self) -> Result<KeybindingSet, String> {
        let user_path = self.root.join("keybindings.toml");
        let user = match std::fs::read_to_string(&user_path) {
            Ok(content) => content,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(format!("cannot read {}: {e}", user_path.display())),
        };
        KeybindingSet::from_sources(DEFAULT_KEYBINDINGS, &user)
    }

    // ── Style ────────────────────────────────────────────────────────────

    pub fn style_css_path(&self) -> PathBuf {
        self.root.join("style.css")
    }

    /// The user stylesheet, falling back to the shipped default.
    pub fn load_style(&self) -> String {
        std::fs::read_to_string(self.style_css_path())
            .unwrap_or_else(|_| DEFAULT_STYLE.to_string())
    }

    // ── Panel types ──────────────────────────────────────────────────────

    pub fn panel_types_dir(&self) -> PathBuf {
        self.root.join("panel-types")
    }

    /// Directories under `panel-types/` containing an `index.html`.
    pub fn list_panel_types(&self) -> Result<Vec<String>, String> {
        let dir = self.panel_types_dir();
        let entries = std::fs::read_dir(&dir)
            .map_err(|e| format!("cannot read {}: {e}", dir.display()))?;
        let mut types = Vec::new();
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if entry.path().join("index.html").is_file() {
                types.push(name);
            }
        }
        types.sort();
        Ok(types)
    }

    /// Resolves a panel type name to its directory; `None` for unknown
    /// names or names that escape `panel-types/` (path traversal).
    pub fn panel_dir(&self, name: &str) -> Option<PathBuf> {
        if name.contains('/') || name.contains('\\') || name.contains("..") {
            return None;
        }
        let dir = self.panel_types_dir().join(name);
        if dir.join("index.html").is_file() {
            Some(dir)
        } else {
            None
        }
    }

    // ── Port discovery file ──────────────────────────────────────────────

    pub fn port_file_path(&self) -> PathBuf {
        self.port_dir.join("gui.port")
    }

    /// Writes the bound GUI port for script discovery; returns the path.
    pub fn write_port_file(&self, port: u16) -> Result<PathBuf, String> {
        std::fs::create_dir_all(&self.port_dir)
            .map_err(|e| format!("cannot create {}: {e}", self.port_dir.display()))?;
        let path = self.port_file_path();
        std::fs::write(&path, format!("{port}\n"))
            .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
        Ok(path)
    }

    /// Removes the port file (clean exit); missing file is not an error.
    pub fn remove_port_file(&self) {
        let _ = std::fs::remove_file(self.port_file_path());
    }
}

/// Copies an embedded directory tree to disk. With `overwrite`, existing
/// files are rewritten; otherwise they are left untouched.
fn install_dir(source: &Dir<'_>, target: &Path, overwrite: bool) -> Result<(), String> {
    let io = |e: std::io::Error| format!("config install failed: {e}");
    for dir in source.dirs() {
        std::fs::create_dir_all(target.join(dir.path())).map_err(io)?;
        install_dir(dir, target, overwrite)?;
    }
    for file in source.files() {
        let path = target.join(file.path());
        if overwrite || !path.exists() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(io)?;
            }
            std::fs::write(&path, file.contents()).map_err(io)?;
        }
    }
    Ok(())
}
