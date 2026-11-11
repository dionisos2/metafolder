//! Config directory access in the git-backed model (doc "Configuration"): reading the
//! config.toml/keybindings/stylesheet/panel types installed by
//! `metafolder-sync-config` and the single-file keybinding semantics. There is
//! no runtime install or embedded fallback any more.

use metafolder_gui::config::ConfigDir;
use metafolder_gui::keybindings::{EntryDraft, EntryPlace};

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
    assert!(config.root().join("panel-types/message/index.html").exists());
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
    let err = config.load_keybindings().expect_err("expected an error");
    assert!(err.contains("missing"), "got: {err}");
    assert!(err.contains("metafolder-sync-config"), "got: {err}");
}

#[test]
fn test_load_style_errors_without_config() {
    let (_guard, config) = temp_config();
    assert!(config.load_style().is_err());
}

/// The stylesheet is required like the rest: its absence stops the program
/// before a window exists, with the message on stderr, rather than reaching the
/// shell's failure banner (doc "No runtime fallback").
#[test]
fn test_a_missing_stylesheet_stops_the_gui_before_any_window() {
    let dir = tempfile::tempdir().unwrap();
    let config = ConfigDir::at(dir.path().join("metafolder/gui"));
    common::install_defaults(&config);
    std::fs::remove_file(config.style_css_path()).unwrap();
    // Everything else the GUI requires is there: only the stylesheet can stop it.
    let core = dir.path().join("metafolder/core");
    std::fs::create_dir_all(&core).unwrap();
    std::fs::write(
        core.join("query-grammar"),
        include_str!("../../core/default-config/query-grammar"),
    )
    .unwrap();

    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_metafolder-gui"))
        .arg("--allow-unsandboxed-webview")
        .env("XDG_CONFIG_HOME", dir.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    // A GUI that went past the check would stay up: kill it rather than hang.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while child.try_wait().unwrap().is_none() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let _ = child.kill();
    let output = child.wait_with_output().unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(1), "stderr: {stderr}");
    assert!(stderr.contains("style.css"), "stderr: {stderr}");
}

#[test]
fn test_load_keybindings_reads_the_single_file_as_the_full_set() {
    let (_guard, config) = temp_config();
    common::install_defaults(&config);

    // The shipped file carries the default binding.
    let set = config.load_keybindings().unwrap();
    let t = set.compiled().into_iter().find(|b| b.keys == ["ctrl+t"]).unwrap();
    assert_eq!(t.invocation, "workspace:new self-repo");
}

#[test]
fn test_set_user_keybinding_upserts_and_persists() {
    let (_guard, config) = temp_config();
    common::install_defaults(&config);

    let set = config.set_user_keybinding("alt+t", "panel:split", None, None, false).unwrap();
    let alt_t: Vec<_> = set.compiled().into_iter().filter(|b| b.keys == ["alt+t"]).collect();
    assert_eq!(alt_t.len(), 1);
    assert_eq!(alt_t[0].invocation, "panel:split");

    // Persisted: a fresh load sees the new binding, and the shipped ones remain.
    let reloaded = config.load_keybindings().unwrap();
    let compiled = reloaded.compiled();
    assert!(compiled.iter().any(|b| b.keys == ["alt+t"] && b.invocation == "panel:split"));
    assert!(compiled
        .iter()
        .any(|b| b.keys == ["ctrl+t"] && b.invocation == "workspace:new self-repo"));
}

#[test]
fn test_set_user_keybinding_replaces_differently_spelled_combo() {
    let (_guard, config) = temp_config();
    // Same combo and same scope, spelled differently: the second upsert
    // replaces the first (override is keyed by the normalized combo AND scope).
    config
        .set_user_keybinding("shift+ctrl+a", "first", Some("metarecord-list"), None, true)
        .unwrap();
    let set = config
        .set_user_keybinding("ctrl+shift+A", "second", Some("metarecord-list"), None, true)
        .unwrap();
    let bindings: Vec<_> =
        set.compiled().into_iter().filter(|b| b.keys == ["ctrl+shift+a"]).collect();
    assert_eq!(bindings.len(), 1);
    assert_eq!(bindings[0].invocation, "second");
    assert_eq!(bindings[0].when.as_deref(), Some("metarecord-list"));
    assert!(bindings[0].text_input);
}

#[test]
fn test_remove_user_keybinding_unbinds_the_combo() {
    let (_guard, config) = temp_config();
    common::install_defaults(&config);
    config.set_user_keybinding("ctrl+alt+j", "panel:split", None, None, false).unwrap();

    // In the single-file model, removing unbinds the combo entirely (there is
    // no separate default layer to fall back to).
    let set = config.remove_user_keybinding("ctrl+alt+j", None, None, false).unwrap();
    assert!(set.compiled().iter().all(|b| b.keys != ["ctrl+alt+j"]));
    // Removing a missing combo is a no-op, not an error.
    config.remove_user_keybinding("ctrl+alt+j", None, None, false).unwrap();
}

fn place(source: &str, when: Option<&str>) -> EntryPlace {
    EntryPlace {
        source: source.into(),
        when: when.map(str::to_string),
        focus: None,
        text_input: false,
    }
}

/// The line binding `command` at `place`.
fn line(place: &EntryPlace, command: &str) -> EntryDraft {
    EntryDraft {
        source: place.source.clone(),
        command: command.into(),
        when: place.when.clone(),
        focus: place.focus.clone(),
        text_input: false,
    }
}

const KNOWN: fn(&str) -> bool = |name| name == "panel:split" || name == "workspace:new";

#[test]
fn test_save_keybinding_entry_checks_the_command_and_the_key() {
    let (_guard, config) = temp_config();
    common::install_defaults(&config);
    let current = config.load_keybindings().unwrap();

    // A command nobody registered is refused, and nothing is written.
    let alt_t = place("alt+t", None);
    let error = config
        .save_keybinding_entry(&current, KNOWN, &line(&alt_t, "panel:splt"), None)
        .unwrap_err();
    assert!(error.contains("panel:splt"), "{error}");
    // So is a key already bound in that place (ctrl+t is shipped).
    let error = config
        .save_keybinding_entry(&current, KNOWN, &line(&place("ctrl+t", None), "panel:split"), None)
        .unwrap_err();
    assert!(error.contains("workspace:new"), "{error}");
    let reread = config.load_keybindings().unwrap().compiled();
    assert!(reread.iter().all(|b| b.keys != ["alt+t"]));
    assert!(reread
        .iter()
        .any(|b| b.keys == ["ctrl+t"] && b.invocation == "workspace:new self-repo"));

    let saved =
        config.save_keybinding_entry(&current, KNOWN, &line(&alt_t, "panel:split"), None).unwrap();
    assert!(saved.compiled().iter().any(|b| b.keys == ["alt+t"] && b.invocation == "panel:split"));
}

#[test]
fn test_save_keybinding_entry_replaces_the_edited_line() {
    let (_guard, config) = temp_config();
    common::install_defaults(&config);
    let current = config.load_keybindings().unwrap();

    // Same place: the command changes where it is.
    let ctrl_t = place("ctrl+t", None);
    let set = config
        .save_keybinding_entry(&current, KNOWN, &line(&ctrl_t, "panel:split"), Some(&ctrl_t))
        .unwrap();
    let table = set.compiled();
    let bound: Vec<_> = table.iter().filter(|b| b.keys == ["ctrl+t"]).collect();
    assert_eq!(bound.len(), 1);
    assert_eq!(bound[0].invocation, "panel:split");

    // Another key or scope: the line moves, the old one is gone.
    let moved = line(&place("alt+y", Some("log")), "panel:split");
    let set = config.save_keybinding_entry(&set, KNOWN, &moved, Some(&ctrl_t)).unwrap();
    let table = set.compiled();
    assert!(table.iter().all(|b| b.keys != ["ctrl+t"]));
    assert!(table.iter().any(|b| b.keys == ["alt+y"] && b.when.as_deref() == Some("log")));
}

#[test]
fn test_save_keybinding_entry_overrides_a_suggestion_in_its_place_only() {
    let (_guard, config) = temp_config();
    common::install_defaults(&config);
    let mut current = config.load_keybindings().unwrap();
    current.add_suggestion("ctrl+alt+y", "panel:split", Some("my-panel"), false, None).unwrap();
    let suggested = place("ctrl+alt+y", Some("my-panel"));

    // Moving it elsewhere would leave it in effect: the panel suggests it again.
    let moved = line(&place("ctrl+alt+u", Some("my-panel")), "panel:split");
    let error =
        config.save_keybinding_entry(&current, KNOWN, &moved, Some(&suggested)).unwrap_err();
    assert!(error.contains("suggested"), "{error}");

    // In its own place, the file's binding wins over it.
    let set = config
        .save_keybinding_entry(
            &current,
            KNOWN,
            &line(&suggested, "workspace:new"),
            Some(&suggested),
        )
        .unwrap();
    let table = set.compiled();
    let bound: Vec<_> = table.iter().filter(|b| b.keys == ["ctrl+alt+y"]).collect();
    assert_eq!(bound.len(), 1);
    assert_eq!(bound[0].invocation, "workspace:new");
    assert!(!bound[0].suggested);
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
    assert!(types.contains(&"message".to_string()));
    assert!(types.contains(&"my-custom-panel".to_string()));
    assert!(!types.contains(&"broken".to_string()));
}

#[test]
fn test_panel_dir_resolution() {
    let (_guard, config) = temp_config();
    common::install_defaults(&config);
    assert_eq!(config.panel_dir("message").unwrap(), config.root().join("panel-types/message"));
    assert!(config.panel_dir("nope").is_none());
    // Path traversal in a panel name must not resolve.
    assert!(config.panel_dir("../escape").is_none());
}

// The metarecord-list search-field editing shortcuts (GUI: search commands).
// The finder's Enter leaves the field while Shift+Enter keeps it.
#[test]
fn test_search_field_editing_keybindings() {
    let (_guard, config) = temp_config();
    common::install_defaults(&config);
    let compiled = config.load_keybindings().unwrap().compiled();

    let find = |keys: &[&str]| -> Vec<String> {
        compiled
            .iter()
            .filter(|b| b.keys.iter().map(String::as_str).eq(keys.iter().copied()))
            .map(|b| b.invocation.clone())
            .collect()
    };
    let mlist = |keys: &[&str]| -> Option<String> {
        compiled
            .iter()
            .find(|b| {
                b.keys.iter().map(String::as_str).eq(keys.iter().copied())
                    && b.when.as_deref() == Some("metarecord-list")
            })
            .map(|b| b.invocation.clone())
    };

    // The list's fields are its own actions (doc "Choosing keybindings"):
    // `p` then a verb — `e` edit, shift for edit from empty, `q` the query,
    // `u` empty all three. The finder is `f` (and `/`), shift+f empties it.
    assert_eq!(mlist(&["f"]).as_deref(), Some("metarecord-list:focus finder"));
    assert_eq!(mlist(&["/"]).as_deref(), Some("metarecord-list:focus finder"));
    assert_eq!(mlist(&["shift+f"]).as_deref(), Some("metarecord-list:focus finder clear"));
    assert_eq!(mlist(&["p", "e"]).as_deref(), Some("metarecord-list:focus simplified"));
    assert_eq!(mlist(&["p", "shift+e"]).as_deref(), Some("metarecord-list:focus simplified clear"));
    assert_eq!(mlist(&["p", "q"]).as_deref(), Some("metarecord-list:focus normal"));
    assert_eq!(mlist(&["p", "shift+q"]).as_deref(), Some("metarecord-list:focus normal clear"));
    assert_eq!(mlist(&["p", "u"]).as_deref(), Some("metarecord-list:clear-all"));
    // `p p` (run) applies the fields from outside them, as Enter does inside.
    assert_eq!(mlist(&["p", "p"]).as_deref(), Some("metarecord-list:apply-query"));
    // No single letter of the list is a field operation any more.
    for letter in ["s", "q", "c"] {
        assert!(find(&[letter]).is_empty(), "{letter}: {:?}", find(&[letter]));
    }

    // Finder Enter leaves the field (submit-finder); Shift+Enter re-runs but
    // keeps focus (apply-finder). Both are focus-scoped to the finder input.
    let enter_finder = compiled
        .iter()
        .find(|b| b.keys == ["enter"] && b.focus.as_deref() == Some("finder"))
        .map(|b| b.invocation.clone());
    assert_eq!(enter_finder.as_deref(), Some("metarecord-list:apply-query"));
    let shift_enter_finder = compiled
        .iter()
        .find(|b| b.keys == ["shift+enter"] && b.focus.as_deref() == Some("finder"))
        .map(|b| b.invocation.clone());
    assert_eq!(shift_enter_finder.as_deref(), Some("metarecord-list:apply-query stay"));
}

#[test]
fn test_load_config_errors_without_a_config_file() {
    // Like the keybindings/style, a missing config.toml is an error (no
    // runtime fallback) — metafolder-sync-config must have installed it.
    let (_guard, config) = temp_config();
    assert!(config.load_config().is_err());
}

// The file-manager's two keys that had none: jumping to the root (the disk
// root when no repository is active) and revealing dot-entries.
#[test]
fn test_file_manager_root_and_hidden_keybindings() {
    let (_guard, config) = temp_config();
    common::install_defaults(&config);
    let compiled = config.load_keybindings().unwrap().compiled();

    let in_fm = |keys: &[&str]| -> Option<String> {
        compiled
            .iter()
            .find(|b| {
                b.keys.iter().map(String::as_str).eq(keys.iter().copied())
                    && b.when.as_deref() == Some("file-manager")
            })
            .map(|b| b.invocation.clone())
    };

    // The root is the generic `cursor:root`, bound once for every panel that
    // has a top (doc "Generic panel commands").
    assert!(compiled
        .iter()
        .any(|b| b.keys == ["alt+home"] && b.when.is_none() && b.invocation == "cursor:root self"));
    assert_eq!(in_fm(&["ctrl+h"]).as_deref(), Some("file-manager:toggle hidden"));
    assert_eq!(in_fm(&["v", "h"]).as_deref(), Some("file-manager:toggle hidden"));
    assert_eq!(in_fm(&["v", "r"]).as_deref(), Some("file-manager:toggle root"));
}

// `ctrl+h` starts the help keys inside a text field too, where `h` is text: it
// has its own variable, since `text-input` belongs to a line and not to a value
// of a variable (doc "Keybinding variables").
#[test]
fn test_help_keys_work_while_typing_under_ctrl_h() {
    let (_guard, config) = temp_config();
    common::install_defaults(&config);
    let compiled = config.load_keybindings().unwrap().compiled();
    let global = |keys: &[&str]| {
        compiled
            .iter()
            .find(|b| {
                b.keys.iter().map(String::as_str).eq(keys.iter().copied())
                    && b.when.is_none()
                    && b.focus.is_none()
            })
            .unwrap_or_else(|| panic!("{keys:?} is not bound"))
    };

    for (letter, command) in
        [("h", "help:open"), ("c", "help:cursor"), ("k", "help:key"), ("x", "help:command")]
    {
        let typing = global(&["ctrl+h", letter]);
        assert_eq!(typing.invocation, command);
        assert!(typing.text_input, "ctrl+h {letter} must work in a text field");

        let plain = global(&["h", letter]);
        assert_eq!(plain.invocation, command);
        assert!(!plain.text_input, "h is text in a field");
    }
}

// The `viewer` panel's Backspace: it steps up a level (like the file manager's),
// and mpv's "back to normal speed" keeps the same key with shift.
#[test]
fn test_file_panel_back_keybinding() {
    let (_guard, config) = temp_config();
    common::install_defaults(&config);
    let compiled = config.load_keybindings().unwrap().compiled();

    let in_file = |keys: &[&str]| -> Option<String> {
        compiled
            .iter()
            .find(|b| {
                b.keys.iter().map(String::as_str).eq(keys.iter().copied())
                    && b.when.as_deref() == Some("viewer")
            })
            .map(|b| b.invocation.clone())
    };

    assert_eq!(in_file(&["backspace"]).as_deref(), Some("viewer:back"));
    assert_eq!(in_file(&["shift+backspace"]).as_deref(), Some("viewer:speed reset"));
}

// The global "go" family: repositories, recent metarecords, folders — global,
// acting on the selection and the daemon whichever panel is focused.
#[test]
fn test_go_family_load_and_folder_keybindings() {
    let (_guard, config) = temp_config();
    common::install_defaults(&config);
    let compiled = config.load_keybindings().unwrap().compiled();

    let global = |keys: &[&str]| -> Option<String> {
        compiled
            .iter()
            .find(|b| {
                b.keys.iter().map(String::as_str).eq(keys.iter().copied()) && b.when.is_none()
            })
            .map(|b| b.invocation.clone())
    };

    assert_eq!(global(&["g", "shift+o"]).as_deref(), Some("repos:load on-disk"));
    assert_eq!(global(&["g", "o"]).as_deref(), Some("repos:switch"));
    // A command opening something names its default target, and the same
    // keys after `i` / `b` / `n` open here, beside, in a new workspace (doc
    // "Keybinding variables"). From a picker, here; changing the list itself,
    // here (doc "Choosing keybindings").
    assert_eq!(global(&["g", "r"]).as_deref(), Some("recent here time"));
    assert_eq!(global(&["b", "g", "r"]).as_deref(), Some("recent other time"));
    assert_eq!(global(&["n", "g", "r"]).as_deref(), Some("recent new time"));
    assert_eq!(global(&["g", "shift+r"]).as_deref(), Some("recent here score"));
    assert_eq!(global(&["g", "d"]).as_deref(), Some("folder:open files here"));
    assert_eq!(global(&["g", "l"]).as_deref(), Some("folder:open children here"));
    assert_eq!(global(&["g", "s"]).as_deref(), Some("metarecord-list:folder here"));
    assert_eq!(global(&["b", "g", "s"]).as_deref(), Some("metarecord-list:folder other"));
    // Showing another panel from a row: beside.
    assert_eq!(global(&["g", "m"]).as_deref(), Some("file-manager:reveal other"));
    assert_eq!(global(&["i", "g", "m"]).as_deref(), Some("file-manager:reveal here"));
}
