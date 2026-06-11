//! Field spec parser: `name:type[=value]` (spec-data-model "* CLI",
//! "Field specification syntax").

use metafolder_core::entry::Value;
use uuid::Uuid;

/// Parses a CLI field spec into `(field_name, value)`.
pub fn parse_field_spec(spec: &str) -> Result<(String, Value), String> {
    let (name, rest) = spec
        .split_once(':')
        .ok_or_else(|| format!("invalid field spec '{spec}': expected name:type[=value]"))?;
    if name.is_empty() {
        return Err(format!("invalid field spec '{spec}': empty field name"));
    }
    let (vtype, raw) = match rest.split_once('=') {
        Some((t, v)) => (t, Some(v)),
        None => (rest, None),
    };
    let value = match (vtype, raw) {
        ("nothing", None) => Value::Nothing,
        ("nothing", Some(_)) => {
            return Err(format!("invalid field spec '{spec}': 'nothing' takes no value"))
        }
        ("string", Some(v)) => Value::String(unquote(v).to_string()),
        ("int", Some(v)) => {
            Value::Int(v.parse().map_err(|_| format!("invalid int value: '{v}'"))?)
        }
        ("float", Some(v)) => {
            Value::Float(v.parse().map_err(|_| format!("invalid float value: '{v}'"))?)
        }
        ("bool", Some(v)) => match v {
            "true" => Value::Bool(true),
            "false" => Value::Bool(false),
            _ => return Err(format!("invalid bool value: '{v}' (expected true or false)")),
        },
        ("datetime", Some(v)) => Value::DateTime(v.to_string()),
        ("ref", Some(v)) => Value::Ref(parse_uuid(v)?),
        ("refbase", Some(v)) => Value::RefBase(parse_uuid(v)?),
        ("externalref", Some(v)) => {
            let (repo, entry) = v.split_once(':').ok_or_else(|| {
                format!("invalid externalref value '{v}': expected <repo_uuid>:<entry_uuid>")
            })?;
            Value::ExternalRef { repo: parse_uuid(repo)?, entry: parse_uuid(entry)? }
        }
        ("tree_ref", Some(v)) => {
            let (parent, leaf) = v.split_once('/').ok_or_else(|| {
                format!("invalid tree_ref value '{v}': expected <parent_uuid>/<name> or /<name>")
            })?;
            if leaf.is_empty() || leaf.contains('/') {
                return Err(format!(
                    "invalid tree_ref name '{leaf}': must be a single non-empty path component"
                ));
            }
            let parent = if parent.is_empty() { None } else { Some(parse_uuid(parent)?) };
            Value::TreeRef { parent, name: leaf.to_string() }
        }
        (other, None) => {
            return Err(format!(
                "invalid field spec '{spec}': missing '=value' for type '{other}'"
            ))
        }
        (other, Some(_)) => return Err(format!("unknown value type '{other}'")),
    };
    Ok((name.to_string(), value))
}

fn parse_uuid(s: &str) -> Result<Uuid, String> {
    Uuid::parse_str(s).map_err(|_| format!("invalid UUID: '{s}'"))
}

/// Strips one pair of surrounding double quotes, if present (the spec allows
/// `genre:string="hard bop"`).
fn unquote(v: &str) -> &str {
    if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
        &v[1..v.len() - 1]
    } else {
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    fn ok(spec: &str) -> (String, Value) {
        parse_field_spec(spec).unwrap_or_else(|e| panic!("'{spec}' should parse: {e}"))
    }

    fn err(spec: &str) -> String {
        parse_field_spec(spec).expect_err(&format!("'{spec}' should be rejected"))
    }

    // ── string ───────────────────────────────────────────────────────────────

    #[test]
    fn test_string_unquoted() {
        assert_eq!(ok("genre:string=jazz"), ("genre".into(), Value::String("jazz".into())));
    }

    #[test]
    fn test_string_quoted() {
        assert_eq!(
            ok(r#"genre:string="hard bop""#),
            ("genre".into(), Value::String("hard bop".into()))
        );
    }

    #[test]
    fn test_string_value_may_contain_equals_and_colons() {
        assert_eq!(ok("note:string=a=b:c"), ("note".into(), Value::String("a=b:c".into())));
    }

    #[test]
    fn test_string_empty_value() {
        assert_eq!(ok("note:string="), ("note".into(), Value::String("".into())));
    }

    // ── nothing ──────────────────────────────────────────────────────────────

    #[test]
    fn test_nothing() {
        assert_eq!(ok("rating:nothing"), ("rating".into(), Value::Nothing));
    }

    #[test]
    fn test_nothing_rejects_value() {
        err("rating:nothing=5");
    }

    // ── int / float / bool / datetime ────────────────────────────────────────

    #[test]
    fn test_int() {
        assert_eq!(ok("rating:int=5"), ("rating".into(), Value::Int(5)));
    }

    #[test]
    fn test_int_negative() {
        assert_eq!(ok("rating:int=-3"), ("rating".into(), Value::Int(-3)));
    }

    #[test]
    fn test_int_invalid() {
        err("rating:int=abc");
    }

    #[test]
    fn test_float() {
        assert_eq!(ok("score:float=3.14"), ("score".into(), Value::Float(3.14)));
    }

    #[test]
    fn test_float_invalid() {
        err("score:float=x");
    }

    #[test]
    fn test_bool_true_false() {
        assert_eq!(ok("seen:bool=true"), ("seen".into(), Value::Bool(true)));
        assert_eq!(ok("seen:bool=false"), ("seen".into(), Value::Bool(false)));
    }

    #[test]
    fn test_bool_invalid() {
        err("seen:bool=yes");
    }

    #[test]
    fn test_datetime() {
        assert_eq!(
            ok("added:datetime=2024-01-01T12:00:00Z"),
            ("added".into(), Value::DateTime("2024-01-01T12:00:00Z".into()))
        );
    }

    // ── ref / refbase / externalref ──────────────────────────────────────────

    #[test]
    fn test_ref() {
        let u = Uuid::parse_str("8f3a2b1c4d5e6f708192a3b4c5d6e7f8").unwrap();
        assert_eq!(
            ok("author:ref=8f3a2b1c4d5e6f708192a3b4c5d6e7f8"),
            ("author".into(), Value::Ref(u))
        );
    }

    #[test]
    fn test_ref_accepts_hyphenated_uuid() {
        let u = Uuid::parse_str("8f3a2b1c4d5e6f708192a3b4c5d6e7f8").unwrap();
        assert_eq!(
            ok("author:ref=8f3a2b1c-4d5e-6f70-8192-a3b4c5d6e7f8"),
            ("author".into(), Value::Ref(u))
        );
    }

    #[test]
    fn test_ref_invalid_uuid() {
        err("author:ref=nope");
    }

    #[test]
    fn test_refbase() {
        let u = Uuid::parse_str("47ab0000000000000000000000000001").unwrap();
        assert_eq!(
            ok("origin:refbase=47ab0000000000000000000000000001"),
            ("origin".into(), Value::RefBase(u))
        );
    }

    #[test]
    fn test_externalref() {
        let repo = Uuid::parse_str("47ab0000000000000000000000000001").unwrap();
        let entry = Uuid::parse_str("8f3a2b1c4d5e6f708192a3b4c5d6e7f8").unwrap();
        assert_eq!(
            ok("link_a:externalref=47ab0000000000000000000000000001:8f3a2b1c4d5e6f708192a3b4c5d6e7f8"),
            ("link_a".into(), Value::ExternalRef { repo, entry })
        );
    }

    #[test]
    fn test_externalref_missing_colon() {
        err("link_a:externalref=47ab0000000000000000000000000001");
    }

    // ── tree_ref ─────────────────────────────────────────────────────────────

    #[test]
    fn test_tree_ref_with_parent() {
        let parent = Uuid::parse_str("8f3a2b1c4d5e6f708192a3b4c5d6e7f8").unwrap();
        assert_eq!(
            ok("parent:tree_ref=8f3a2b1c4d5e6f708192a3b4c5d6e7f8/félins"),
            ("parent".into(), Value::TreeRef { parent: Some(parent), name: "félins".into() })
        );
    }

    #[test]
    fn test_tree_ref_root() {
        assert_eq!(
            ok("parent:tree_ref=/tags"),
            ("parent".into(), Value::TreeRef { parent: None, name: "tags".into() })
        );
    }

    #[test]
    fn test_tree_ref_missing_slash() {
        err("parent:tree_ref=8f3a2b1c4d5e6f708192a3b4c5d6e7f8");
    }

    #[test]
    fn test_tree_ref_bad_parent_uuid() {
        err("parent:tree_ref=zzz/name");
    }

    #[test]
    fn test_tree_ref_name_with_slash_rejected() {
        err("parent:tree_ref=/a/b");
    }

    #[test]
    fn test_tree_ref_empty_name_rejected() {
        err("parent:tree_ref=8f3a2b1c4d5e6f708192a3b4c5d6e7f8/");
    }

    // ── malformed specs ──────────────────────────────────────────────────────

    #[test]
    fn test_missing_colon() {
        err("rating");
    }

    #[test]
    fn test_unknown_type() {
        err("rating:integer=5");
    }

    #[test]
    fn test_missing_value_for_typed_field() {
        err("rating:int");
    }

    #[test]
    fn test_empty_name() {
        err(":int=5");
    }

    #[test]
    fn test_reserved_name_is_accepted_by_the_parser() {
        // Enforcement of mfr_* rules is the daemon's job, not the parser's.
        let (name, _) = ok("mfr_path:tree_ref=/x");
        assert_eq!(name, "mfr_path");
    }
}
