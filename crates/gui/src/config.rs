//! `~/.config/metafolder/gui/` access (spec-config; spec-gui "Panel type
//! system", "Style and theming"): reading the keybindings, stylesheet and
//! panel types that `metafolder-sync-config` installed, plus the port
//! discovery file for scripts.
//!
//! There is no installation or embedded fallback here. The configuration is
//! materialised by `metafolder-sync-config` into the git-backed config repo;
//! at runtime a missing configuration file is an error, never a fall back to a
//! shipped default (spec-config "No runtime fallback").

use crate::keybindings::KeybindingSet;
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// GUI settings read from `~/.config/metafolder/gui/config.toml` (spec-config;
/// spec-gui "Connection to the daemon"). Missing fields fall back to the
/// defaults below — notably the daemon's own default port, so a fresh install
/// connects without any flag or extra file.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct GuiConfig {
    /// Base URL of the daemon the GUI talks to.
    pub daemon_url: String,
    /// Port the GUI's own HTTP server (panel assets + scripting API) binds.
    pub gui_port: u16,
}

impl Default for GuiConfig {
    fn default() -> Self {
        GuiConfig { daemon_url: "http://127.0.0.1:7523".to_string(), gui_port: 7524 }
    }
}

pub struct ConfigDir {
    root: PathBuf,
}

impl ConfigDir {
    /// The real user config dir: `~/.config/metafolder/gui` (respecting
    /// `$XDG_CONFIG_HOME`).
    pub fn default_location() -> Result<Self, String> {
        let root = metafolder_core::config::crate_config_dir("gui")
            .ok_or("cannot determine the user configuration directory")?;
        Ok(ConfigDir { root })
    }

    /// A config dir at an explicit location (tests).
    pub fn at(root: PathBuf) -> Self {
        ConfigDir { root }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    // ── General settings ─────────────────────────────────────────────────

    pub fn config_path(&self) -> PathBuf {
        self.root.join("config.toml")
    }

    /// The GUI settings, read from `config.toml`. A missing file is an error
    /// (spec-config "No runtime fallback"); the shipped default-config file
    /// supplies the defaults, so this normally succeeds once
    /// `metafolder-sync-config` has run.
    pub fn load_config(&self) -> Result<GuiConfig, String> {
        let src = metafolder_core::config::read_required(&self.config_path())?;
        toml::from_str(&src).map_err(|e| format!("invalid GUI config file: {e}"))
    }

    // ── Keybindings ──────────────────────────────────────────────────────

    pub fn keybindings_path(&self) -> PathBuf {
        self.root.join("keybindings.toml")
    }

    /// The complete keybinding set, read from the single user file. A missing
    /// or invalid file is an error (spec-config "No runtime fallback").
    pub fn load_keybindings(&self) -> Result<KeybindingSet, String> {
        let src = metafolder_core::config::read_required(&self.keybindings_path())?;
        KeybindingSet::from_source(&src)
    }

    /// Writes (upserts) one keybinding into `keybindings.toml` and returns the
    /// recompiled set. Combos are matched after normalization, so
    /// `"shift+ctrl+a"` replaces an existing `"ctrl+shift+a"` entry.
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

    /// Removes (unbinds) one combo from `keybindings.toml`; a missing combo is
    /// a no-op. Reverting to a shipped default is a git operation on the config
    /// repo, not handled here (spec-config). Returns the recompiled set.
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
        let path = self.keybindings_path();
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
        let path = self.keybindings_path();
        let serialized =
            toml::to_string_pretty(table).map_err(|e| format!("cannot serialize: {e}"))?;
        std::fs::write(&path, serialized).map_err(|e| format!("cannot write {}: {e}", path.display()))
    }

    // ── Style ────────────────────────────────────────────────────────────

    pub fn style_css_path(&self) -> PathBuf {
        self.root.join("style.css")
    }

    /// The user stylesheet. A missing file is an error (no embedded fallback).
    pub fn load_style(&self) -> Result<String, String> {
        metafolder_core::config::read_required(&self.style_css_path())
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_defaults_to_the_daemon_default_port() {
        // An empty config still yields the daemon's default URL and the GUI
        // default port, so no flag or extra file is needed out of the box.
        let parsed: GuiConfig = toml::from_str("").unwrap();
        assert_eq!(parsed.daemon_url, "http://127.0.0.1:7523");
        assert_eq!(parsed.gui_port, 7524);
    }

    #[test]
    fn test_config_overrides_each_field() {
        let parsed: GuiConfig =
            toml::from_str("daemon-url = \"http://127.0.0.1:9000\"\ngui-port = 8800\n").unwrap();
        assert_eq!(parsed.daemon_url, "http://127.0.0.1:9000");
        assert_eq!(parsed.gui_port, 8800);
    }

    #[test]
    fn test_load_config_errors_when_the_file_is_missing() {
        let dir = std::env::temp_dir().join(format!("mf_gui_cfg_{}", uuid::Uuid::new_v4()));
        let config = ConfigDir::at(dir);
        assert!(config.load_config().is_err());
    }

    #[test]
    fn test_load_config_reads_the_file() {
        let dir = std::env::temp_dir().join(format!("mf_gui_cfg_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.toml"), "gui-port = 7600\n").unwrap();
        let config = ConfigDir::at(dir.clone());
        let loaded = config.load_config().unwrap();
        assert_eq!(loaded.gui_port, 7600);
        // Unspecified field keeps the default.
        assert_eq!(loaded.daemon_url, "http://127.0.0.1:7523");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
