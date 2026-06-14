//! Shared test helper: materialise the shipped defaults into a config dir,
//! the way `metafolder-sync-config` would (a plain copy; no git). Used by the
//! integration tests in place of the removed runtime install step.

use metafolder_gui::config::ConfigDir;
use std::path::Path;

/// Copies `crates/gui/default-config/` into `config.root()`.
pub fn install_defaults(config: &ConfigDir) {
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("default-config");
    copy_tree(&source, config.root());
}

fn copy_tree(src: &Path, dst: &Path) {
    std::fs::create_dir_all(dst).unwrap();
    for entry in std::fs::read_dir(src).unwrap() {
        let entry = entry.unwrap();
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if from.is_dir() {
            copy_tree(&from, &to);
        } else {
            std::fs::copy(&from, &to).unwrap();
        }
    }
}
