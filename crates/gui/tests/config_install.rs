//! Config directory management: first-run installation of editable
//! defaults (panel types, keybindings, stylesheet), the always-refreshed
//! defaults mirror, keybinding loading, and the port discovery file.

use metafolder_gui::config::ConfigDir;

fn temp_config() -> (tempfile::TempDir, ConfigDir) {
    let dir = tempfile::tempdir().unwrap();
    let config = ConfigDir::at(dir.path().join("metafolder-gui"));
    (dir, config)
}

#[test]
fn test_first_run_installs_defaults() {
    let (_guard, config) = temp_config();
    config.install_defaults().unwrap();

    assert!(config.root().join("keybindings.toml").exists());
    assert!(config.root().join("style.css").exists());
    assert!(config.root().join("panel-types/hello/index.html").exists());
    // The mirror lets users diff their edits against shipped defaults.
    assert!(config
        .root()
        .join("panel-types-defaults/hello/index.html")
        .exists());
}

#[test]
fn test_user_edits_are_never_overwritten() {
    let (_guard, config) = temp_config();
    config.install_defaults().unwrap();

    let keybindings = config.root().join("keybindings.toml");
    let panel = config.root().join("panel-types/hello/index.html");
    std::fs::write(&keybindings, "# my edits\n").unwrap();
    std::fs::write(&panel, "<html>custom</html>").unwrap();

    config.install_defaults().unwrap();
    assert_eq!(std::fs::read_to_string(&keybindings).unwrap(), "# my edits\n");
    assert_eq!(std::fs::read_to_string(&panel).unwrap(), "<html>custom</html>");
}

#[test]
fn test_defaults_mirror_is_always_refreshed() {
    let (_guard, config) = temp_config();
    config.install_defaults().unwrap();

    let mirrored = config.root().join("panel-types-defaults/hello/index.html");
    std::fs::write(&mirrored, "stale").unwrap();
    config.install_defaults().unwrap();
    assert_ne!(std::fs::read_to_string(&mirrored).unwrap(), "stale");
}

#[test]
fn test_load_keybindings_merges_user_over_defaults() {
    let (_guard, config) = temp_config();
    config.install_defaults().unwrap();

    // Defaults only: alt+t is tab:new (shipped default).
    let set = config.load_keybindings().unwrap();
    let table = set.compiled();
    let alt_t = table.iter().find(|b| b.keys == ["alt+t"]).unwrap();
    assert_eq!(alt_t.invocation, "tab:new");

    // User override wins.
    std::fs::write(
        config.root().join("keybindings.toml"),
        r#""alt+t" = { command = "panel:split" }"#,
    )
    .unwrap();
    let set = config.load_keybindings().unwrap();
    let table = set.compiled();
    let alt_t = table.iter().find(|b| b.keys == ["alt+t"]).unwrap();
    assert_eq!(alt_t.invocation, "panel:split");
}

#[test]
fn test_load_keybindings_works_without_user_file() {
    let (_guard, config) = temp_config();
    // No install at all: shipped defaults still load.
    let set = config.load_keybindings().unwrap();
    assert!(set.compiled().iter().any(|b| b.invocation == "tab:new"));
}

#[test]
fn test_list_panel_types_includes_custom_directories() {
    let (_guard, config) = temp_config();
    config.install_defaults().unwrap();

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
    config.install_defaults().unwrap();
    assert_eq!(
        config.panel_dir("hello").unwrap(),
        config.root().join("panel-types/hello")
    );
    assert!(config.panel_dir("nope").is_none());
    // Path traversal in a panel name must not resolve.
    assert!(config.panel_dir("../escape").is_none());
}

#[test]
fn test_port_file_roundtrip() {
    let (_guard, config) = temp_config();
    let path = config.write_port_file(7524).unwrap();
    assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), "7524");
    config.remove_port_file();
    assert!(!path.exists());
}
