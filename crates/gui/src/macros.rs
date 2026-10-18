//! `gui/macros.js`, the file `macro:save` writes (doc "Macros").
//!
//! It is the user's as much as the GUI's: a saved macro is a user command,
//! to read and edit like one. So the GUI never rewrites what is there. It adds
//! the new entry at the end of the exported object — before its closing `};`
//! — and refuses a name the file already holds; the entry's text (JavaScript)
//! is the shell's, which knows the steps.

use std::path::Path;

/// `source` with `entry` added at the end of its exported object.
///
/// Refused when `name` is already a key of the file, or when the file no
/// longer ends with the `};` closing the object — an edit the GUI cannot
/// place an entry into without guessing.
pub fn insert_macro(source: &str, name: &str, entry: &str) -> Result<String, String> {
    if holds_key(source, name) {
        return Err(format!("macros.js already holds {name}: pick another name"));
    }
    let body = source.trim_end();
    let Some(head) = body.strip_suffix("};") else {
        return Err("macros.js does not end with the `};` closing its export: \
                    the macro cannot be added at its end"
            .to_string());
    };
    let mut out = String::with_capacity(source.len() + entry.len() + 2);
    out.push_str(head);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(entry.trim_end_matches('\n'));
    out.push('\n');
    out.push_str("};\n");
    Ok(out)
}

/// Whether `name`, quoted either way, opens an entry (`'name':`).
fn holds_key(source: &str, name: &str) -> bool {
    ['\'', '"'].iter().any(|quote| {
        let key = format!("{quote}{name}{quote}");
        source.match_indices(&key).any(|(at, _)| {
            source[at + key.len()..].trim_start_matches([' ', '\t']).starts_with(':')
        })
    })
}

/// Adds `entry` under `name` to the file at `path` (doc "Macros").
pub fn save(path: &Path, name: &str, entry: &str) -> Result<(), String> {
    let source = metafolder_core::config::read_required(path)?;
    let updated = insert_macro(&source, name, entry)?;
    std::fs::write(path, updated).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHIPPED: &str = "// header\nexport default {\n};\n";
    const ENTRY: &str = "  'user:a': {\n    run: async (mf) => {},\n  },\n";

    #[test]
    fn test_an_entry_goes_before_the_closing_brace() {
        let out = insert_macro(SHIPPED, "user:a", ENTRY).unwrap();
        assert_eq!(out, format!("// header\nexport default {{\n{ENTRY}}};\n"));
    }

    #[test]
    fn test_a_second_entry_goes_after_the_first() {
        let once = insert_macro(SHIPPED, "user:a", ENTRY).unwrap();
        let entry_b = ENTRY.replace("user:a", "user:b");
        let twice = insert_macro(&once, "user:b", &entry_b).unwrap();
        assert!(twice.find("'user:a'").unwrap() < twice.find("'user:b'").unwrap());
        assert!(twice.ends_with("  },\n};\n"));
    }

    #[test]
    fn test_what_the_user_wrote_is_kept_as_it_is() {
        let edited =
            "export default {\n  // mine\n  'user:x': { run: (mf) => mf.invoke(['quit']) },\n};";
        let out = insert_macro(edited, "user:a", ENTRY).unwrap();
        assert!(out.starts_with(
            "export default {\n  // mine\n  'user:x': { run: (mf) => mf.invoke(['quit']) },\n"
        ));
    }

    #[test]
    fn test_a_name_the_file_holds_is_refused() {
        let once = insert_macro(SHIPPED, "user:a", ENTRY).unwrap();
        let err = insert_macro(&once, "user:a", ENTRY).unwrap_err();
        assert!(err.contains("already holds user:a"), "{err}");
        // Double quotes and spaces before the colon count too.
        let edited = "export default {\n  \"user:a\" : {},\n};\n";
        assert!(insert_macro(edited, "user:a", ENTRY).is_err());
    }

    #[test]
    fn test_a_name_mentioned_but_not_a_key_is_not_a_clash() {
        let edited = "export default {\n  'user:x': { run: (mf) => mf.invoke(['user:a']) },\n};\n";
        assert!(insert_macro(edited, "user:a", ENTRY).is_ok());
    }

    #[test]
    fn test_a_file_not_ending_with_the_closing_brace_is_refused() {
        let err =
            insert_macro("export default {};\nconsole.log(1);\n", "user:a", ENTRY).unwrap_err();
        assert!(err.contains("does not end with"), "{err}");
    }

    #[test]
    fn test_save_writes_the_file() {
        let dir =
            std::env::temp_dir().join(format!("metafolder-tests/macros-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("macros.js");
        std::fs::write(&path, SHIPPED).unwrap();
        save(&path, "user:a", ENTRY).unwrap();
        assert!(std::fs::read_to_string(&path).unwrap().contains("'user:a'"));
        assert!(save(&path, "user:a", ENTRY).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
