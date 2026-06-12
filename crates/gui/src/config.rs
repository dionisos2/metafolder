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
    /// can diff their edits against the current defaults. A user panel
    /// file identical to the previous defaults mirror was never edited,
    /// so it is upgraded to the new shipped version in place.
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

        // Before the mirror refresh: it still holds the defaults shipped
        // by the previous run, the reference for "never edited".
        upgrade_pristine(
            &PANEL_TYPES,
            &self.root.join("panel-types"),
            &self.root.join("panel-types-defaults"),
        )?;
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

    /// Writes (or replaces) one user keybinding override and returns the
    /// recompiled merged set. Combos are matched after normalization, so
    /// `"shift+ctrl+a"` replaces an existing `"ctrl+shift+a"` metarecord.
    pub fn set_user_keybinding(
        &self,
        combo: &str,
        command: &str,
        when: Option<&str>,
        text_input: bool,
    ) -> Result<KeybindingSet, String> {
        let normalized = crate::keybindings::parse_combo(combo)?.join(" ");
        let mut table = self.read_user_keybindings_table()?;
        table.retain(|key, _| {
            crate::keybindings::parse_combo(key)
                .map(|keys| keys.join(" ") != normalized)
                .unwrap_or(true)
        });
        let mut entry = toml::Table::new();
        entry.insert("command".into(), toml::Value::String(command.to_string()));
        if let Some(when) = when {
            entry.insert("when".into(), toml::Value::String(when.to_string()));
        }
        if text_input {
            entry.insert("text-input".into(), toml::Value::Boolean(true));
        }
        table.insert(normalized, toml::Value::Table(entry));
        self.write_user_keybindings_table(&table)?;
        self.load_keybindings()
    }

    /// Removes a user override (reverting to the shipped default);
    /// missing metarecords are a no-op. Returns the recompiled set.
    pub fn remove_user_keybinding(&self, combo: &str) -> Result<KeybindingSet, String> {
        let normalized = crate::keybindings::parse_combo(combo)?.join(" ");
        let mut table = self.read_user_keybindings_table()?;
        table.retain(|key, _| {
            crate::keybindings::parse_combo(key)
                .map(|keys| keys.join(" ") != normalized)
                .unwrap_or(true)
        });
        self.write_user_keybindings_table(&table)?;
        self.load_keybindings()
    }

    fn read_user_keybindings_table(&self) -> Result<toml::Table, String> {
        let path = self.root.join("keybindings.toml");
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                toml::from_str(&content).map_err(|e| format!("invalid keybindings file: {e}"))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(toml::Table::new()),
            Err(e) => Err(format!("cannot read {}: {e}", path.display())),
        }
    }

    fn write_user_keybindings_table(&self, table: &toml::Table) -> Result<(), String> {
        std::fs::create_dir_all(&self.root)
            .map_err(|e| format!("cannot create {}: {e}", self.root.display()))?;
        let path = self.root.join("keybindings.toml");
        let serialized =
            toml::to_string_pretty(table).map_err(|e| format!("cannot serialize: {e}"))?;
        std::fs::write(&path, serialized).map_err(|e| format!("cannot write {}: {e}", path.display()))
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

/// Upgrades the never-edited user copies of shipped files: a user file
/// whose content equals the previous defaults mirror (`old_defaults`,
/// written by the last run) carries no user edit and is rewritten with
/// the new shipped content. Anything else — edited, missing, or without
/// a mirror counterpart (first run) — is left to `install_dir`.
fn upgrade_pristine(source: &Dir<'_>, user: &Path, old_defaults: &Path) -> Result<(), String> {
    let io = |e: std::io::Error| format!("config install failed: {e}");
    for dir in source.dirs() {
        upgrade_pristine(dir, user, old_defaults)?;
    }
    for file in source.files() {
        let user_path = user.join(file.path());
        let Ok(current) = std::fs::read(&user_path) else { continue };
        let Ok(previous) = std::fs::read(old_defaults.join(file.path())) else { continue };
        if current == previous && current != file.contents() {
            std::fs::write(&user_path, file.contents()).map_err(io)?;
        }
    }
    Ok(())
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
