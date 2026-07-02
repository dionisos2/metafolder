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
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Per-panel list page sizes (spec-gui "metarecord-list / file / file-manager
/// panel types"), read from the `[page-size]` table of `config.toml`. They tune
/// progressive list loading: how many rows/tiles each list panel renders per
/// window before more are fetched on scroll. Smallest for `metarecord-list`
/// (each row needs several daemon round-trips), largest for `file-manager`
/// (a plain text row is the cheapest to build).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct PageSizes {
    pub metarecord_list: u32,
    pub file: u32,
    pub file_manager: u32,
    pub treeref: u32,
    pub ref_list: u32,
}

impl Default for PageSizes {
    fn default() -> Self {
        PageSizes {
            metarecord_list: 100,
            file: 150,
            file_manager: 200,
            treeref: 200,
            ref_list: 100,
        }
    }
}

/// Miscellaneous GUI runtime settings (the `[settings]` table of `config.toml`).
/// These stay Rust-side (they drive background loops and the GUI HTTP server),
/// unlike `[page-size]`/`[panels]`/`[cache]` which are handed to the panels.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct Settings {
    /// How often (seconds) the GUI polls the daemon's health endpoint.
    pub daemon_health_poll_secs: u64,
    /// How often (milliseconds) a running `reconcile:run` polls its task.
    pub reconcile_poll_ms: u64,
    /// How long (seconds) the thumbnail server reuses the fetched repository
    /// list before re-querying the daemon (keeps a thumbnail grid from hitting
    /// `GET /repos` once per tile).
    pub repo_list_cache_ttl_secs: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            daemon_health_poll_secs: 5,
            reconcile_poll_ms: 200,
            repo_list_cache_ttl_secs: 3,
        }
    }
}

/// In-realm daemon-data cache budgets (the `[cache]` table), handed to the
/// frontend cache singleton (LRU eviction beyond each cap).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct CacheSizes {
    /// Max cached metarecords.
    pub max_entities: u32,
    /// Max cached TreeRef paths.
    pub max_tree_refs: u32,
    /// Max cached query results.
    pub max_queries: u32,
}

impl Default for CacheSizes {
    fn default() -> Self {
        CacheSizes { max_entities: 20000, max_tree_refs: 20000, max_queries: 256 }
    }
}

/// UX timing knobs shared by the panels (the `[panels]` table), handed to each
/// panel through the `metafolder` object (`metafolder.settings`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct PanelSettings {
    /// Standard status-message duration (ms) before it auto-clears.
    pub status_message_ms: u32,
    /// Longer status duration (ms) used for errors and important notices.
    pub status_error_ms: u32,
    /// Debounce (ms) of the incremental finder filter in `metarecord-list`.
    pub finder_debounce_ms: u32,
    /// Debounce (ms) of the live edit preview in `metarecord-list`.
    pub live_preview_debounce_ms: u32,
    /// Interval (ms) at which the `repos` panel polls task status.
    pub task_poll_ms: u32,
}

impl Default for PanelSettings {
    fn default() -> Self {
        PanelSettings {
            status_message_ms: 5000,
            status_error_ms: 8000,
            finder_debounce_ms: 500,
            live_preview_debounce_ms: 130,
            task_poll_ms: 1500,
        }
    }
}

/// GUI settings read from `~/.config/metafolder/gui/config.toml` (spec-config;
/// spec-gui "Connection to the daemon"). Missing fields fall back to the
/// defaults below — notably the daemon's own default port, so a fresh install
/// connects without any flag or extra file.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct GuiConfig {
    /// Port of the daemon the GUI talks to (on 127.0.0.1, the only address it
    /// listens on).
    pub daemon_port: u16,
    /// Port the GUI's own HTTP server (panel assets + scripting API) binds.
    pub gui_port: u16,
    /// Per-panel progressive-loading page sizes.
    pub page_size: PageSizes,
    /// Miscellaneous Rust-side runtime settings (`[settings]`).
    pub settings: Settings,
    /// In-realm daemon-data cache budgets (`[cache]`).
    pub cache: CacheSizes,
    /// UX timing knobs shared by the panels (`[panels]`).
    pub panels: PanelSettings,
    /// Per-field-name seed queries for `ref` value pickers (spec-gui "Picker
    /// seeds"), read from the `[picker-seeds]` table: field name → query text
    /// (in the `metarecord-list` query box's syntax, where the seed is injected).
    pub picker_seeds: std::collections::HashMap<String, String>,
}

impl GuiConfig {
    /// The daemon base URL the GUI connects to (loopback + the configured port).
    pub fn daemon_base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.daemon_port)
    }
}

impl Settings {
    /// The daemon health poll interval as a [`std::time::Duration`].
    pub fn daemon_health_poll(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.daemon_health_poll_secs)
    }

    /// The reconcile task poll interval as a [`std::time::Duration`].
    pub fn reconcile_poll(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.reconcile_poll_ms)
    }

    /// The thumbnail repo-list cache TTL as a [`std::time::Duration`].
    pub fn repo_list_cache_ttl(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.repo_list_cache_ttl_secs)
    }
}

impl Default for GuiConfig {
    fn default() -> Self {
        GuiConfig {
            daemon_port: 7523,
            gui_port: 7524,
            page_size: PageSizes::default(),
            settings: Settings::default(),
            cache: CacheSizes::default(),
            panels: PanelSettings::default(),
            picker_seeds: std::collections::HashMap::new(),
        }
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
        focus: Option<&str>,
        text_input: bool,
    ) -> Result<KeybindingSet, String> {
        let normalized = crate::keybindings::parse_combo(combo)?.join(" ");
        let mut table = self.read_user_keybindings_table()?;
        // A combo may already hold several scoped bindings (as an array): collect
        // them, drop the one for this exact (when, focus) scope, then add it back.
        let mut elements = take_combo_elements(&mut table, &normalized);
        elements.retain(|e| !(binding_when(e) == when && binding_focus(e) == focus));
        let mut entry = toml::Table::new();
        entry.insert("command".into(), toml::Value::String(command.to_string()));
        if let Some(when) = when {
            entry.insert("when".into(), toml::Value::String(when.to_string()));
        }
        if let Some(focus) = focus {
            entry.insert("focus".into(), toml::Value::String(focus.to_string()));
        }
        if text_input {
            entry.insert("text-input".into(), toml::Value::Boolean(true));
        }
        elements.push(entry);
        table.insert(normalized, collapse_elements(elements));
        self.write_user_keybindings_table(&table)?;
        self.load_keybindings()
    }

    /// Removes (unbinds) one `(when, focus)`-scoped binding of `combo` from
    /// `keybindings.toml` (other scopes of the same combo are kept); a missing
    /// binding is a no-op. Reverting to a shipped default is a git operation on
    /// the config repo, not handled here (spec-config). Returns the recompiled
    /// set.
    pub fn remove_user_keybinding(
        &self,
        combo: &str,
        when: Option<&str>,
        focus: Option<&str>,
    ) -> Result<KeybindingSet, String> {
        let normalized = crate::keybindings::parse_combo(combo)?.join(" ");
        let mut table = self.read_user_keybindings_table()?;
        let mut elements = take_combo_elements(&mut table, &normalized);
        elements.retain(|e| !(binding_when(e) == when && binding_focus(e) == focus));
        if !elements.is_empty() {
            table.insert(normalized, collapse_elements(elements));
        }
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

/// The `when` scope of one binding element (`None` = global).
fn binding_when(element: &toml::Table) -> Option<&str> {
    element.get("when").and_then(toml::Value::as_str)
}

/// The `focus` scope of one binding element (`None` = not focus-scoped).
fn binding_focus(element: &toml::Table) -> Option<&str> {
    element.get("focus").and_then(toml::Value::as_str)
}

/// Removes every user-file entry whose key normalizes to `normalized` (a combo
/// may be spelled differently, and its value may be a single table or an array)
/// and returns their binding elements as a flat list.
fn take_combo_elements(table: &mut toml::Table, normalized: &str) -> Vec<toml::Table> {
    let keys: Vec<String> = table
        .keys()
        .filter(|k| {
            crate::keybindings::parse_combo(k)
                .map(|ks| ks.join(" ") == normalized)
                .unwrap_or(false)
        })
        .cloned()
        .collect();
    let mut elements = Vec::new();
    for key in keys {
        match table.remove(&key) {
            Some(toml::Value::Table(t)) => elements.push(t),
            Some(toml::Value::Array(arr)) => {
                for v in arr {
                    if let toml::Value::Table(t) = v {
                        elements.push(t);
                    }
                }
            }
            _ => {}
        }
    }
    elements
}

/// A combo's TOML value: a single table when there is one binding, an array of
/// tables when there are several `when`-scoped ones.
fn collapse_elements(mut elements: Vec<toml::Table>) -> toml::Value {
    if elements.len() == 1 {
        toml::Value::Table(elements.pop().unwrap())
    } else {
        toml::Value::Array(elements.into_iter().map(toml::Value::Table).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_defaults_to_the_daemon_default_port() {
        // An empty config still yields the daemon's default port and the GUI
        // default port, so no flag or extra file is needed out of the box.
        let parsed: GuiConfig = toml::from_str("").unwrap();
        assert_eq!(parsed.daemon_port, 7523);
        assert_eq!(parsed.daemon_base_url(), "http://127.0.0.1:7523");
        assert_eq!(parsed.gui_port, 7524);
    }

    #[test]
    fn test_config_overrides_each_field() {
        let parsed: GuiConfig =
            toml::from_str("daemon-port = 9000\ngui-port = 8800\n").unwrap();
        assert_eq!(parsed.daemon_port, 9000);
        assert_eq!(parsed.gui_port, 8800);
    }

    #[test]
    fn test_page_sizes_default_per_panel() {
        // An empty config yields the documented per-panel defaults.
        let parsed: GuiConfig = toml::from_str("").unwrap();
        assert_eq!(parsed.page_size.metarecord_list, 100);
        assert_eq!(parsed.page_size.file, 150);
        assert_eq!(parsed.page_size.file_manager, 200);
    }

    #[test]
    fn test_page_sizes_override_one_panel_keeps_other_defaults() {
        let parsed: GuiConfig = toml::from_str("[page-size]\nfile = 42\n").unwrap();
        assert_eq!(parsed.page_size.file, 42);
        // Unspecified panels keep their defaults.
        assert_eq!(parsed.page_size.metarecord_list, 100);
        assert_eq!(parsed.page_size.file_manager, 200);
    }

    #[test]
    fn test_page_sizes_serialize_with_kebab_panel_keys() {
        // The frontend keys the map by panel-type name, so the JSON must use
        // kebab-case keys matching the panel directory names.
        let json = serde_json::to_value(PageSizes::default()).unwrap();
        assert_eq!(json["metarecord-list"], 100);
        assert_eq!(json["file"], 150);
        assert_eq!(json["file-manager"], 200);
        // The keys must match the panel directory names exactly.
        assert_eq!(json["treeref"], 200);
        assert_eq!(json["ref-list"], 100);
    }

    #[test]
    fn test_settings_default_and_override() {
        let empty: GuiConfig = toml::from_str("").unwrap();
        assert_eq!(empty.settings, Settings::default());
        assert_eq!(empty.settings.daemon_health_poll_secs, 5);
        assert_eq!(empty.settings.reconcile_poll_ms, 200);
        assert_eq!(empty.settings.repo_list_cache_ttl_secs, 3);

        let parsed: GuiConfig =
            toml::from_str("[settings]\ndaemon-health-poll-secs = 12\n").unwrap();
        assert_eq!(parsed.settings.daemon_health_poll_secs, 12);
        // Unspecified keys keep their defaults.
        assert_eq!(parsed.settings.reconcile_poll_ms, 200);
    }

    #[test]
    fn test_cache_sizes_default_and_override() {
        let empty: GuiConfig = toml::from_str("").unwrap();
        assert_eq!(empty.cache, CacheSizes::default());
        assert_eq!(empty.cache.max_entities, 20000);
        assert_eq!(empty.cache.max_queries, 256);

        let parsed: GuiConfig =
            toml::from_str("[cache]\nmax-queries = 1000\n").unwrap();
        assert_eq!(parsed.cache.max_queries, 1000);
        assert_eq!(parsed.cache.max_entities, 20000);
    }

    #[test]
    fn test_panel_settings_default_and_override() {
        let empty: GuiConfig = toml::from_str("").unwrap();
        assert_eq!(empty.panels, PanelSettings::default());
        assert_eq!(empty.panels.finder_debounce_ms, 500);
        assert_eq!(empty.panels.status_error_ms, 8000);

        let parsed: GuiConfig =
            toml::from_str("[panels]\nfinder-debounce-ms = 1000\n").unwrap();
        assert_eq!(parsed.panels.finder_debounce_ms, 1000);
        // Unspecified keys keep their defaults.
        assert_eq!(parsed.panels.live_preview_debounce_ms, 130);
    }

    #[test]
    fn test_panel_settings_serialize_with_kebab_keys() {
        // The frontend reads these keys off `metafolder.settings`; they must be
        // camelCase-friendly kebab keys that the JS maps to camelCase.
        let json = serde_json::to_value(PanelSettings::default()).unwrap();
        assert_eq!(json["finder-debounce-ms"], 500);
        assert_eq!(json["status-error-ms"], 8000);
        assert_eq!(json["task-poll-ms"], 1500);
    }

    #[test]
    fn test_picker_seeds_default_empty_and_parse() {
        let empty: GuiConfig = toml::from_str("").unwrap();
        assert!(empty.picker_seeds.is_empty());

        let parsed: GuiConfig = toml::from_str(
            "[picker-seeds]\ntag = 'type = \"tag\"'\nauthor = 'type = \"person\"'\n",
        )
        .unwrap();
        assert_eq!(parsed.picker_seeds.get("tag").map(String::as_str), Some("type = \"tag\""));
        assert_eq!(parsed.picker_seeds.get("author").map(String::as_str), Some("type = \"person\""));
        assert_eq!(parsed.picker_seeds.get("missing"), None);
    }

    fn kb_dir() -> ConfigDir {
        let dir = std::env::temp_dir().join(format!("mf_gui_kb_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        ConfigDir::at(dir)
    }

    fn scopes_of<'a>(set: &'a KeybindingSet, combo: &[&str]) -> Vec<Option<String>> {
        let mut v: Vec<Option<String>> = set
            .compiled()
            .into_iter()
            .filter(|b| b.keys.iter().map(String::as_str).collect::<Vec<_>>() == combo)
            .map(|b| b.when)
            .collect();
        v.sort();
        v
    }

    #[test]
    fn test_set_keybinding_grows_a_shared_combo_into_an_array() {
        let config = kb_dir();
        std::fs::write(
            config.keybindings_path(),
            "\"j\" = { command = \"file-manager:next\", when = \"file-manager\" }\n",
        )
        .unwrap();

        let set = config
            .set_user_keybinding("j", "metarecord-list:next", Some("metarecord-list"), None, false)
            .unwrap();
        // Both scopes now coexist under `j`.
        assert_eq!(
            scopes_of(&set, &["j"]),
            vec![Some("file-manager".into()), Some("metarecord-list".into())]
        );
        // Persisted as an array, so a re-read keeps both.
        let reread = config.load_keybindings().unwrap();
        assert_eq!(scopes_of(&reread, &["j"]).len(), 2);
        std::fs::remove_dir_all(config.root()).unwrap();
    }

    #[test]
    fn test_set_keybinding_replaces_only_the_same_scope() {
        let config = kb_dir();
        std::fs::write(
            config.keybindings_path(),
            "\"j\" = [\n  { command = \"metarecord-list:next\", when = \"metarecord-list\" },\n  { command = \"file-manager:next\", when = \"file-manager\" },\n]\n",
        )
        .unwrap();

        let set = config
            .set_user_keybinding("j", "metarecord-list:custom", Some("metarecord-list"), None, false)
            .unwrap();
        let js: Vec<_> = set.compiled().into_iter().filter(|b| b.keys == ["j"]).collect();
        assert_eq!(js.len(), 2);
        let mlist = js.iter().find(|b| b.when.as_deref() == Some("metarecord-list")).unwrap();
        assert_eq!(mlist.invocation, "metarecord-list:custom");
        let fm = js.iter().find(|b| b.when.as_deref() == Some("file-manager")).unwrap();
        assert_eq!(fm.invocation, "file-manager:next");
        std::fs::remove_dir_all(config.root()).unwrap();
    }

    #[test]
    fn test_remove_keybinding_drops_one_scope_and_keeps_the_rest() {
        let config = kb_dir();
        std::fs::write(
            config.keybindings_path(),
            "\"j\" = [\n  { command = \"metarecord-list:next\", when = \"metarecord-list\" },\n  { command = \"file-manager:next\", when = \"file-manager\" },\n]\n",
        )
        .unwrap();

        let set = config.remove_user_keybinding("j", Some("metarecord-list"), None).unwrap();
        assert_eq!(scopes_of(&set, &["j"]), vec![Some("file-manager".into())]);
        // The single remaining binding is persisted (as a table or 1-array).
        let reread = config.load_keybindings().unwrap();
        assert_eq!(scopes_of(&reread, &["j"]), vec![Some("file-manager".into())]);
        std::fs::remove_dir_all(config.root()).unwrap();
    }

    #[test]
    fn test_remove_keybinding_last_scope_removes_the_key() {
        let config = kb_dir();
        std::fs::write(
            config.keybindings_path(),
            "\"j\" = { command = \"metarecord-list:next\", when = \"metarecord-list\" }\n\"t\" = { command = \"tab:new\" }\n",
        )
        .unwrap();

        let set = config.remove_user_keybinding("j", Some("metarecord-list"), None).unwrap();
        assert!(scopes_of(&set, &["j"]).is_empty());
        // A different combo is untouched.
        assert_eq!(scopes_of(&set, &["t"]), vec![None]);
        std::fs::remove_dir_all(config.root()).unwrap();
    }

    #[test]
    fn test_focus_scoped_binding_is_set_and_removed_independently() {
        let config = kb_dir();
        std::fs::write(
            config.keybindings_path(),
            "\"down\" = { command = \"metarecord-list:next\", when = \"metarecord-list\" }\n",
        )
        .unwrap();

        // A focus-scoped binding on the same combo coexists with the when one.
        let set = config
            .set_user_keybinding("down", "metarecord-list:next", None, Some("finder"), false)
            .unwrap();
        let downs: Vec<_> = set.compiled().into_iter().filter(|b| b.keys == ["down"]).collect();
        assert_eq!(downs.len(), 2);
        assert!(downs.iter().any(|b| b.when.as_deref() == Some("metarecord-list") && b.focus.is_none()));
        assert!(downs.iter().any(|b| b.focus.as_deref() == Some("finder") && b.when.is_none()));

        // Removing by focus targets only the focus-scoped binding.
        let set = config.remove_user_keybinding("down", None, Some("finder")).unwrap();
        let downs: Vec<_> = set.compiled().into_iter().filter(|b| b.keys == ["down"]).collect();
        assert_eq!(downs.len(), 1);
        assert_eq!(downs[0].when.as_deref(), Some("metarecord-list"));
        std::fs::remove_dir_all(config.root()).unwrap();
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
        assert_eq!(loaded.daemon_port, 7523);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
