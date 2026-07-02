//! Config directory access in the git-backed model (spec-config): reading the
//! config.toml/keybindings/stylesheet/panel types installed by
//! `metafolder-sync-config` and the single-file keybinding semantics. There is
//! no runtime install or embedded fallback any more.

use metafolder_gui::config::ConfigDir;

mod common;

fn temp_config() -> (tempfile::TempDir, ConfigDir) {
    let dir = tempfile::tempdir().unwrap();
    let config = ConfigDir::at(dir.path().join("gui"));
    (dir, config)
}

#[test]
fn test_installed_defaults_are_readable() {
    let (_guard, config) = temp_config();
    common::install_defaults(&config);

    assert!(config.root().join("config.toml").exists());
    assert!(config.root().join("keybindings.toml").exists());
    assert!(config.root().join("style.css").exists());
    assert!(config.root().join("panel-types/hello/index.html").exists());
    assert!(config.load_keybindings().is_ok());
    assert!(config.load_style().is_ok());
    // The shipped config defaults to the daemon's own default port.
    let gui_config = config.load_config().unwrap();
    assert_eq!(gui_config.daemon_port, 7523);
    assert_eq!(gui_config.gui_port, 7524);
}

#[test]
fn test_load_keybindings_errors_without_config() {
    let (_guard, config) = temp_config();
    // No install: a missing file is an error, not a silent default.
    let err = config.load_keybindings().err().expect("expected an error");
    assert!(err.contains("missing"), "got: {err}");
    assert!(err.contains("metafolder-sync-config"), "got: {err}");
}

#[test]
fn test_load_style_errors_without_config() {
    let (_guard, config) = temp_config();
    assert!(config.load_style().is_err());
}

#[test]
fn test_load_keybindings_reads_the_single_file_as_the_full_set() {
    let (_guard, config) = temp_config();
    common::install_defaults(&config);

    // The shipped file carries the default binding.
    let set = config.load_keybindings().unwrap();
    let t = set.compiled().into_iter().find(|b| b.keys == ["t"]).unwrap();
    assert_eq!(t.invocation, "tab:new");
}

#[test]
fn test_set_user_keybinding_upserts_and_persists() {
    let (_guard, config) = temp_config();
    common::install_defaults(&config);

    let set = config
        .set_user_keybinding("alt+t", "panel:split", None, None, false)
        .unwrap();
    let alt_t: Vec<_> = set.compiled().into_iter().filter(|b| b.keys == ["alt+t"]).collect();
    assert_eq!(alt_t.len(), 1);
    assert_eq!(alt_t[0].invocation, "panel:split");

    // Persisted: a fresh load sees the new binding, and the shipped ones remain.
    let reloaded = config.load_keybindings().unwrap();
    let compiled = reloaded.compiled();
    assert!(compiled.iter().any(|b| b.keys == ["alt+t"] && b.invocation == "panel:split"));
    assert!(compiled.iter().any(|b| b.keys == ["t"] && b.invocation == "tab:new"));
}

#[test]
fn test_set_user_keybinding_replaces_differently_spelled_combo() {
    let (_guard, config) = temp_config();
    // Same combo and same scope, spelled differently: the second upsert
    // replaces the first (override is keyed by the normalized combo AND scope).
    config
        .set_user_keybinding("shift+ctrl+a", "first", Some("metarecord-list"), None, false)
        .unwrap();
    let set = config
        .set_user_keybinding("ctrl+shift+A", "second", Some("metarecord-list"), None, true)
        .unwrap();
    let bindings: Vec<_> = set
        .compiled()
        .into_iter()
        .filter(|b| b.keys == ["ctrl+shift+a"])
        .collect();
    assert_eq!(bindings.len(), 1);
    assert_eq!(bindings[0].invocation, "second");
    assert_eq!(bindings[0].when.as_deref(), Some("metarecord-list"));
    assert!(bindings[0].text_input);
}

#[test]
fn test_remove_user_keybinding_unbinds_the_combo() {
    let (_guard, config) = temp_config();
    common::install_defaults(&config);
    config
        .set_user_keybinding("t", "panel:split", None, None, false)
        .unwrap();

    // In the single-file model, removing unbinds the combo entirely (there is
    // no separate default layer to fall back to).
    let set = config.remove_user_keybinding("t", None, None).unwrap();
    assert!(set.compiled().iter().all(|b| b.keys != ["t"]));
    // Removing a missing combo is a no-op, not an error.
    config.remove_user_keybinding("t", None, None).unwrap();
}

#[test]
fn test_list_panel_types_includes_custom_directories() {
    let (_guard, config) = temp_config();
    common::install_defaults(&config);

    let custom = config.root().join("panel-types/my-custom-panel");
    std::fs::create_dir_all(&custom).unwrap();
    std::fs::write(custom.join("index.html"), "<html></html>").unwrap();
    // A directory without index.html is not a panel type.
    std::fs::create_dir_all(config.root().join("panel-types/broken")).unwrap();

    let types = config.list_panel_types().unwrap();
    assert!(types.contains(&"hello".to_string()));
    assert!(types.contains(&"my-custom-panel".to_string()));
    assert!(!types.contains(&"broken".to_string()));
}

#[test]
fn test_panel_dir_resolution() {
    let (_guard, config) = temp_config();
    common::install_defaults(&config);
    assert_eq!(
        config.panel_dir("hello").unwrap(),
        config.root().join("panel-types/hello")
    );
    assert!(config.panel_dir("nope").is_none());
    // Path traversal in a panel name must not resolve.
    assert!(config.panel_dir("../escape").is_none());
}

#[test]
fn test_load_config_errors_without_a_config_file() {
    // Like the keybindings/style, a missing config.toml is an error (no
    // runtime fallback) — metafolder-sync-config must have installed it.
    let (_guard, config) = temp_config();
    assert!(config.load_config().is_err());
}
