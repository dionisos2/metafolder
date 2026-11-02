//! CLI configuration file (doc "Configuration"): `~/.config/metafolder/cli/config.toml`,
//! read once at startup. As for the daemon, a missing or malformed file, or a
//! missing setting, is an error (doc "No runtime fallback"). `--config <file>`
//! reads that file instead, which makes scripts immune to the user's personal
//! tweaks.
//!
//! Precedence for every knob: an explicit CLI flag (or its env var) wins over
//! the config file.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Tunable UX/performance settings (the `[settings]` table). Every key is
/// required: the defaults live in the shipped file alone.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct CliSettings {
    /// Daemon port used when neither `-p/--port` nor `METAFOLDER_DAEMON_PORT`
    /// is given.
    pub daemon_port: u16,
    /// Internal pagination page size: the CLI follows `next_cursor` and streams
    /// the output, requesting this many rows per round-trip. Larger values mean
    /// fewer round-trips but bigger responses.
    pub page_size: usize,
    /// Poll interval, in milliseconds, while `mf reconcile` waits for its
    /// background task (used when `--poll-interval` is omitted).
    pub reconcile_poll_interval_ms: u64,
}

/// A default repository selector (the `[repo]` table), used when neither
/// `-n/--name` nor `-u/--uuid` (nor their env vars) is given. At most one of
/// the two keys is meaningful; if both are set, `name` is tried first.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
pub struct RepoDefault {
    /// Default repository name (resolved to a UUID via `GET /repos`).
    pub name: Option<String>,
    /// Default repository UUID.
    pub uuid: Option<String>,
}

/// Parsed CLI configuration.
#[derive(Debug, Clone, PartialEq)]
pub struct CliConfig {
    /// Tunable UX/performance settings (`[settings]`).
    pub settings: CliSettings,
    /// The default repository selector (`[repo]`).
    pub repo: RepoDefault,
    /// The tag-model field names (`[tag]`), for `mf tag`.
    pub tag: crate::tag::TagConfig,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    settings: CliSettings,
    #[serde(default)]
    repo: RepoDefault,
    tag: crate::tag::TagConfig,
}

/// Default configuration file path:
/// `$XDG_CONFIG_HOME/metafolder/cli/config.toml`.
pub fn default_config_path() -> Option<PathBuf> {
    metafolder_core::config::crate_config_dir("cli").map(|dir| dir.join("config.toml"))
}

/// Parses configuration text into a [`CliConfig`].
fn parse_config(contents: &str) -> Result<CliConfig, String> {
    let raw: RawConfig = toml::from_str(contents).map_err(|e| e.to_string())?;
    Ok(CliConfig { settings: raw.settings, repo: raw.repo, tag: raw.tag })
}

/// Reads and validates the configuration file. A missing or malformed file
/// is an error.
pub fn read_config(path: &Path) -> Result<CliConfig, String> {
    let contents = metafolder_core::config::read_required(path)?;
    parse_config(&contents).map_err(|e| format!("in {}: {e}", path.display()))
}

/// Loads the CLI configuration: the file `--config` names, else the user's.
pub fn load(explicit: Option<&Path>) -> Result<CliConfig, String> {
    match explicit.map(Path::to_path_buf).or_else(default_config_path) {
        Some(path) => read_config(&path),
        None => Err("cannot determine the configuration file: set HOME or use --config".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHIPPED: &str = include_str!("../default-config/config.toml");

    #[test]
    fn test_the_shipped_file_holds_the_documented_values() {
        let shipped = parse_config(SHIPPED).unwrap();
        assert_eq!(shipped.settings.daemon_port, 7523);
        assert_eq!(shipped.settings.page_size, 500);
        assert_eq!(shipped.settings.reconcile_poll_interval_ms, 200);
        assert_eq!(shipped.repo, RepoDefault::default());
        assert_eq!(shipped.tag.type_field, "mf_schema");
        assert_eq!(shipped.tag.negative, "negative_tag");
    }

    /// No built-in copy of the defaults (doc "No runtime fallback"): a key left
    /// out is an error naming it.
    #[test]
    fn test_a_missing_key_is_an_error_naming_it() {
        for key in ["daemon-port", "page-size", "type-field", "sparse"] {
            let without: String = SHIPPED
                .lines()
                .filter(|l| !l.split('=').next().unwrap().trim().eq(key))
                .collect::<Vec<_>>()
                .join("\n");
            let err = parse_config(&without).expect_err(key);
            assert!(err.contains(key), "{key}: {err}");
        }
        assert!(parse_config("").unwrap_err().contains("settings"));
    }

    #[test]
    fn test_no_default_repository_is_valid() {
        // Nothing in the code stands in for it: without one, a command needing
        // a repository and given none is a usage error.
        assert_eq!(parse_config(SHIPPED).unwrap().repo.name, None);
    }

    #[test]
    fn test_parses_settings_and_repo_tables() {
        let src = SHIPPED
            .replace("daemon-port = 7523", "daemon-port = 9000")
            .replace("page-size = 500", "page-size = 42")
            .replace("reconcile-poll-interval-ms = 200", "reconcile-poll-interval-ms = 750")
            .replace("[repo]\n", "[repo]\nname = \"music\"\n");
        let config = parse_config(&src).unwrap();
        assert_eq!(config.settings.daemon_port, 9000);
        assert_eq!(config.settings.page_size, 42);
        assert_eq!(config.settings.reconcile_poll_interval_ms, 750);
        assert_eq!(config.repo.name.as_deref(), Some("music"));
        assert_eq!(config.repo.uuid, None);
    }

    #[test]
    fn test_repo_default_uuid() {
        let src = SHIPPED.replace("[repo]\n", "[repo]\nuuid = \"abc\"\n");
        let config = parse_config(&src).unwrap();
        assert_eq!(config.repo.uuid.as_deref(), Some("abc"));
        assert_eq!(config.repo.name, None);
    }

    #[test]
    fn test_unknown_key_is_rejected() {
        assert!(parse_config(&SHIPPED.replace("[settings]\n", "[settings]\nnope = 1\n")).is_err());
        assert!(parse_config(&format!("nope = 1\n{SHIPPED}")).is_err());
    }

    #[test]
    fn test_a_missing_file_is_an_error_naming_it() {
        let path = std::env::temp_dir()
            .join("metafolder-tests")
            .join(format!("mf_cli_missing_{}.toml", uuid::Uuid::new_v4()));
        let err = read_config(&path).unwrap_err();
        assert!(err.contains(&path.display().to_string()), "got: {err}");
        assert!(err.contains("metafolder-sync-config"), "got: {err}");
    }

    #[test]
    fn test_malformed_file_is_an_error_naming_the_path() {
        let path = std::env::temp_dir()
            .join("metafolder-tests")
            .join(format!("mf_cli_bad_{}.toml", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "this is = not = valid").unwrap();
        let err = read_config(&path).unwrap_err();
        assert!(err.contains(&path.display().to_string()), "got: {err}");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn test_an_explicit_file_replaces_the_user_configuration() {
        let dir = std::env::temp_dir().join("metafolder-tests");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("mf_cli_explicit_{}.toml", uuid::Uuid::new_v4()));
        std::fs::write(&path, SHIPPED.replace("page-size = 500", "page-size = 7")).unwrap();
        assert_eq!(load(Some(&path)).unwrap().settings.page_size, 7);
        std::fs::remove_file(&path).unwrap();
    }
}
