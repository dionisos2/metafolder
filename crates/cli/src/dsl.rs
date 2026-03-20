use anyhow::bail;
use metafolder_core::entry::Value;
use metafolder_core::query::Query;

// ── Tokens ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum Token {
    Ident(String),
    Str(String),
    Int(i64),
    Float(f64),
    Bool(bool),
    Eq,       // =
    Neq,      // !=
    Lt,       // <
    Lte,      // <=
    Gt,       // >
    Gte,      // >=
    Arrow,    // ->
    ArrowStar, // ->*
    LParen,   // (
    RParen,   // )
    And,      // AND
    Or,       // OR
    Not,      // NOT
    Is,       // IS
    Present,  // PRESENT
    Absent,   // ABSENT
    Unknown,  // UNKNOWN
    Matches,  // MATCHES
    Eof,
}

// ── Lexer ─────────────────────────────────────────────────────────────────────

struct Lexer {
    input: Vec<char>,
    pos: usize,
}

impl Lexer {
    fn new(input: &str) -> Self {
        Self { input: input.chars().collect(), pos: 0 }
    }

    fn peek_char(&self) -> Option<char> {
        self.input.get(self.pos).copied()
    }

    fn advance_char(&mut self) -> Option<char> {
        let c = self.input.get(self.pos).copied();
        self.pos += 1;
        c
    }

    fn skip_ws(&mut self) {
        while matches!(self.peek_char(), Some(' ' | '\t' | '\n' | '\r')) {
            self.pos += 1;
        }
    }

    fn tokenize(&mut self) -> anyhow::Result<Vec<Token>> {
        let mut tokens = Vec::new();
        loop {
            let tok = self.next_token()?;
            let done = tok == Token::Eof;
            tokens.push(tok);
            if done {
                break;
            }
        }
        Ok(tokens)
    }

    fn next_token(&mut self) -> anyhow::Result<Token> {
        self.skip_ws();
        let c = match self.peek_char() {
            None => return Ok(Token::Eof),
            Some(c) => c,
        };

        match c {
            '(' => {
                self.pos += 1;
                Ok(Token::LParen)
            }
            ')' => {
                self.pos += 1;
                Ok(Token::RParen)
            }
            '=' => {
                self.pos += 1;
                Ok(Token::Eq)
            }
            '!' => {
                self.pos += 1;
                if self.peek_char() == Some('=') {
                    self.pos += 1;
                    Ok(Token::Neq)
                } else {
                    bail!("Expected '=' after '!'")
                }
            }
            '<' => {
                self.pos += 1;
                if self.peek_char() == Some('=') {
                    self.pos += 1;
                    Ok(Token::Lte)
                } else {
                    Ok(Token::Lt)
                }
            }
            '>' => {
                self.pos += 1;
                if self.peek_char() == Some('=') {
                    self.pos += 1;
                    Ok(Token::Gte)
                } else {
                    Ok(Token::Gt)
                }
            }
            '-' => {
                self.pos += 1;
                if self.peek_char() == Some('>') {
                    self.pos += 1;
                    if self.peek_char() == Some('*') {
                        self.pos += 1;
                        Ok(Token::ArrowStar)
                    } else {
                        Ok(Token::Arrow)
                    }
                } else if matches!(self.peek_char(), Some('0'..='9')) {
                    self.lex_number(true)
                } else {
                    bail!("Unexpected '-'")
                }
            }
            '"' | '\'' => self.lex_string(),
            '0'..='9' => self.lex_number(false),
            'a'..='z' | 'A'..='Z' | '_' => self.lex_ident(),
            other => bail!("Unexpected character: {other:?}"),
        }
    }

    fn lex_string(&mut self) -> anyhow::Result<Token> {
        let quote = self.advance_char().unwrap();
        let mut s = String::new();
        loop {
            match self.advance_char() {
                None => bail!("Unterminated string literal"),
                Some('\\') => match self.advance_char() {
                    Some('n') => s.push('\n'),
                    Some('t') => s.push('\t'),
                    Some('\\') => s.push('\\'),
                    Some('"') => s.push('"'),
                    Some('\'') => s.push('\''),
                    None => bail!("Unexpected end of string after '\\'"),
                    Some(c) => { s.push('\\'); s.push(c); }
                },
                Some(c) if c == quote => break,
                Some(c) => s.push(c),
            }
        }
        Ok(Token::Str(s))
    }

    fn lex_number(&mut self, negative: bool) -> anyhow::Result<Token> {
        let mut s = if negative { "-".to_string() } else { String::new() };
        while matches!(self.peek_char(), Some('0'..='9')) {
            s.push(self.advance_char().unwrap());
        }
        if self.peek_char() == Some('.') {
            s.push(self.advance_char().unwrap());
            while matches!(self.peek_char(), Some('0'..='9')) {
                s.push(self.advance_char().unwrap());
            }
            Ok(Token::Float(s.parse()?))
        } else {
            Ok(Token::Int(s.parse()?))
        }
    }

    fn lex_ident(&mut self) -> anyhow::Result<Token> {
        let mut s = String::new();
        while matches!(self.peek_char(), Some('a'..='z' | 'A'..='Z' | '0'..='9' | '_')) {
            s.push(self.advance_char().unwrap());
        }
        let tok = match s.as_str() {
            "AND" => Token::And,
            "OR" => Token::Or,
            "NOT" => Token::Not,
            "IS" => Token::Is,
            "PRESENT" => Token::Present,
            "ABSENT" => Token::Absent,
            "UNKNOWN" => Token::Unknown,
            "MATCHES" => Token::Matches,
            "true" => Token::Bool(true),
            "false" => Token::Bool(false),
            _ => Token::Ident(s),
        };
        Ok(tok)
    }
}

// ── Parser ────────────────────────────────────────────────────────────────────

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, pos: 0 }
    }

    fn peek(&self) -> &Token {
        self.tokens.get(self.pos).unwrap_or(&Token::Eof)
    }

    fn advance(&mut self) -> Token {
        let tok = self.tokens.get(self.pos).cloned().unwrap_or(Token::Eof);
        self.pos += 1;
        tok
    }

    fn expect_ident(&mut self) -> anyhow::Result<String> {
        match self.advance() {
            Token::Ident(s) => Ok(s),
            other => bail!("Expected identifier, got {other:?}"),
        }
    }

    fn parse_expr(&mut self) -> anyhow::Result<Query> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> anyhow::Result<Query> {
        let mut left = self.parse_and()?;
        while self.peek() == &Token::Or {
            self.advance();
            let right = self.parse_and()?;
            left = match left {
                Query::Or { mut operands } => {
                    operands.push(right);
                    Query::Or { operands }
                }
                other => Query::Or { operands: vec![other, right] },
            };
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> anyhow::Result<Query> {
        let mut left = self.parse_not()?;
        while self.peek() == &Token::And {
            self.advance();
            let right = self.parse_not()?;
            left = match left {
                Query::And { mut operands } => {
                    operands.push(right);
                    Query::And { operands }
                }
                other => Query::And { operands: vec![other, right] },
            };
        }
        Ok(left)
    }

    fn parse_not(&mut self) -> anyhow::Result<Query> {
        if self.peek() == &Token::Not {
            self.advance();
            let inner = self.parse_not()?;
            Ok(Query::Not { operand: Box::new(inner) })
        } else {
            self.parse_atom()
        }
    }

    fn parse_atom(&mut self) -> anyhow::Result<Query> {
        if self.peek() == &Token::LParen {
            self.advance();
            let q = self.parse_expr()?;
            match self.advance() {
                Token::RParen => Ok(q),
                other => bail!("Expected ')', got {other:?}"),
            }
        } else {
            self.parse_follow_chain()
        }
    }

    /// IDENT  (-> | ->*)  atom   |   leaf
    fn parse_follow_chain(&mut self) -> anyhow::Result<Query> {
        let ident = self.expect_ident()?;
        match self.peek().clone() {
            Token::Arrow => {
                self.advance();
                let cond = self.parse_atom()?;
                Ok(Query::Follows { field: ident, condition: Box::new(cond) })
            }
            Token::ArrowStar => {
                self.advance();
                let cond = self.parse_atom()?;
                Ok(Query::FollowsTransitive { field: ident, condition: Box::new(cond) })
            }
            _ => self.parse_leaf_with_ident(ident),
        }
    }

    fn parse_leaf_with_ident(&mut self, ident: String) -> anyhow::Result<Query> {
        match self.peek().clone() {
            Token::Is => {
                self.advance();
                match self.advance() {
                    Token::Present => Ok(Query::IsPresent { field: ident }),
                    Token::Absent => Ok(Query::IsAbsent { field: ident }),
                    Token::Unknown => Ok(Query::IsUnknown { field: ident }),
                    other => bail!("Expected PRESENT, ABSENT, or UNKNOWN, got {other:?}"),
                }
            }
            Token::Eq => {
                self.advance();
                Ok(Query::Eq { field: ident, value: self.parse_value()? })
            }
            Token::Neq => {
                self.advance();
                Ok(Query::Neq { field: ident, value: self.parse_value()? })
            }
            Token::Lt => {
                self.advance();
                Ok(Query::Lt { field: ident, value: self.parse_value()? })
            }
            Token::Lte => {
                self.advance();
                Ok(Query::Lte { field: ident, value: self.parse_value()? })
            }
            Token::Gt => {
                self.advance();
                Ok(Query::Gt { field: ident, value: self.parse_value()? })
            }
            Token::Gte => {
                self.advance();
                Ok(Query::Gte { field: ident, value: self.parse_value()? })
            }
            Token::Matches => {
                self.advance();
                match self.advance() {
                    Token::Str(s) => Ok(Query::Matches { field: ident, pattern: s }),
                    other => bail!("Expected string pattern after MATCHES, got {other:?}"),
                }
            }
            other => bail!("Unexpected token {other:?} after identifier '{ident}'"),
        }
    }

    fn parse_value(&mut self) -> anyhow::Result<Value> {
        match self.advance() {
            Token::Str(s) => Ok(Value::String(s)),
            Token::Int(n) => Ok(Value::Int(n)),
            Token::Float(f) => Ok(Value::Float(f)),
            Token::Bool(b) => Ok(Value::Bool(b)),
            other => bail!("Expected value literal, got {other:?}"),
        }
    }
}

// ── Public API ────────────────────────────────────────────────────────────────

/// Parses a DSL predicate string into a `Query`.
///
/// Grammar (simplified):
/// ```text
/// expr    = or_expr
/// or_expr = and_expr ("OR" and_expr)*
/// and_expr= not_expr ("AND" not_expr)*
/// not_expr= "NOT" not_expr | atom
/// atom    = "(" expr ")" | follow_chain
/// follow_chain = IDENT "->"  atom   → Follows
///              | IDENT "->*" atom   → FollowsTransitive
///              | leaf
/// leaf    = IDENT "IS" ("PRESENT"|"ABSENT"|"UNKNOWN")
///         | IDENT ("=" | "!=" | "<" | "<=" | ">" | ">=") value
/// value   = string | int | float | "true" | "false"
/// ```
pub fn parse(input: &str) -> anyhow::Result<Query> {
    let tokens = Lexer::new(input).tokenize()?;
    let mut parser = Parser::new(tokens);
    let q = parser.parse_expr()?;
    if parser.peek() != &Token::Eof {
        bail!("Unexpected trailing input at token {:?}", parser.peek());
    }
    Ok(q)
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── IS predicates ─────────────────────────────────────────────────────────

    #[test]
    fn test_parse_is_present() {
        let q = parse("path IS PRESENT").unwrap();
        assert!(matches!(q, Query::IsPresent { field } if field == "path"));
    }

    #[test]
    fn test_parse_is_absent() {
        let q = parse("path IS ABSENT").unwrap();
        assert!(matches!(q, Query::IsAbsent { field } if field == "path"));
    }

    #[test]
    fn test_parse_is_unknown() {
        let q = parse("path IS UNKNOWN").unwrap();
        assert!(matches!(q, Query::IsUnknown { field } if field == "path"));
    }

    // ── Comparisons ───────────────────────────────────────────────────────────

    #[test]
    fn test_parse_eq_string() {
        let q = parse(r#"label = "jazz""#).unwrap();
        assert!(
            matches!(q, Query::Eq { field, value: Value::String(s) } if field == "label" && s == "jazz")
        );
    }

    #[test]
    fn test_parse_eq_int() {
        let q = parse("rating = 5").unwrap();
        assert!(matches!(q, Query::Eq { field, value: Value::Int(5) } if field == "rating"));
    }

    #[test]
    fn test_parse_eq_float() {
        let q = parse("score = 3.14").unwrap();
        assert!(matches!(q, Query::Eq { field, value: Value::Float(f) } if field == "score" && (f - 3.14).abs() < 1e-9));
    }

    #[test]
    fn test_parse_eq_bool() {
        let q = parse("active = true").unwrap();
        assert!(matches!(q, Query::Eq { field, value: Value::Bool(true) } if field == "active"));
    }

    #[test]
    fn test_parse_gt() {
        let q = parse("rating > 3").unwrap();
        assert!(matches!(q, Query::Gt { field, value: Value::Int(3) } if field == "rating"));
    }

    // ── Boolean combinators ───────────────────────────────────────────────────

    #[test]
    fn test_parse_and() {
        let q = parse("path IS PRESENT AND rating IS PRESENT").unwrap();
        assert!(matches!(q, Query::And { operands } if operands.len() == 2));
    }

    #[test]
    fn test_parse_or() {
        let q = parse("path IS PRESENT OR rating IS PRESENT").unwrap();
        assert!(matches!(q, Query::Or { operands } if operands.len() == 2));
    }

    #[test]
    fn test_parse_not() {
        let q = parse("NOT path IS PRESENT").unwrap();
        assert!(matches!(q, Query::Not { .. }));
    }

    // ── Follow chains ─────────────────────────────────────────────────────────

    #[test]
    fn test_parse_follows() {
        let q = parse(r#"tag -> (label = "jazz")"#).unwrap();
        assert!(matches!(q, Query::Follows { field, .. } if field == "tag"));
    }

    #[test]
    fn test_parse_follows_transitive() {
        let q = parse(r#"parent ->* (label = "music")"#).unwrap();
        assert!(matches!(q, Query::FollowsTransitive { field, .. } if field == "parent"));
    }

    #[test]
    fn test_parse_chained() {
        // tag -> parent ->* (label = "music")
        let q = parse(r#"tag -> parent ->* (label = "music")"#).unwrap();
        match q {
            Query::Follows { field, condition } => {
                assert_eq!(field, "tag");
                assert!(matches!(*condition, Query::FollowsTransitive { ref field, .. } if field == "parent"));
            }
            other => panic!("Expected Follows, got {other:?}"),
        }
    }

    // ── Precedence ────────────────────────────────────────────────────────────

    #[test]
    fn test_parse_precedence_and_over_or() {
        // a OR b AND c  =>  a OR (b AND c)
        let q = parse("a IS PRESENT OR b IS PRESENT AND c IS PRESENT").unwrap();
        match q {
            Query::Or { operands } => {
                assert_eq!(operands.len(), 2);
                assert!(matches!(operands[0], Query::IsPresent { .. }));
                assert!(matches!(operands[1], Query::And { .. }));
            }
            other => panic!("Expected Or, got {other:?}"),
        }
    }

    #[test]
    fn test_parse_parens() {
        // (a OR b) AND c
        let q = parse("(a IS PRESENT OR b IS PRESENT) AND c IS PRESENT").unwrap();
        assert!(matches!(q, Query::And { .. }));
    }

    // ── Error cases ───────────────────────────────────────────────────────────

    #[test]
    fn test_parse_error_empty() {
        assert!(parse("").is_err());
    }

    #[test]
    fn test_parse_error_unexpected_token() {
        assert!(parse("rating IS BADKEYWORD").is_err());
    }

    #[test]
    fn test_parse_error_trailing() {
        assert!(parse("path IS PRESENT EXTRA").is_err());
    }

    #[test]
    fn test_parse_error_unclosed_paren() {
        assert!(parse("(path IS PRESENT").is_err());
    }

    #[test]
    fn test_parse_matches() {
        let q = parse(r#"path MATCHES "\.mp3$""#).unwrap();
        match q {
            Query::Matches { field, pattern } => {
                assert_eq!(field, "path");
                assert_eq!(pattern, r"\.mp3$");
            }
            other => panic!("Expected Matches, got {other:?}"),
        }
    }
}
