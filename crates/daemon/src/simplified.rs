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
# Boolean skeleton: space or AND = AND, OR (or +), ! = NOT, parentheses.
query = q:or                       => $q
or    = items:and ++ ("OR" | "+")  => {join(" OR ", $items)}
and   = items:andterm +            => {join(" AND ", $items)}
andterm = "AND" u:unary            => $u
        | u:unary                  => $u
unary = "!" u:unary                => NOT $u
      | a:atom                     => $a
atom  = "(" q:query ")"            => ($q)
      | p:predicate                => $p

# Predicates — the part you edit most of the time. Comparison operators mirror
# the normal DSL (=, !=, <, <=, >, >=); ~ is regex match.
predicate =
    f:field "="  v:STRING   => $f = $v
  | f:field "="  n:NUMBER   => $f = $n
  | f:field "="  "true"     => $f = true
  | f:field "="  "false"    => $f = false
  | f:field "="  w:WORD     => $f = {str($w)}
  | f:field "!=" v:STRING   => $f != $v
  | f:field "!=" n:NUMBER   => $f != $n
  | f:field "!=" "true"     => $f != true
  | f:field "!=" "false"    => $f != false
  | f:field "!=" w:WORD     => $f != {str($w)}
  | f:field "~"  v:value    => $f MATCHES {str($v)}
  | f:field ">=" n:number   => $f >= $n
  | f:field "<=" n:number   => $f <= $n
  | f:field ">"  n:number   => $f > $n
  | f:field "<"  n:number   => $f < $n
  | f:field "?"             => $f IS PRESENT
  | "under" v:value         => mfr_path ->* {str($v)}
  # Date macros: `modified`/`created` pick the field; relative (younger/older)
  # and absolute (since/before/between) forms. Dates are quoted ISO-8601.
  | df:datefield "younger" d:duration            => $df > @{now() - num($d)}
  | df:datefield "older"   d:duration            => $df < @{now() - num($d)}
  | df:datefield "since"   v:value               => $df >= @{str($v)}
  | df:datefield "before"  v:value               => $df < @{str($v)}
  | df:datefield "between" a:value "and" b:value => $df >= @{str($a)} AND $df <= @{str($b)}
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

# Date fields and durations (durations are milliseconds).
datefield = "modified" => mfr_mtime
          | "created"  => mfr_btime
duration  = n:NUMBER u:dur_unit => {num($n) * num($u)}
dur_unit  = ("s" | "sec" | "secs" | "second" | "seconds") => 1000
          | ("min" | "mins" | "minute" | "minutes")       => 60000
          | ("h" | "hr" | "hrs" | "hour" | "hours")        => 3600000
          | ("d" | "day" | "days")                         => 86400000
          | ("w" | "week" | "weeks")                       => 604800000
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
        assert_eq!(expand(&g, "genre=jazz").unwrap(), "genre = \"jazz\"");
        assert_eq!(expand(&g, "rating=4").unwrap(), "rating = 4");
        assert_eq!(expand(&g, "seen=true").unwrap(), "seen = true");
        assert_eq!(expand(&g, "size>=100MB").unwrap(), "mfr_size >= 104857600");
        assert_eq!(
            expand(&g, "fav genre=jazz").unwrap(),
            "rating >= 4 AND genre = \"jazz\""
        );
        assert_eq!(expand(&g, "a=x OR b=y").unwrap(), "a = \"x\" OR b = \"y\"");
    }

    #[test]
    fn default_grammar_supports_all_comparison_operators() {
        let g = parse_grammar(DEFAULT_GRAMMAR).unwrap();
        assert_eq!(expand(&g, "rating>3").unwrap(), "rating > 3");
        assert_eq!(expand(&g, "rating<3").unwrap(), "rating < 3");
        assert_eq!(expand(&g, "rating>=3").unwrap(), "rating >= 3");
        assert_eq!(expand(&g, "rating<=3").unwrap(), "rating <= 3");
        assert_eq!(expand(&g, "rating!=3").unwrap(), "rating != 3");
        assert_eq!(expand(&g, "genre!=jazz").unwrap(), "genre != \"jazz\"");
    }

    #[test]
    fn default_grammar_accepts_explicit_and() {
        let g = parse_grammar(DEFAULT_GRAMMAR).unwrap();
        // Juxtaposition and the explicit AND keyword are equivalent.
        let expected = "a = \"x\" AND b = \"y\"";
        assert_eq!(expand(&g, "a=x b=y").unwrap(), expected);
        assert_eq!(expand(&g, "a=x AND b=y").unwrap(), expected);
    }

    #[test]
    fn default_grammar_expands_relative_date_macros() {
        use metafolder_core::simplified::engine::expand_at;
        let g = parse_grammar(DEFAULT_GRAMMAR).unwrap();
        let now = 1_000_000_000_000;
        // younger = more recent than (now - duration); older = before it.
        assert_eq!(
            expand_at(&g, "modified younger 3d", now).unwrap(),
            format!("mfr_mtime > @{}", now - 3 * 86_400_000)
        );
        assert_eq!(
            expand_at(&g, "created older 2 weeks", now).unwrap(),
            format!("mfr_btime < @{}", now - 2 * 604_800_000)
        );
        // Abbreviations and word/number juxtaposition both tokenize alike.
        assert_eq!(
            expand_at(&g, "modified younger 30min", now).unwrap(),
            format!("mfr_mtime > @{}", now - 30 * 60_000)
        );
        assert_eq!(
            expand_at(&g, "modified older 6h", now).unwrap(),
            format!("mfr_mtime < @{}", now - 6 * 3_600_000)
        );
    }

    #[test]
    fn default_grammar_expands_absolute_date_macros() {
        let g = parse_grammar(DEFAULT_GRAMMAR).unwrap();
        assert_eq!(
            expand(&g, "modified since \"2024-01-01\"").unwrap(),
            "mfr_mtime >= @\"2024-01-01\""
        );
        assert_eq!(
            expand(&g, "created before \"2024-06-01\"").unwrap(),
            "mfr_btime < @\"2024-06-01\""
        );
        assert_eq!(
            expand(&g, "created between \"2023-01-01\" and \"2024-01-01\"").unwrap(),
            "mfr_btime >= @\"2023-01-01\" AND mfr_btime <= @\"2024-01-01\""
        );
    }

    #[test]
    fn date_macros_produce_valid_dsl() {
        // The expanded text must be accepted by the normal DSL parser.
        let g = parse_grammar(DEFAULT_GRAMMAR).unwrap();
        let dsl = expand(&g, "modified since \"2024-01-01\"").unwrap();
        metafolder_core::dsl::parse_query(&dsl).expect("expanded date macro is valid DSL");
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
