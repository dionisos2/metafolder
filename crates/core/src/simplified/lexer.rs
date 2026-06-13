//! Fixed tokenizer for the simplified query language (spec-query "Lexer
//! (fixed)"). Whitespace is insignificant except as a token separator, so
//! `genre:jazz` and `genre : jazz` — and `100MB` / `100 MB` — tokenize
//! identically. Maximal munch on words gives free word boundaries.

/// Token categories produced by [`lex`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokKind {
    /// `[A-Za-z_][A-Za-z0-9_]*`.
    Word,
    /// An integer or float literal.
    Number,
    /// A double-quoted literal, kept with its surrounding quotes.
    Str,
    /// A maximal run of punctuation characters (`:`, `~`, `>=`, `..`, `+`, …).
    Symbol,
    /// `(` — its own token.
    LParen,
    /// `)` — its own token.
    RParen,
}

/// A token carrying its raw source lexeme (`Str` includes the quotes, so a
/// production with no template can return the source text verbatim).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tok {
    pub kind: TokKind,
    pub text: String,
}

/// Tokenizes simplified-language input, discarding whitespace between tokens.
pub fn lex(input: &str) -> Result<Vec<Tok>, String> {
    let chars: Vec<char> = input.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '(' {
            toks.push(Tok { kind: TokKind::LParen, text: "(".into() });
            i += 1;
        } else if c == ')' {
            toks.push(Tok { kind: TokKind::RParen, text: ")".into() });
            i += 1;
        } else if c == '"' {
            let start = i;
            i += 1;
            loop {
                match chars.get(i) {
                    None => return Err("unterminated string literal".into()),
                    // Skip the backslash and the escaped character, keeping
                    // both in the raw lexeme; downstream the DSL parser decodes.
                    Some('\\') => i += 2,
                    Some('"') => {
                        i += 1;
                        break;
                    }
                    Some(_) => i += 1,
                }
            }
            toks.push(Tok { kind: TokKind::Str, text: chars[start..i].iter().collect() });
        } else if c.is_ascii_digit() {
            let start = i;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            // Fractional part only when '.' is followed by a digit, so `3..5`
            // lexes as 3, `..`, 5 rather than a broken float.
            if chars.get(i) == Some(&'.')
                && chars.get(i + 1).is_some_and(|d| d.is_ascii_digit())
            {
                i += 1;
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
            }
            toks.push(Tok { kind: TokKind::Number, text: chars[start..i].iter().collect() });
        } else if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            toks.push(Tok { kind: TokKind::Word, text: chars[start..i].iter().collect() });
        } else {
            let start = i;
            while i < chars.len() && is_symbol_char(chars[i]) {
                i += 1;
            }
            toks.push(Tok { kind: TokKind::Symbol, text: chars[start..i].iter().collect() });
        }
    }
    Ok(toks)
}

/// A symbol is any character that is not whitespace, word/number material, a
/// string quote, or a parenthesis (parentheses are their own single tokens).
fn is_symbol_char(c: char) -> bool {
    !c.is_whitespace()
        && !c.is_ascii_alphanumeric()
        && c != '_'
        && c != '"'
        && c != '('
        && c != ')'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lx(s: &str) -> Vec<(TokKind, String)> {
        lex(s)
            .unwrap()
            .into_iter()
            .map(|t| (t.kind, t.text))
            .collect()
    }
    fn w(s: &str) -> (TokKind, String) {
        (TokKind::Word, s.into())
    }
    fn n(s: &str) -> (TokKind, String) {
        (TokKind::Number, s.into())
    }
    fn st(s: &str) -> (TokKind, String) {
        (TokKind::Str, s.into())
    }
    fn sym(s: &str) -> (TokKind, String) {
        (TokKind::Symbol, s.into())
    }
    fn lp() -> (TokKind, String) {
        (TokKind::LParen, "(".into())
    }
    fn rp() -> (TokKind, String) {
        (TokKind::RParen, ")".into())
    }

    #[test]
    fn empty_and_whitespace() {
        assert_eq!(lx(""), vec![]);
        assert_eq!(lx("   \t "), vec![]);
    }

    #[test]
    fn field_colon_value() {
        assert_eq!(lx("genre:jazz"), vec![w("genre"), sym(":"), w("jazz")]);
    }

    #[test]
    fn whitespace_insignificant_around_operator() {
        assert_eq!(lx("genre : jazz"), lx("genre:jazz"));
    }

    #[test]
    fn comparison_and_number() {
        assert_eq!(lx("rating>=4"), vec![w("rating"), sym(">="), n("4")]);
    }

    #[test]
    fn number_unit_adjacent_equals_spaced() {
        assert_eq!(lx("100MB"), vec![n("100"), w("MB")]);
        assert_eq!(lx("100 MB"), lx("100MB"));
    }

    #[test]
    fn float_number() {
        assert_eq!(lx("1.5"), vec![n("1.5")]);
    }

    #[test]
    fn range_is_not_a_float() {
        assert_eq!(lx("3..5"), vec![n("3"), sym(".."), n("5")]);
    }

    #[test]
    fn string_keeps_quotes() {
        assert_eq!(lx(r#""a b""#), vec![st(r#""a b""#)]);
    }

    #[test]
    fn string_with_escaped_quote() {
        assert_eq!(lx(r#""a\"b""#), vec![st(r#""a\"b""#)]);
    }

    #[test]
    fn parens_are_their_own_tokens() {
        assert_eq!(lx("(a)"), vec![lp(), w("a"), rp()]);
    }

    #[test]
    fn maximal_munch_symbol() {
        assert_eq!(lx("->*"), vec![sym("->*")]);
        assert_eq!(lx("!a"), vec![sym("!"), w("a")]);
    }

    #[test]
    fn symbol_stops_at_paren() {
        assert_eq!(lx(":("), vec![sym(":"), lp()]);
    }

    #[test]
    fn word_boundary_is_free() {
        // A grammar literal "tag" must not match the token "tags": they are
        // distinct words.
        assert_eq!(lx("tags"), vec![w("tags")]);
    }

    #[test]
    fn unterminated_string_errors() {
        assert!(lex(r#""abc"#).is_err());
    }
}
