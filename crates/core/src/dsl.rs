//! Query DSL: hand-written lexer + recursive-descent parser compiling the
//! human-friendly predicate syntax to the `Query` JSON IR (spec-query
//! "* CLI", "Query DSL").

use crate::record::Value;
use crate::query::{FollowTarget, Query};

/// Parses a DSL predicate string into a `Query`.
pub fn parse_query(input: &str) -> Result<Query, String> {
    let tokens = lex(input)?;
    let mut parser = Parser { tokens, pos: 0 };
    let query = parser.or_expr()?;
    match parser.peek() {
        None => Ok(query),
        Some(tok) => Err(format!("unexpected trailing input: {}", describe(tok))),
    }
}

// ── Lexer ─────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    LParen,
    RParen,
    Arrow,     // ->
    ArrowStar, // ->*
    Eq,
    Neq,
    Lt,
    Lte,
    Gt,
    Gte,
    Str(String),
    Int(i64),
    Float(f64),
    Ident(String),
    // Keywords (case-sensitive uppercase, except the boolean literals).
    And,
    Or,
    Not,
    Is,
    Present,
    Absent,
    Unknown,
    Matches,
    True,
    False,
}

fn describe(tok: &Tok) -> String {
    match tok {
        Tok::LParen => "'('".into(),
        Tok::RParen => "')'".into(),
        Tok::Arrow => "'->'".into(),
        Tok::ArrowStar => "'->*'".into(),
        Tok::Eq => "'='".into(),
        Tok::Neq => "'!='".into(),
        Tok::Lt => "'<'".into(),
        Tok::Lte => "'<='".into(),
        Tok::Gt => "'>'".into(),
        Tok::Gte => "'>='".into(),
        Tok::Str(s) => format!("string \"{s}\""),
        Tok::Int(n) => format!("number {n}"),
        Tok::Float(f) => format!("number {f}"),
        Tok::Ident(name) => format!("identifier '{name}'"),
        Tok::And => "'AND'".into(),
        Tok::Or => "'OR'".into(),
        Tok::Not => "'NOT'".into(),
        Tok::Is => "'IS'".into(),
        Tok::Present => "'PRESENT'".into(),
        Tok::Absent => "'ABSENT'".into(),
        Tok::Unknown => "'UNKNOWN'".into(),
        Tok::Matches => "'MATCHES'".into(),
        Tok::True => "'true'".into(),
        Tok::False => "'false'".into(),
    }
}

fn lex(input: &str) -> Result<Vec<Tok>, String> {
    let chars: Vec<char> = input.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            c if c.is_whitespace() => i += 1,
            '(' => {
                tokens.push(Tok::LParen);
                i += 1;
            }
            ')' => {
                tokens.push(Tok::RParen);
                i += 1;
            }
            '=' => {
                tokens.push(Tok::Eq);
                i += 1;
            }
            '!' => {
                if chars.get(i + 1) == Some(&'=') {
                    tokens.push(Tok::Neq);
                    i += 2;
                } else {
                    return Err("expected '=' after '!'".into());
                }
            }
            '<' => {
                if chars.get(i + 1) == Some(&'=') {
                    tokens.push(Tok::Lte);
                    i += 2;
                } else {
                    tokens.push(Tok::Lt);
                    i += 1;
                }
            }
            '>' => {
                if chars.get(i + 1) == Some(&'=') {
                    tokens.push(Tok::Gte);
                    i += 2;
                } else {
                    tokens.push(Tok::Gt);
                    i += 1;
                }
            }
            '-' => {
                if chars.get(i + 1) == Some(&'>') {
                    if chars.get(i + 2) == Some(&'*') {
                        tokens.push(Tok::ArrowStar);
                        i += 3;
                    } else {
                        tokens.push(Tok::Arrow);
                        i += 2;
                    }
                } else if chars.get(i + 1).is_some_and(|c| c.is_ascii_digit()) {
                    tokens.push(lex_number(&chars, &mut i)?);
                } else {
                    return Err("expected '>' or a digit after '-'".into());
                }
            }
            '"' => tokens.push(lex_string(&chars, &mut i)?),
            c if c.is_ascii_digit() => tokens.push(lex_number(&chars, &mut i)?),
            c if c.is_alphabetic() || c == '_' => tokens.push(lex_word(&chars, &mut i)),
            other => return Err(format!("unexpected character '{other}'")),
        }
    }
    Ok(tokens)
}

fn lex_string(chars: &[char], i: &mut usize) -> Result<Tok, String> {
    let mut out = String::new();
    *i += 1; // opening quote
    while *i < chars.len() {
        match chars[*i] {
            '"' => {
                *i += 1;
                return Ok(Tok::Str(out));
            }
            '\\' => {
                let escaped = chars
                    .get(*i + 1)
                    .ok_or_else(|| "unterminated string literal".to_string())?;
                match escaped {
                    '"' | '\\' => out.push(*escaped),
                    other => return Err(format!("unsupported escape '\\{other}'")),
                }
                *i += 2;
            }
            c => {
                out.push(c);
                *i += 1;
            }
        }
    }
    Err("unterminated string literal".into())
}

fn lex_number(chars: &[char], i: &mut usize) -> Result<Tok, String> {
    let start = *i;
    if chars[*i] == '-' {
        *i += 1;
    }
    while *i < chars.len() && chars[*i].is_ascii_digit() {
        *i += 1;
    }
    let mut is_float = false;
    if *i < chars.len() && chars[*i] == '.' {
        is_float = true;
        *i += 1;
        while *i < chars.len() && chars[*i].is_ascii_digit() {
            *i += 1;
        }
    }
    let text: String = chars[start..*i].iter().collect();
    if is_float {
        text.parse().map(Tok::Float).map_err(|_| format!("invalid number: '{text}'"))
    } else {
        text.parse().map(Tok::Int).map_err(|_| format!("invalid number: '{text}'"))
    }
}

fn lex_word(chars: &[char], i: &mut usize) -> Tok {
    let start = *i;
    while *i < chars.len() && (chars[*i].is_alphanumeric() || chars[*i] == '_') {
        *i += 1;
    }
    let word: String = chars[start..*i].iter().collect();
    match word.as_str() {
        "AND" => Tok::And,
        "OR" => Tok::Or,
        "NOT" => Tok::Not,
        "IS" => Tok::Is,
        "PRESENT" => Tok::Present,
        "ABSENT" => Tok::Absent,
        "UNKNOWN" => Tok::Unknown,
        "MATCHES" => Tok::Matches,
        "true" => Tok::True,
        "false" => Tok::False,
        _ => Tok::Ident(word),
    }
}

// ── Parser ────────────────────────────────────────────────────────────────────

struct Parser {
    tokens: Vec<Tok>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.tokens.get(self.pos)
    }

    fn next(&mut self) -> Option<Tok> {
        let tok = self.tokens.get(self.pos).cloned();
        if tok.is_some() {
            self.pos += 1;
        }
        tok
    }

    fn expect(&mut self, expected: Tok) -> Result<(), String> {
        match self.next() {
            Some(tok) if tok == expected => Ok(()),
            Some(tok) => Err(format!("expected {}, got {}", describe(&expected), describe(&tok))),
            None => Err(format!("expected {}, got end of input", describe(&expected))),
        }
    }

    fn or_expr(&mut self) -> Result<Query, String> {
        let mut operands = vec![self.and_expr()?];
        while self.peek() == Some(&Tok::Or) {
            self.next();
            operands.push(self.and_expr()?);
        }
        Ok(if operands.len() == 1 {
            operands.pop().expect("one operand")
        } else {
            Query::Or { operands }
        })
    }

    fn and_expr(&mut self) -> Result<Query, String> {
        let mut operands = vec![self.unary()?];
        while self.peek() == Some(&Tok::And) {
            self.next();
            operands.push(self.unary()?);
        }
        Ok(if operands.len() == 1 {
            operands.pop().expect("one operand")
        } else {
            Query::And { operands }
        })
    }

    fn unary(&mut self) -> Result<Query, String> {
        if self.peek() == Some(&Tok::Not) {
            self.next();
            Ok(Query::Not { operand: Box::new(self.unary()?) })
        } else {
            self.atom()
        }
    }

    fn atom(&mut self) -> Result<Query, String> {
        if self.peek() == Some(&Tok::LParen) {
            self.next();
            let query = self.or_expr()?;
            self.expect(Tok::RParen)?;
            Ok(query)
        } else {
            self.predicate()
        }
    }

    fn predicate(&mut self) -> Result<Query, String> {
        let field = match self.next() {
            Some(Tok::Ident(name)) => name,
            Some(tok) => return Err(format!("expected a field name, got {}", describe(&tok))),
            None => return Err("expected a field name, got end of input".into()),
        };
        match self.next() {
            Some(Tok::Is) => match self.next() {
                Some(Tok::Present) => Ok(Query::IsPresent { field }),
                Some(Tok::Absent) => Ok(Query::IsAbsent { field }),
                Some(Tok::Unknown) => Ok(Query::IsUnknown { field }),
                Some(tok) => Err(format!(
                    "expected PRESENT, ABSENT or UNKNOWN after IS, got {}",
                    describe(&tok)
                )),
                None => Err("expected PRESENT, ABSENT or UNKNOWN after IS".into()),
            },
            Some(Tok::Eq) => Ok(Query::Eq { field, value: self.literal()? }),
            Some(Tok::Neq) => Ok(Query::Neq { field, value: self.literal()? }),
            Some(Tok::Lt) => Ok(Query::Lt { field, value: self.literal()? }),
            Some(Tok::Lte) => Ok(Query::Lte { field, value: self.literal()? }),
            Some(Tok::Gt) => Ok(Query::Gt { field, value: self.literal()? }),
            Some(Tok::Gte) => Ok(Query::Gte { field, value: self.literal()? }),
            Some(Tok::Matches) => match self.next() {
                Some(Tok::Str(pattern)) => Ok(Query::Matches { field, pattern }),
                Some(tok) => {
                    Err(format!("expected a string after MATCHES, got {}", describe(&tok)))
                }
                None => Err("expected a string after MATCHES".into()),
            },
            Some(Tok::Arrow) => match self.next() {
                Some(Tok::Str(path)) => {
                    Ok(Query::Follows { field, target: FollowTarget::Path(path) })
                }
                Some(Tok::LParen) => {
                    let sub = self.or_expr()?;
                    self.expect(Tok::RParen)?;
                    Ok(Query::Follows { field, target: FollowTarget::Condition(Box::new(sub)) })
                }
                Some(tok) => Err(format!(
                    "expected a path string or a parenthesized query after '->', got {}",
                    describe(&tok)
                )),
                None => Err("expected a path string or a parenthesized query after '->'".into()),
            },
            Some(Tok::ArrowStar) => match self.next() {
                Some(Tok::Str(path)) => Ok(Query::FollowsTransitive { field, path }),
                Some(tok) => {
                    Err(format!("expected a path string after '->*', got {}", describe(&tok)))
                }
                None => Err("expected a path string after '->*'".into()),
            },
            Some(tok) => Err(format!(
                "expected an operator after field '{field}', got {}",
                describe(&tok)
            )),
            None => Err(format!("expected an operator after field '{field}'")),
        }
    }

    fn literal(&mut self) -> Result<Value, String> {
        match self.next() {
            Some(Tok::Int(n)) => Ok(Value::Int(n)),
            Some(Tok::Float(f)) => Ok(Value::Float(f)),
            Some(Tok::Str(s)) => Ok(Value::String(s)),
            Some(Tok::True) => Ok(Value::Bool(true)),
            Some(Tok::False) => Ok(Value::Bool(false)),
            Some(tok) => Err(format!("expected a literal value, got {}", describe(&tok))),
            None => Err("expected a literal value, got end of input".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::Value;
    use crate::query::FollowTarget;

    fn ok(input: &str) -> Query {
        parse_query(input).unwrap_or_else(|e| panic!("'{input}' should parse: {e}"))
    }

    fn err(input: &str) -> String {
        parse_query(input).expect_err(&format!("'{input}' should be rejected"))
    }

    fn eq_int(field: &str, n: i64) -> Query {
        Query::Eq { field: field.into(), value: Value::Int(n) }
    }

    // ── comparisons ──────────────────────────────────────────────────────────

    #[test]
    fn test_gt_int() {
        assert_eq!(ok("rating > 3"), Query::Gt { field: "rating".into(), value: Value::Int(3) });
    }

    #[test]
    fn test_eq_string() {
        assert_eq!(
            ok(r#"genre = "jazz""#),
            Query::Eq { field: "genre".into(), value: Value::String("jazz".into()) }
        );
    }

    #[test]
    fn test_all_comparison_operators() {
        assert_eq!(ok("a = 1"), eq_int("a", 1));
        assert_eq!(ok("a != 1"), Query::Neq { field: "a".into(), value: Value::Int(1) });
        assert_eq!(ok("a < 1"), Query::Lt { field: "a".into(), value: Value::Int(1) });
        assert_eq!(ok("a <= 1"), Query::Lte { field: "a".into(), value: Value::Int(1) });
        assert_eq!(ok("a > 1"), Query::Gt { field: "a".into(), value: Value::Int(1) });
        assert_eq!(ok("a >= 1"), Query::Gte { field: "a".into(), value: Value::Int(1) });
    }

    #[test]
    fn test_float_literal() {
        assert_eq!(ok("score >= 3.5"), Query::Gte { field: "score".into(), value: Value::Float(3.5) });
    }

    #[test]
    fn test_negative_int_literal() {
        assert_eq!(ok("delta < -2"), Query::Lt { field: "delta".into(), value: Value::Int(-2) });
    }

    #[test]
    fn test_bool_literals_lowercase() {
        assert_eq!(ok("seen = true"), Query::Eq { field: "seen".into(), value: Value::Bool(true) });
        assert_eq!(
            ok("seen != false"),
            Query::Neq { field: "seen".into(), value: Value::Bool(false) }
        );
    }

    // ── three-valued predicates ──────────────────────────────────────────────

    #[test]
    fn test_is_present_absent_unknown() {
        assert_eq!(ok("tag IS PRESENT"), Query::IsPresent { field: "tag".into() });
        assert_eq!(ok("mfr_path IS ABSENT"), Query::IsAbsent { field: "mfr_path".into() });
        assert_eq!(ok("rating IS UNKNOWN"), Query::IsUnknown { field: "rating".into() });
    }

    // ── matches ──────────────────────────────────────────────────────────────

    #[test]
    fn test_matches() {
        assert_eq!(
            ok(r#"title MATCHES "[Ll]ive""#),
            Query::Matches { field: "title".into(), pattern: "[Ll]ive".into() }
        );
    }

    #[test]
    fn test_matches_requires_string() {
        err("title MATCHES 5");
    }

    // ── traversal ────────────────────────────────────────────────────────────

    #[test]
    fn test_follows_path() {
        assert_eq!(
            ok(r#"mfr_path -> "/music/jazz""#),
            Query::Follows {
                field: "mfr_path".into(),
                target: FollowTarget::Path("/music/jazz".into()),
            }
        );
    }

    #[test]
    fn test_follows_condition() {
        assert_eq!(
            ok(r#"author -> (name = "Coltrane")"#),
            Query::Follows {
                field: "author".into(),
                target: FollowTarget::Condition(Box::new(Query::Eq {
                    field: "name".into(),
                    value: Value::String("Coltrane".into()),
                })),
            }
        );
    }

    #[test]
    fn test_follows_transitive() {
        assert_eq!(
            ok(r#"mfr_path ->* "/music/jazz""#),
            Query::FollowsTransitive { field: "mfr_path".into(), path: "/music/jazz".into() }
        );
    }

    #[test]
    fn test_follows_transitive_requires_string() {
        err("mfr_path ->* (a = 1)");
    }

    // ── combinators and precedence ───────────────────────────────────────────

    #[test]
    fn test_and_flattens_chain() {
        assert_eq!(
            ok("a = 1 AND b = 2 AND c = 3"),
            Query::And { operands: vec![eq_int("a", 1), eq_int("b", 2), eq_int("c", 3)] }
        );
    }

    #[test]
    fn test_or_binds_looser_than_and() {
        assert_eq!(
            ok("a = 1 OR b = 2 AND c = 3"),
            Query::Or {
                operands: vec![
                    eq_int("a", 1),
                    Query::And { operands: vec![eq_int("b", 2), eq_int("c", 3)] },
                ],
            }
        );
    }

    #[test]
    fn test_not_binds_tighter_than_and() {
        assert_eq!(
            ok("NOT a = 1 AND b = 2"),
            Query::And {
                operands: vec![
                    Query::Not { operand: Box::new(eq_int("a", 1)) },
                    eq_int("b", 2),
                ],
            }
        );
    }

    #[test]
    fn test_parentheses_override_precedence() {
        assert_eq!(
            ok("(a = 1 OR b = 2) AND c = 3"),
            Query::And {
                operands: vec![
                    Query::Or { operands: vec![eq_int("a", 1), eq_int("b", 2)] },
                    eq_int("c", 3),
                ],
            }
        );
    }

    #[test]
    fn test_spec_example_not_parenthesized() {
        assert_eq!(
            ok("NOT (seen = true OR rating IS UNKNOWN)"),
            Query::Not {
                operand: Box::new(Query::Or {
                    operands: vec![
                        Query::Eq { field: "seen".into(), value: Value::Bool(true) },
                        Query::IsUnknown { field: "rating".into() },
                    ],
                }),
            }
        );
    }

    #[test]
    fn test_spec_example_transitive_and_matches() {
        assert_eq!(
            ok(r#"mfr_path ->* "/music/jazz" AND title MATCHES "[Ll]ive""#),
            Query::And {
                operands: vec![
                    Query::FollowsTransitive {
                        field: "mfr_path".into(),
                        path: "/music/jazz".into(),
                    },
                    Query::Matches { field: "title".into(), pattern: "[Ll]ive".into() },
                ],
            }
        );
    }

    #[test]
    fn test_double_not() {
        assert_eq!(
            ok("NOT NOT a = 1"),
            Query::Not {
                operand: Box::new(Query::Not { operand: Box::new(eq_int("a", 1)) }),
            }
        );
    }

    // ── strings ──────────────────────────────────────────────────────────────

    #[test]
    fn test_string_escapes() {
        assert_eq!(
            ok(r#"name = "a\"b\\c""#),
            Query::Eq { field: "name".into(), value: Value::String(r#"a"b\c"#.into()) }
        );
    }

    #[test]
    fn test_unterminated_string() {
        err(r#"name = "oops"#);
    }

    // ── errors ───────────────────────────────────────────────────────────────

    #[test]
    fn test_empty_input() {
        err("");
    }

    #[test]
    fn test_lowercase_keywords_rejected() {
        // Keywords are case-sensitive uppercase: `and` is an identifier here,
        // which makes the input trailing garbage after the first predicate.
        err("a = 1 and b = 2");
    }

    #[test]
    fn test_trailing_garbage() {
        err("a = 1 b");
    }

    #[test]
    fn test_missing_operand() {
        err("a = 1 AND");
    }

    #[test]
    fn test_bare_field_is_not_a_predicate() {
        err("rating");
    }

    #[test]
    fn test_is_requires_present_absent_or_unknown() {
        err("rating IS GREAT");
    }

    #[test]
    fn test_unbalanced_parenthesis() {
        err("(a = 1");
    }
}
