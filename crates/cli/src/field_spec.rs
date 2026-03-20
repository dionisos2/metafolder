use anyhow::bail;
use metafolder_core::entry::{Field, Value};
use uuid::Uuid;

/// Parses `name:type[=value]` into a `Field`.
///
/// Examples:
///   `path:string=/some/path`
///   `path:string="hello world"`
///   `rating:int=5`
///   `score:float=3.14`
///   `active:bool=true`
///   `path:nothing`
///   `parent:ref=abc123-0000-0000-0000-000000000000`
///   `created:date=2024-01-01`
///   `ts:datetime=2024-01-01T12:00:00Z`
///   `dur:duration=5000`
pub fn parse(spec: &str) -> anyhow::Result<Field> {
    let colon = spec
        .find(':')
        .ok_or_else(|| anyhow::anyhow!("Missing ':' in field spec: {spec}"))?;
    let name = spec[..colon].to_string();
    let rest = &spec[colon + 1..];

    let (type_str, value_part) = match rest.find('=') {
        Some(eq) => (&rest[..eq], Some(&rest[eq + 1..])),
        None => (rest, None),
    };

    let value = match type_str {
        "string" => {
            let raw = value_part
                .ok_or_else(|| anyhow::anyhow!("Missing value for string field: {spec}"))?;
            Value::String(parse_string_value(raw)?)
        }
        "int" => {
            let raw = value_part
                .ok_or_else(|| anyhow::anyhow!("Missing value for int field: {spec}"))?;
            Value::Int(raw.trim().parse()?)
        }
        "float" => {
            let raw = value_part
                .ok_or_else(|| anyhow::anyhow!("Missing value for float field: {spec}"))?;
            Value::Float(raw.trim().parse()?)
        }
        "bool" => {
            let raw = value_part
                .ok_or_else(|| anyhow::anyhow!("Missing value for bool field: {spec}"))?;
            Value::Bool(raw.trim().parse()?)
        }
        "nothing" => {
            if value_part.is_some() {
                bail!("'nothing' type must not have a value: {spec}");
            }
            Value::Nothing
        }
        "ref" => {
            let raw = value_part
                .ok_or_else(|| anyhow::anyhow!("Missing value for ref field: {spec}"))?;
            let s = parse_string_value(raw).unwrap_or_else(|_| raw.trim().to_string());
            Value::Ref(s.parse::<Uuid>()?)
        }
        "date" => {
            let raw = value_part
                .ok_or_else(|| anyhow::anyhow!("Missing value for date field: {spec}"))?;
            Value::Date(parse_string_value(raw).unwrap_or_else(|_| raw.trim().to_string()))
        }
        "datetime" => {
            let raw = value_part
                .ok_or_else(|| anyhow::anyhow!("Missing value for datetime field: {spec}"))?;
            Value::DateTime(parse_string_value(raw).unwrap_or_else(|_| raw.trim().to_string()))
        }
        "duration" => {
            let raw = value_part
                .ok_or_else(|| anyhow::anyhow!("Missing value for duration field: {spec}"))?;
            Value::Duration(raw.trim().parse()?)
        }
        other => bail!("Unknown type '{}' in field spec: {}", other, spec),
    };

    Ok(Field { name, value })
}

/// Parse a string value that may be quoted (`"..."` or `'...'`) or bare.
fn parse_string_value(s: &str) -> anyhow::Result<String> {
    let s = s.trim();
    if s.starts_with('"') || s.starts_with('\'') {
        let quote = s.chars().next().unwrap();
        let mut result = String::new();
        let mut chars = s[1..].chars();
        loop {
            match chars.next() {
                None => bail!("Unterminated string literal"),
                Some('\\') => match chars.next() {
                    Some('n') => result.push('\n'),
                    Some('t') => result.push('\t'),
                    Some('\\') => result.push('\\'),
                    Some('"') => result.push('"'),
                    Some('\'') => result.push('\''),
                    other => bail!("Invalid escape: {other:?}"),
                },
                Some(c) if c == quote => break,
                Some(c) => result.push(c),
            }
        }
        Ok(result)
    } else {
        Ok(s.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_string_bare() {
        let f = parse("path:string=/some/path").unwrap();
        assert_eq!(f.name, "path");
        assert_eq!(f.value, Value::String("/some/path".into()));
    }

    #[test]
    fn test_parse_string_quoted() {
        let f = parse(r#"label:string="hello world""#).unwrap();
        assert_eq!(f.value, Value::String("hello world".into()));
    }

    #[test]
    fn test_parse_int() {
        let f = parse("rating:int=5").unwrap();
        assert_eq!(f.value, Value::Int(5));
    }

    #[test]
    fn test_parse_int_negative() {
        let f = parse("delta:int=-3").unwrap();
        assert_eq!(f.value, Value::Int(-3));
    }

    #[test]
    fn test_parse_float() {
        let f = parse("score:float=3.14").unwrap();
        assert!(matches!(f.value, Value::Float(f) if (f - 3.14).abs() < 1e-9));
    }

    #[test]
    fn test_parse_bool() {
        let f = parse("active:bool=true").unwrap();
        assert_eq!(f.value, Value::Bool(true));
    }

    #[test]
    fn test_parse_nothing() {
        let f = parse("path:nothing").unwrap();
        assert_eq!(f.value, Value::Nothing);
    }

    #[test]
    fn test_parse_ref() {
        let id = Uuid::new_v4();
        let f = parse(&format!("parent:ref={id}")).unwrap();
        assert_eq!(f.value, Value::Ref(id));
    }

    #[test]
    fn test_parse_date() {
        let f = parse("created:date=2024-01-15").unwrap();
        assert_eq!(f.value, Value::Date("2024-01-15".into()));
    }

    #[test]
    fn test_parse_duration() {
        let f = parse("dur:duration=5000").unwrap();
        assert_eq!(f.value, Value::Duration(5000));
    }

    #[test]
    fn test_parse_missing_colon() {
        assert!(parse("badspec").is_err());
    }

    #[test]
    fn test_parse_unknown_type() {
        assert!(parse("x:color=red").is_err());
    }

    #[test]
    fn test_parse_nothing_with_value_fails() {
        assert!(parse("x:nothing=something").is_err());
    }
}
