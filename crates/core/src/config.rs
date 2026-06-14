//! User configuration paths (spec-config "core configuration API").
//!
//! The user configuration lives in a single git repository at
//! `$XDG_CONFIG_HOME/metafolder/` (or `$HOME/.config/metafolder/`), one
//! subdirectory per crate. Resolution is std-only (no `dirs` crate), in the
//! spirit of the project's dependency-free date handling. Reading is strict:
//! a missing configuration file is an error, never a fall back to a shipped
//! default (the canonical default may legitimately have been deleted).

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// The root of the user configuration repository.
///
/// `$XDG_CONFIG_HOME/metafolder` when `XDG_CONFIG_HOME` holds an absolute
/// path (per the XDG Base Directory spec, relative values are ignored),
/// otherwise `$HOME/.config/metafolder`. `None` when neither is usable.
pub fn config_root() -> Option<PathBuf> {
    config_root_from(
        std::env::var_os("XDG_CONFIG_HOME").as_deref(),
        std::env::var_os("HOME").as_deref(),
    )
}

/// Pure core of [`config_root`], parameterised over the environment for tests.
fn config_root_from(xdg: Option<&OsStr>, home: Option<&OsStr>) -> Option<PathBuf> {
    let base = xdg
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| home.map(|h| PathBuf::from(h).join(".config")))?;
    Some(base.join("metafolder"))
}

/// The configuration directory for one crate: `<config_root>/<crate_name>`.
pub fn crate_config_dir(crate_name: &str) -> Option<PathBuf> {
    config_root().map(|root| crate_dir(&root, crate_name))
}

/// Pure join of a config root and a crate name.
fn crate_dir(root: &Path, crate_name: &str) -> PathBuf {
    root.join(crate_name)
}

/// Reads a required configuration file. A missing file is an explicit error
/// pointing at the install command; there is no fall back to a shipped
/// default (spec-config "No runtime fallback").
pub fn read_required(path: &Path) -> Result<String, String> {
    match std::fs::read_to_string(path) {
        Ok(content) => Ok(content),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(format!(
            "configuration file {} is missing (run metafolder-sync-config)",
            path.display()
        )),
        Err(e) => Err(format!(
            "cannot read configuration file {}: {e}",
            path.display()
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    #[test]
    fn absolute_xdg_config_home_wins() {
        let root = config_root_from(Some(OsStr::new("/x/cfg")), Some(OsStr::new("/home/u")));
        assert_eq!(root, Some(PathBuf::from("/x/cfg/metafolder")));
    }

    #[test]
    fn empty_xdg_falls_back_to_home_dot_config() {
        let root = config_root_from(Some(OsStr::new("")), Some(OsStr::new("/home/u")));
        assert_eq!(root, Some(PathBuf::from("/home/u/.config/metafolder")));
    }

    #[test]
    fn relative_xdg_is_ignored() {
        let root = config_root_from(Some(OsStr::new("rel/cfg")), Some(OsStr::new("/home/u")));
        assert_eq!(root, Some(PathBuf::from("/home/u/.config/metafolder")));
    }

    #[test]
    fn missing_xdg_uses_home() {
        let root = config_root_from(None, Some(OsStr::new("/home/u")));
        assert_eq!(root, Some(PathBuf::from("/home/u/.config/metafolder")));
    }

    #[test]
    fn no_xdg_and_no_home_is_none() {
        assert_eq!(config_root_from(None, None), None);
    }

    #[test]
    fn crate_dir_appends_crate_name() {
        let root = PathBuf::from("/home/u/.config/metafolder");
        assert_eq!(crate_dir(&root, "gui"), PathBuf::from("/home/u/.config/metafolder/gui"));
    }

    #[test]
    fn read_required_reports_missing() {
        let mut path = std::env::temp_dir();
        path.push(format!("metafolder-cfg-missing-{}", std::process::id()));
        path.push("nope.toml");
        let err = read_required(&path).unwrap_err();
        assert!(err.contains("missing"), "got: {err}");
        assert!(err.contains("metafolder-sync-config"), "got: {err}");
    }

    #[test]
    fn read_required_reads_existing() {
        let mut path = std::env::temp_dir();
        path.push(format!("metafolder-cfg-read-{}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        path.push("file.toml");
        std::fs::write(&path, "hello").unwrap();
        assert_eq!(read_required(&path).unwrap(), "hello");
        let _: OsString = path.into_os_string();
    }
}
