//! Loading of the global simplified-query grammar (spec-query "Simplified
//! query language" / "Configuration"): `$XDG_CONFIG_HOME/metafolder/
//! query-grammar`, installed with an editable default on first run, parsed and
//! validated at startup. A missing file is installed; a malformed grammar
//! disables the simplified language (the normal DSL is unaffected).

use std::path::{Path, PathBuf};

use metafolder_core::simplified::engine::validate;
use metafolder_core::simplified::grammar::{parse_grammar, Grammar};

/// The default grammar installed on first run (spec-query "Default grammar").
pub const DEFAULT_GRAMMAR: &str = r#"# Simplified query grammar — edit freely; transpiles to the normal DSL.
# Boolean skeleton: space = AND, OR (or +), ! = NOT, parentheses.
query = q:or                       => $q
or    = items:and ++ ("OR" | "+")  => {join(" OR ", $items)}
and   = items:unary +              => {join(" AND ", $items)}
unary = "!" u:unary                => NOT $u
      | a:atom                     => $a
atom  = "(" q:query ")"            => ($q)
      | p:predicate                => $p

# Predicates — the part you edit most of the time.
predicate =
    f:field ":"  v:STRING   => $f = $v
  | f:field ":"  n:NUMBER   => $f = $n
  | f:field ":"  "true"     => $f = true
  | f:field ":"  "false"    => $f = false
  | f:field ":"  w:WORD     => $f = {str($w)}
  | f:field "~"  v:value    => $f MATCHES {str($v)}
  | f:field ">=" n:number   => $f >= $n
  | f:field "?"             => $f IS PRESENT
  | "under" v:value         => mfr_path ->* {str($v)}
  | "tag"                   => tag = "test"
  | "fav"                   => rating >= 4

# Field aliases: map a few names, pass the rest through.
field  = "path" => mfr_path
       | "size" => mfr_size
       | w:WORD => $w
value  = STRING | WORD
number = n:NUMBER "MB"   => {num($n) * 1048576}
       | n:NUMBER "mins" => {num($n) * 60}
       | n:NUMBER        => $n
"#;

/// `$XDG_CONFIG_HOME/metafolder/query-grammar`.
pub fn grammar_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("metafolder").join("query-grammar"))
}

/// Reads the grammar at `path`, installing the default if the file is missing,
/// then parses and validates it.
pub fn load_grammar(path: &Path) -> Result<Grammar, String> {
    let src = match std::fs::read_to_string(path) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| format!("creating {}: {e}", parent.display()))?;
            }
            std::fs::write(path, DEFAULT_GRAMMAR)
                .map_err(|e| format!("installing default grammar at {}: {e}", path.display()))?;
            DEFAULT_GRAMMAR.to_string()
        }
        Err(e) => return Err(format!("reading {}: {e}", path.display())),
    };
    let grammar = parse_grammar(&src)?;
    validate(&grammar)?;
    Ok(grammar)
}

/// The parsed default grammar. Panics only if [`DEFAULT_GRAMMAR`] is malformed,
/// which a unit test guards against; handy for tests and seeding.
pub fn default_grammar() -> Grammar {
    parse_grammar(DEFAULT_GRAMMAR).expect("DEFAULT_GRAMMAR is valid")
}

/// Startup entry point: resolves the path, installs/loads the grammar, and
/// returns `None` (with a logged reason) if it cannot be used.
pub fn init() -> Option<Grammar> {
    let path = grammar_path()?;
    match load_grammar(&path) {
        Ok(g) => Some(g),
        Err(e) => {
            eprintln!("[daemon] Warning: simplified query language disabled: {e}");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use metafolder_core::simplified::engine::expand;

    #[test]
    fn default_grammar_parses_and_validates() {
        let g = parse_grammar(DEFAULT_GRAMMAR).expect("default grammar parses");
        validate(&g).expect("default grammar validates");
    }

    #[test]
    fn default_grammar_expands_examples() {
        let g = parse_grammar(DEFAULT_GRAMMAR).unwrap();
        assert_eq!(expand(&g, "genre:jazz").unwrap(), "genre = \"jazz\"");
        assert_eq!(expand(&g, "rating:4").unwrap(), "rating = 4");
        assert_eq!(expand(&g, "seen:true").unwrap(), "seen = true");
        assert_eq!(expand(&g, "size>=100MB").unwrap(), "mfr_size >= 104857600");
        assert_eq!(
            expand(&g, "fav genre:jazz").unwrap(),
            "rating >= 4 AND genre = \"jazz\""
        );
        assert_eq!(expand(&g, "a:x OR b:y").unwrap(), "a = \"x\" OR b = \"y\"");
    }

    #[test]
    fn load_installs_default_when_missing() {
        let dir = std::env::temp_dir().join(format!("mf-grammar-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("query-grammar");
        let g = load_grammar(&path).expect("installs and loads default");
        assert!(path.exists(), "default grammar file was written");
        let g2 = load_grammar(&path).expect("re-loads the existing file");
        assert_eq!(g, g2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
