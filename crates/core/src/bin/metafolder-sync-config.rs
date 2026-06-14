//! Installs/updates the user configuration repository (spec-config).
//!
//! Usage: `metafolder-sync-config [--source <dir>] [--config-dir <dir>]`
//! `--source` defaults to the current directory (a source checkout whose
//! `crates/*/default-config/` are gathered); `--config-dir` defaults to
//! `$XDG_CONFIG_HOME/metafolder`.

use std::path::PathBuf;
use std::process::ExitCode;

use metafolder_core::config_sync::sync;

fn main() -> ExitCode {
    let mut source: Option<PathBuf> = None;
    let mut config_dir: Option<PathBuf> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--source" => source = args.next().map(PathBuf::from),
            "--config-dir" => config_dir = args.next().map(PathBuf::from),
            "-h" | "--help" => {
                println!(
                    "usage: metafolder-sync-config [--source <dir>] [--config-dir <dir>]"
                );
                return ExitCode::SUCCESS;
            }
            other => {
                eprintln!("metafolder-sync-config: unexpected argument '{other}'");
                return ExitCode::from(2);
            }
        }
    }

    let source = source.unwrap_or_else(|| {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    });
    let config_dir = match config_dir.or_else(metafolder_core::config::config_root) {
        Some(dir) => dir,
        None => {
            eprintln!("metafolder-sync-config: cannot determine the config directory");
            return ExitCode::FAILURE;
        }
    };

    match sync(&source, &config_dir) {
        Ok(outcome) => {
            if outcome.initialized {
                println!("initialised {}", config_dir.display());
            }
            if let Some(paths) = outcome.conflict {
                eprintln!(
                    "metafolder-sync-config: merge conflict in {} file(s); \
                     'main' was left untouched. Resolve manually with git, or \
                     revert your edits and re-run.",
                    paths.len()
                );
                for p in paths {
                    eprintln!("  {}", p.display());
                }
                return ExitCode::FAILURE;
            }
            if outcome.updated {
                println!("configuration updated");
            } else if !outcome.initialized {
                println!("configuration already up to date");
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("metafolder-sync-config: {e}");
            ExitCode::FAILURE
        }
    }
}
