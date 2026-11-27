//! Installs/updates the user configuration repository (doc "Configuration").
//!
//! Usage: `metafolder-sync-config [--source <dir> | --defaults <dir>] [--config-dir <dir>]`
//! and `metafolder-sync-config --source <dir> --export <dir>`.
//!
//! The defaults come from a source checkout (`--source`, whose
//! `crates/*/default-config/` are gathered), from a directory laid out as the
//! configuration is (`--defaults`), or else from the one a release installs
//! beside the binary, `<prefix>/share/metafolder/defaults/` (doc "Building a
//! release"). `--export` writes a checkout's defaults to such a directory
//! instead of applying them. `--config-dir` defaults to
//! `$XDG_CONFIG_HOME/metafolder`.

use std::path::PathBuf;
use std::process::ExitCode;

use metafolder_core::config_sync::{
    conflict_help, export_defaults, installed_defaults, sync, sync_defaults,
};

const USAGE: &str =
    "usage: metafolder-sync-config [--source <dir> | --defaults <dir>] [--config-dir <dir>]\n\
       metafolder-sync-config --source <dir> --export <dir>";

fn main() -> ExitCode {
    let mut source: Option<PathBuf> = None;
    let mut defaults: Option<PathBuf> = None;
    let mut export: Option<PathBuf> = None;
    let mut config_dir: Option<PathBuf> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--source" => source = args.next().map(PathBuf::from),
            "--defaults" => defaults = args.next().map(PathBuf::from),
            "--export" => export = args.next().map(PathBuf::from),
            "--config-dir" => config_dir = args.next().map(PathBuf::from),
            "-h" | "--help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("metafolder-sync-config: unexpected argument '{other}'");
                return ExitCode::from(2);
            }
        }
    }

    if source.is_some() && defaults.is_some() {
        eprintln!("metafolder-sync-config: --source and --defaults are exclusive\n{USAGE}");
        return ExitCode::from(2);
    }
    if let Some(out) = export {
        let Some(source) = source else {
            eprintln!("metafolder-sync-config: --export needs --source\n{USAGE}");
            return ExitCode::from(2);
        };
        return match export_defaults(&source, &out) {
            Ok(()) => {
                println!("defaults exported to {}", out.display());
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("metafolder-sync-config: {e}");
                ExitCode::FAILURE
            }
        };
    }
    let config_dir = match config_dir.or_else(metafolder_core::config::config_root) {
        Some(dir) => dir,
        None => {
            eprintln!("metafolder-sync-config: cannot determine the config directory");
            return ExitCode::FAILURE;
        }
    };

    let applied = match (source, defaults) {
        (Some(source), _) => sync(&source, &config_dir),
        (None, Some(defaults)) => sync_defaults(&defaults, &config_dir),
        (None, None) => match std::env::current_exe() {
            Ok(exe) => sync_defaults(&installed_defaults(&exe), &config_dir),
            Err(e) => Err(format!("cannot locate this binary ({e}): give --source or --defaults")),
        },
    };
    match applied {
        Ok(outcome) => {
            if outcome.initialized {
                println!("initialised {}", config_dir.display());
            }
            if let Some(paths) = outcome.conflict {
                eprint!("metafolder-sync-config: {}", conflict_help(&config_dir, &paths));
                eprintln!("Once committed, the configuration is up to date (no re-run needed).");
                return ExitCode::FAILURE;
            }
            if outcome.updated {
                println!("configuration updated");
            } else if !outcome.initialized {
                println!("configuration already up to date");
            }
            if outcome.keybindings_converted {
                println!("gui/keybindings.toml or gui/command-classes.toml converted to the current form");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("metafolder-sync-config: {e}");
            ExitCode::FAILURE
        }
    }
}
