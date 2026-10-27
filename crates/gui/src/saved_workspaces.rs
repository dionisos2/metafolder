//! Saved workspaces (doc "Saved workspaces"): a workspace saved under a name
//! in its repository's `.metafolder/gui/workspaces/<name>.json` — its layout
//! alone, or the work in progress too — and the name of the repository's
//! default in `.metafolder/gui/default-workspace`.
//!
//! This module is the files only: what a save holds is decided by the panels'
//! declarations (the shell gathers them), and what loading one does to a
//! workspace is [`crate::state::GuiState::apply_saved`].

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The format written; a file of a newer one is refused, not misread
/// (doc "Persisted format changes").
pub const FORMAT_VERSION: u64 = 1;

const DIR: &str = "gui/workspaces";
const DEFAULT_FILE: &str = "gui/default-workspace";
const EXTENSION: &str = "json";
const MAX_NAME_LEN: usize = 128;

/// What a save holds: the layout alone, or the work in progress too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Layout,
    Work,
}

impl Kind {
    pub fn parse(text: &str) -> Result<Kind, String> {
        match text {
            "layout" => Ok(Kind::Layout),
            "work" => Ok(Kind::Work),
            other => Err(format!("not a kind of saved workspace: \"{other}\" (layout / work)")),
        }
    }
}

/// The panel types a workspace shows, and whether with one panel or two.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Slots {
    /// 1 or 2.
    pub shown: u8,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub left: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub right: Option<String>,
}

/// One saved workspace, as its file holds it. The layout variables and the
/// work variables are kept apart, so that the layout of a `work` save is read
/// without its work.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SavedWorkspace {
    pub version: u64,
    pub kind: Kind,
    pub slots: Slots,
    #[serde(default)]
    pub layout: Map<String, Value>,
    /// The work variables; absent from a `layout` save.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work: Option<Map<String, Value>>,
}

/// A name a workspace can be saved under: it is a file name, so no `/`, no
/// leading `.`, no control character, and not empty.
pub fn validate_name(name: &str) -> Result<(), String> {
    let ok = !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && !name.starts_with('.')
        && !name.chars().any(|c| c == '/' || c.is_control());
    if ok {
        Ok(())
    } else {
        Err(format!(
            "invalid name for a saved workspace: {name:?} (no '/', no leading '.', \
             at most {MAX_NAME_LEN} bytes)"
        ))
    }
}

fn file_of(metafolder: &Path, name: &str) -> PathBuf {
    metafolder.join(DIR).join(format!("{name}.{EXTENSION}"))
}

/// Writes `text` to `file` through a temporary file renamed over it, creating
/// its directory.
fn write_atomic(file: &Path, text: &str) -> Result<(), String> {
    let dir = file.parent().expect("a saved-workspace file has a directory");
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    let mut tmp = file.as_os_str().to_owned();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, text).map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, file).map_err(|e| format!("cannot replace {}: {e}", file.display()))
}

/// The text of `file`; `None` when it does not exist.
fn read_text(file: &Path) -> Result<Option<String>, String> {
    match std::fs::read_to_string(file) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("cannot read {}: {e}", file.display())),
    }
}

fn remove(file: &Path) -> Result<bool, String> {
    match std::fs::remove_file(file) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(format!("cannot remove {}: {e}", file.display())),
    }
}

fn parse(file: &Path, text: &str) -> Result<SavedWorkspace, String> {
    // The version first, so that a newer format is named as such rather than
    // reported as whatever field it renamed.
    let value: Value = serde_json::from_str(text)
        .map_err(|e| format!("{} is not a saved workspace: {e}", file.display()))?;
    let version = value["version"].as_u64().unwrap_or(0);
    if version > FORMAT_VERSION {
        return Err(format!(
            "{} is a saved workspace of version {version}; this GUI reads up to version \
             {FORMAT_VERSION}",
            file.display()
        ));
    }
    serde_json::from_value(value)
        .map_err(|e| format!("{} is not a saved workspace: {e}", file.display()))
}

/// The names saved in `metafolder`, with their kind, sorted by name. A file
/// that does not read as a saved workspace is listed with no kind.
pub fn list(metafolder: &Path) -> Result<Vec<(String, Option<Kind>)>, String> {
    let dir = metafolder.join(DIR);
    let entries = match std::fs::read_dir(&dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("cannot read {}: {e}", dir.display())),
    };
    let mut names = Vec::new();
    for entry in entries {
        let path = entry.map_err(|e| format!("cannot read {}: {e}", dir.display()))?.path();
        if path.extension().and_then(|e| e.to_str()) != Some(EXTENSION) {
            continue;
        }
        let Some(name) = path.file_stem().and_then(|s| s.to_str()) else { continue };
        if validate_name(name).is_err() {
            continue;
        }
        let kind = read_text(&path)?.and_then(|text| parse(&path, &text).ok()).map(|s| s.kind);
        names.push((name.to_string(), kind));
    }
    names.sort();
    Ok(names)
}

/// The workspace saved as `name`; `None` when there is none.
pub fn read(metafolder: &Path, name: &str) -> Result<Option<SavedWorkspace>, String> {
    validate_name(name)?;
    let file = file_of(metafolder, name);
    read_text(&file)?.map(|text| parse(&file, &text)).transpose()
}

/// Saves `saved` as `name`, replacing what was saved under it, atomically.
pub fn write(metafolder: &Path, name: &str, saved: &SavedWorkspace) -> Result<(), String> {
    validate_name(name)?;
    let text = serde_json::to_string_pretty(saved).map_err(|e| e.to_string())?;
    write_atomic(&file_of(metafolder, name), &(text + "\n"))
}

/// Deletes the workspace saved as `name` — unsetting the default when it was
/// the default. Whether there was one.
pub fn delete(metafolder: &Path, name: &str) -> Result<bool, String> {
    validate_name(name)?;
    if default_name(metafolder)?.as_deref() == Some(name) {
        unset_default(metafolder)?;
    }
    remove(&file_of(metafolder, name))
}

/// The name of the repository's default workspace, if one is set.
pub fn default_name(metafolder: &Path) -> Result<Option<String>, String> {
    let text = read_text(&metafolder.join(DEFAULT_FILE))?;
    Ok(text.map(|t| t.trim_end_matches('\n').to_string()).filter(|name| !name.is_empty()))
}

/// Makes the workspace saved as `name` the default; refused when there is none.
pub fn set_default(metafolder: &Path, name: &str) -> Result<(), String> {
    validate_name(name)?;
    if !file_of(metafolder, name).exists() {
        return Err(format!("no workspace saved as \"{name}\""));
    }
    write_atomic(&metafolder.join(DEFAULT_FILE), &format!("{name}\n"))
}

/// Leaves the repository without a default. Whether there was one.
pub fn unset_default(metafolder: &Path) -> Result<bool, String> {
    remove(&metafolder.join(DEFAULT_FILE))
}

/// The repository's default workspace: its name and what it holds. A default
/// naming a save that is gone is no default.
pub fn read_default(metafolder: &Path) -> Result<Option<(String, SavedWorkspace)>, String> {
    let Some(name) = default_name(metafolder)? else { return Ok(None) };
    Ok(read(metafolder, &name)?.map(|saved| (name, saved)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A `.metafolder/` of its own, removed on drop — a panicking test included.
    struct TempMetafolder(PathBuf);

    impl TempMetafolder {
        fn new(prefix: &str) -> Self {
            let dir = std::env::temp_dir()
                .join("metafolder-tests")
                .join(format!("metafolder_gui_saved_{prefix}_{}", uuid::Uuid::new_v4()));
            std::fs::create_dir_all(&dir).unwrap();
            TempMetafolder(dir)
        }
    }

    impl Drop for TempMetafolder {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn sample(kind: Kind) -> SavedWorkspace {
        SavedWorkspace {
            version: FORMAT_VERSION,
            kind,
            slots: Slots {
                shown: 2,
                left: Some("metarecord-list".into()),
                right: Some("file".into()),
            },
            layout: json!({"metarecord-list:columns": "name, size"}).as_object().unwrap().clone(),
            work: (kind == Kind::Work)
                .then(|| json!({"metarecord-list:query": "jpg"}).as_object().unwrap().clone()),
        }
    }

    #[test]
    fn test_validate_name_table() {
        for ok in ["photos", "tri des photos", "a-b_c.d", "É", &"x".repeat(MAX_NAME_LEN)] {
            assert!(validate_name(ok).is_ok(), "'{ok}' should be valid");
        }
        for bad in ["", ".hidden", "..", "a/b", "a\nb", "a\0b", &"x".repeat(MAX_NAME_LEN + 1)] {
            assert!(validate_name(bad).is_err(), "'{bad:?}' should be rejected");
        }
    }

    #[test]
    fn test_write_then_read_roundtrip_under_gui_workspaces() {
        let dir = TempMetafolder::new("roundtrip");
        write(&dir.0, "tri", &sample(Kind::Work)).unwrap();
        assert_eq!(read(&dir.0, "tri").unwrap(), Some(sample(Kind::Work)));
        let text = std::fs::read_to_string(dir.0.join("gui/workspaces/tri.json")).unwrap();
        let file: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(file["version"], json!(1));
        assert_eq!(file["kind"], json!("work"));
        assert_eq!(file["work"]["metarecord-list:query"], json!("jpg"));
    }

    #[test]
    fn test_a_layout_save_has_no_work_member() {
        let dir = TempMetafolder::new("layout");
        write(&dir.0, "vue", &sample(Kind::Layout)).unwrap();
        let text = std::fs::read_to_string(dir.0.join("gui/workspaces/vue.json")).unwrap();
        let file: Value = serde_json::from_str(&text).unwrap();
        assert!(file.get("work").is_none(), "{file}");
    }

    #[test]
    fn test_read_missing_is_none_and_a_newer_format_is_refused() {
        let dir = TempMetafolder::new("missing");
        assert_eq!(read(&dir.0, "nothing").unwrap(), None);
        let mut newer = sample(Kind::Layout);
        newer.version = FORMAT_VERSION + 1;
        write(&dir.0, "newer", &newer).unwrap();
        let error = read(&dir.0, "newer").unwrap_err();
        assert!(error.contains("version"), "{error}");
    }

    #[test]
    fn test_an_invalid_name_touches_no_file() {
        let dir = TempMetafolder::new("invalid");
        assert!(write(&dir.0, "../escape", &sample(Kind::Layout)).is_err());
        assert!(!dir.0.join("escape.json").exists());
        assert!(!dir.0.join("gui").exists());
        assert!(read(&dir.0, "a/b").is_err());
    }

    #[test]
    fn test_list_is_sorted_with_kinds_and_skips_other_files() {
        let dir = TempMetafolder::new("list");
        assert_eq!(list(&dir.0).unwrap(), vec![]);
        write(&dir.0, "b", &sample(Kind::Work)).unwrap();
        write(&dir.0, "a", &sample(Kind::Layout)).unwrap();
        std::fs::write(dir.0.join("gui/workspaces/broken.json"), "not json").unwrap();
        std::fs::write(dir.0.join("gui/workspaces/notes.txt"), "x").unwrap();
        assert_eq!(
            list(&dir.0).unwrap(),
            vec![
                ("a".to_string(), Some(Kind::Layout)),
                ("b".to_string(), Some(Kind::Work)),
                ("broken".to_string(), None),
            ]
        );
    }

    #[test]
    fn test_default_set_read_unset() {
        let dir = TempMetafolder::new("default");
        assert_eq!(default_name(&dir.0).unwrap(), None);
        assert!(set_default(&dir.0, "nothing").is_err(), "no such save");
        write(&dir.0, "tri", &sample(Kind::Work)).unwrap();
        set_default(&dir.0, "tri").unwrap();
        assert_eq!(default_name(&dir.0).unwrap(), Some("tri".to_string()));
        assert_eq!(read_default(&dir.0).unwrap(), Some(("tri".to_string(), sample(Kind::Work))));
        assert!(unset_default(&dir.0).unwrap());
        assert!(!unset_default(&dir.0).unwrap());
        assert_eq!(read_default(&dir.0).unwrap(), None);
    }

    #[test]
    fn test_deleting_the_default_unsets_it_and_another_keeps_it() {
        let dir = TempMetafolder::new("delete");
        write(&dir.0, "a", &sample(Kind::Layout)).unwrap();
        write(&dir.0, "b", &sample(Kind::Layout)).unwrap();
        set_default(&dir.0, "a").unwrap();
        assert!(delete(&dir.0, "b").unwrap());
        assert_eq!(default_name(&dir.0).unwrap(), Some("a".to_string()));
        assert!(delete(&dir.0, "a").unwrap());
        assert_eq!(default_name(&dir.0).unwrap(), None);
        assert!(!delete(&dir.0, "a").unwrap());
    }

    #[test]
    fn test_a_default_naming_a_gone_save_is_no_default() {
        let dir = TempMetafolder::new("gone");
        std::fs::create_dir_all(dir.0.join("gui")).unwrap();
        std::fs::write(dir.0.join(DEFAULT_FILE), "gone\n").unwrap();
        assert_eq!(read_default(&dir.0).unwrap(), None);
    }
}
