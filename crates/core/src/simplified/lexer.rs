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
    /// Exactly 32 ASCII hex characters — a metarecord UUID. Lexed ahead of
    /// `Number`/`Word` since it may start with either a digit or a letter,
    /// which would otherwise split it in two.
    Uuid,
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
        } else if let Some(end) = uuid_run_end(&chars, i) {
            toks.push(Tok { kind: TokKind::Uuid, text: chars[i..end].iter().collect() });
            i = end;
        } else if c.is_ascii_digit() {
            let start = i;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            // Fractional part only when '.' is followed by a digit, so `3..5`
            // lexes as 3, `..`, 5 rather than a broken float.
            if chars.get(i) == Some(&'.') && chars.get(i + 1).is_some_and(|d| d.is_ascii_digit()) {
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

/// End of a UUID token starting at `start`: exactly 32 ASCII hex characters
/// with no word material glued to them. `None` when the run is not
/// UUID-shaped, leaving the `Number`/`Word` branches to tokenize it as before
/// (a truncated run is not an error here — the fixed tokenizer reports
/// nothing, the grammar does).
fn uuid_run_end(chars: &[char], start: usize) -> Option<usize> {
    if !chars[start].is_ascii_hexdigit() {
        return None;
    }
    // The run must be maximal on the left too: in a 33-hex run the `Number`
    // branch takes the leading digit, and the 32 characters left over must not
    // then read as a UUID.
    if start > 0 && chars[start - 1].is_ascii_hexdigit() {
        return None;
    }
    let mut end = start;
    while end < chars.len() && chars[end].is_ascii_hexdigit() {
        end += 1;
    }
    if end - start != 32 {
        return None;
    }
    // `<32 hex>x` / `<32 hex>_x` is one longer word, not a UUID.
    if chars.get(end).is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_') {
        return None;
    }
    Some(end)
}

/// A symbol is any character that is not whitespace, word/number material, a
/// string quote, or a parenthesis (parentheses are their own single tokens).
fn is_symbol_char(c: char) -> bool {
    !c.is_whitespace() && !c.is_ascii_alphanumeric() && c != '_' && c != '"' && c != '(' && c != ')'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lx(s: &str) -> Vec<(TokKind, String)> {
        lex(s).unwrap().into_iter().map(|t| (t.kind, t.text)).collect()
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

    fn uu(s: &str) -> (TokKind, String) {
        (TokKind::Uuid, s.into())
    }

    #[test]
    fn uuid_is_one_token() {
        // 32 hex characters are one token whether they start with a digit
        // (`Number`'s branch) or a letter (`Word`'s).
        let digit_initial = "8f3a2b1c4d5e6f708192a3b4c5d6e7f8";
        let letter_initial = "f13a2b1c4d5e6f708192a3b4c5d6e7f8";
        assert_eq!(lx(digit_initial), vec![uu(digit_initial)]);
        assert_eq!(lx(letter_initial), vec![uu(letter_initial)]);
        assert_eq!(lx(&digit_initial.to_uppercase()), vec![uu(&digit_initial.to_uppercase())]);
    }

    #[test]
    fn uuid_composes_with_other_tokens() {
        let u = "8f3a2b1c4d5e6f708192a3b4c5d6e7f8";
        assert_eq!(lx(&format!("{u} rating>3")), vec![uu(u), w("rating"), sym(">"), n("3")]);
        assert_eq!(lx(&format!("({u})")), vec![lp(), uu(u), rp()]);
    }

    #[test]
    fn non_uuid_hex_runs_are_unchanged() {
        // Anything but exactly 32 hex characters tokenizes as it always did.
        assert_eq!(lx("8f3a2b1c4d5e"), vec![n("8"), w("f3a2b1c4d5e")]);
        assert_eq!(lx("deadbeef"), vec![w("deadbeef")]);
        assert_eq!(lx("123"), vec![n("123")]);
        // 33 hex characters, and a 32-hex run glued to more word material.
        assert_eq!(
            lx("8f3a2b1c4d5e6f708192a3b4c5d6e7f8f"),
            vec![n("8"), w("f3a2b1c4d5e6f708192a3b4c5d6e7f8f")]
        );
        assert_eq!(
            lx("8f3a2b1c4d5e6f708192a3b4c5d6e7f8_x"),
            vec![n("8"), w("f3a2b1c4d5e6f708192a3b4c5d6e7f8_x")]
        );
    }

    #[test]
    fn unterminated_string_errors() {
        assert!(lex(r#""abc"#).is_err());
    }
}
