//! CLI configuration file (spec-config): `~/.config/metafolder/cli/config.toml`,
//! read once at startup. Mirrors the daemon's optional `[settings]` model: a
//! missing file (or key) keeps the built-in default, a malformed file is an
//! error. `--no-config` skips the file entirely (built-in defaults only), which
//! makes scripts immune to the user's personal tweaks.
//!
//! Precedence for every knob: an explicit CLI flag (or its env var) wins over
//! the config file, which wins over the built-in default.

use std::path::{Path, PathBuf};

use serde::Deserialize;

/// Default daemon port on 127.0.0.1 (spec-main). Also the daemon's own default.
pub const DEFAULT_DAEMON_PORT: u16 = 7523;
/// Default internal pagination page size for `list`/`get`/`query` streaming.
pub const DEFAULT_PAGE_SIZE: usize = 500;
/// Default poll interval (ms) while `mf reconcile` waits for its task.
pub const DEFAULT_RECONCILE_POLL_INTERVAL_MS: u64 = 200;

/// Tunable UX/performance settings (the `[settings]` table). All optional: a
/// missing table or key keeps the default below, so an empty config behaves
/// exactly like no config at all.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "kebab-case", default, deny_unknown_fields)]
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

impl Default for CliSettings {
    fn default() -> Self {
        CliSettings {
            daemon_port: DEFAULT_DAEMON_PORT,
            page_size: DEFAULT_PAGE_SIZE,
            reconcile_poll_interval_ms: DEFAULT_RECONCILE_POLL_INTERVAL_MS,
        }
    }
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
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CliConfig {
    /// Tunable UX/performance settings (`[settings]`).
    pub settings: CliSettings,
    /// The default repository selector (`[repo]`).
    pub repo: RepoDefault,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    #[serde(default)]
    settings: CliSettings,
    #[serde(default)]
    repo: RepoDefault,
}

/// Default configuration file path:
/// `$XDG_CONFIG_HOME/metafolder/cli/config.toml`.
pub fn default_config_path() -> Option<PathBuf> {
    metafolder_core::config::crate_config_dir("cli").map(|dir| dir.join("config.toml"))
}

/// Parses configuration text into a [`CliConfig`].
fn parse_config(contents: &str) -> Result<CliConfig, String> {
    let raw: RawConfig = toml::from_str(contents).map_err(|e| e.to_string())?;
    Ok(CliConfig { settings: raw.settings, repo: raw.repo })
}

/// Reads and validates the configuration file. A missing file is equivalent to
/// an empty one (all defaults); a malformed file is an error.
pub fn read_config(path: &Path) -> Result<CliConfig, String> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(CliConfig::default()),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    parse_config(&contents).map_err(|e| format!("in {}: {e}", path.display()))
}

/// Loads the CLI configuration honouring `--no-config`. With `no_config`, or
/// when no config path can be resolved (no `$HOME`/`$XDG_CONFIG_HOME`), the
/// built-in defaults are returned without touching the filesystem.
pub fn load(no_config: bool) -> Result<CliConfig, String> {
    if no_config {
        return Ok(CliConfig::default());
    }
    match default_config_path() {
        Some(path) => read_config(&path),
        None => Ok(CliConfig::default()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_settings_default_to_the_shipped_values() {
        let empty = CliConfig::default();
        assert_eq!(empty.settings.daemon_port, DEFAULT_DAEMON_PORT);
        assert_eq!(empty.settings.page_size, DEFAULT_PAGE_SIZE);
        assert_eq!(
            empty.settings.reconcile_poll_interval_ms,
            DEFAULT_RECONCILE_POLL_INTERVAL_MS
        );
        assert_eq!(empty.repo, RepoDefault::default());
        // Parsing empty text yields the same defaults.
        assert_eq!(parse_config("").unwrap(), empty);
    }

    #[test]
    fn test_parses_settings_and_repo_tables() {
        let config = parse_config(
            "[settings]\n\
             daemon-port = 9000\n\
             page-size = 42\n\
             reconcile-poll-interval-ms = 750\n\
             [repo]\n\
             name = \"music\"\n",
        )
        .unwrap();
        assert_eq!(config.settings.daemon_port, 9000);
        assert_eq!(config.settings.page_size, 42);
        assert_eq!(config.settings.reconcile_poll_interval_ms, 750);
        assert_eq!(config.repo.name.as_deref(), Some("music"));
        assert_eq!(config.repo.uuid, None);
    }

    #[test]
    fn test_override_one_key_keeps_other_defaults() {
        let config = parse_config("[settings]\npage-size = 10\n").unwrap();
        assert_eq!(config.settings.page_size, 10);
        // Unspecified keys keep their defaults.
        assert_eq!(config.settings.daemon_port, DEFAULT_DAEMON_PORT);
        assert_eq!(
            config.settings.reconcile_poll_interval_ms,
            DEFAULT_RECONCILE_POLL_INTERVAL_MS
        );
    }

    #[test]
    fn test_repo_default_uuid() {
        let config = parse_config("[repo]\nuuid = \"abc\"\n").unwrap();
        assert_eq!(config.repo.uuid.as_deref(), Some("abc"));
        assert_eq!(config.repo.name, None);
    }

    #[test]
    fn test_unknown_key_is_rejected() {
        assert!(parse_config("[settings]\nnope = 1\n").is_err());
        assert!(parse_config("nope = 1\n").is_err());
    }

    #[test]
    fn test_missing_file_yields_defaults() {
        let path = std::env::temp_dir().join(format!(
            "mf_cli_missing_{}.toml",
            uuid::Uuid::new_v4()
        ));
        assert_eq!(read_config(&path).unwrap(), CliConfig::default());
    }

    #[test]
    fn test_malformed_file_is_an_error_naming_the_path() {
        let path = std::env::temp_dir().join(format!(
            "mf_cli_bad_{}.toml",
            uuid::Uuid::new_v4()
        ));
        std::fs::write(&path, "this is = not = valid").unwrap();
        let err = read_config(&path).unwrap_err();
        assert!(err.contains(&path.display().to_string()), "got: {err}");
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn test_no_config_skips_the_file() {
        // `load(true)` never touches the filesystem, even if a config exists.
        assert_eq!(load(true).unwrap(), CliConfig::default());
    }
}
