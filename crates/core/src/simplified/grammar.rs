//! Grammar AST and the parser for the grammar file (spec-query "Grammar
//! notation"). The grammar is a list of named productions; each alternative is
//! a sequence of items with an optional output template. Templates are kept as
//! raw source here and parsed/evaluated separately (see `template`).

use super::lexer::TokKind;

/// A parsed grammar: an ordered list of productions. The start production is
/// `query` (enforced at load/validation time, not here).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Grammar {
    pub productions: Vec<Production>,
}

/// A named production with one or more alternatives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Production {
    pub name: String,
    pub alts: Vec<Alt>,
}

/// One alternative: a sequence of items and an optional output template (the
/// raw text right of `=>`; `None` means "return the matched source text").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Alt {
    pub seq: Vec<Item>,
    pub template: Option<String>,
}

/// A pattern item: an expression with an optional capture label (`name:`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub label: Option<String>,
    pub expr: Expr,
}

/// A pattern expression.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Expr {
    /// `"lit"` — matches one input token whose text equals this (decoded) text.
    Lit(String),
    /// `WORD` / `NUMBER` / `STRING` — matches a token of that class.
    Class(TokKind),
    /// A reference to another production.
    Rule(String),
    /// `( seq | seq | ... )` — a parenthesised alternation of sequences.
    Group(Vec<Vec<Item>>),
    /// A repetition, optionally separated by `sep` (`** ` / `++`).
    Repeat {
        inner: Box<Expr>,
        kind: RepeatKind,
        sep: Option<Box<Expr>>,
    },
}

/// The three repetition flavours: `*`, `+`, `?`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepeatKind {
    Star,
    Plus,
    Opt,
}

/// Parses grammar source into a [`Grammar`].
pub fn parse_grammar(src: &str) -> Result<Grammar, String> {
    // 1. Group lines into productions. A header line is `IDENT =` (a single
    //    `=`, not `=>`); any other non-blank line continues the current one.
    let mut prods: Vec<(String, String)> = Vec::new();
    for raw in src.lines() {
        let line = strip_comment(raw);
        if line.trim().is_empty() {
            continue;
        }
        if let Some((name, rest)) = header_split(line) {
            prods.push((name, rest.to_string()));
        } else {
            match prods.last_mut() {
                Some((_, body)) => {
                    body.push('\n');
                    body.push_str(line);
                }
                None => return Err(format!("alternative line before any production: {}", line.trim())),
            }
        }
    }

    // 2. Parse each production's body into alternatives.
    let mut productions = Vec::with_capacity(prods.len());
    for (name, body) in prods {
        let mut alts = Vec::new();
        for alt_src in split_top_level(&body, '|') {
            if alt_src.trim().is_empty() {
                continue;
            }
            alts.push(parse_alternative(alt_src.trim())?);
        }
        if alts.is_empty() {
            return Err(format!("production '{name}' has no alternatives"));
        }
        productions.push(Production { name, alts });
    }
    Ok(Grammar { productions })
}

/// Cuts a line at the first `#` that is not inside a string literal.
fn strip_comment(line: &str) -> &str {
    let mut in_str = false;
    let mut escaped = false;
    for (i, c) in line.char_indices() {
        if in_str {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
            }
        } else if c == '"' {
            in_str = true;
        } else if c == '#' {
            return &line[..i];
        }
    }
    line
}

/// If `line` is a production header (`IDENT =`, where `=` is not `=>`), returns
/// `(name, rest_after_eq)`.
fn header_split(line: &str) -> Option<(String, &str)> {
    let trimmed = line.trim_start();
    let mut end = 0;
    for (i, c) in trimmed.char_indices() {
        let ok = if i == 0 {
            c.is_ascii_alphabetic() || c == '_'
        } else {
            c.is_ascii_alphanumeric() || c == '_'
        };
        if ok {
            end = i + c.len_utf8();
        } else {
            break;
        }
    }
    if end == 0 {
        return None; // no identifier
    }
    let name = &trimmed[..end];
    let after = trimmed[end..].trim_start();
    let mut it = after.char_indices();
    match it.next() {
        Some((_, '=')) => {}
        _ => return None,
    }
    let rest = &after[1..];
    if rest.starts_with('>') {
        return None; // this was '=>', not a definition
    }
    Some((name.to_string(), rest))
}

/// Splits on `sep` at the top level — outside string literals, `(...)` groups
/// and `{...}` template blocks.
fn split_top_level(s: &str, sep: char) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut paren = 0i32;
    let mut brace = 0i32;
    let mut in_str = false;
    let mut escaped = false;
    for c in s.chars() {
        if in_str {
            cur.push(c);
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => {
                in_str = true;
                cur.push(c);
            }
            '(' => {
                paren += 1;
                cur.push(c);
            }
            ')' => {
                paren -= 1;
                cur.push(c);
            }
            '{' => {
                brace += 1;
                cur.push(c);
            }
            '}' => {
                brace -= 1;
                cur.push(c);
            }
            _ if c == sep && paren == 0 && brace == 0 => {
                parts.push(std::mem::take(&mut cur));
            }
            _ => cur.push(c),
        }
    }
    parts.push(cur);
    parts
}

/// Splits an alternative into its pattern and optional template at the first
/// top-level `=>` (outside string literals).
fn parse_alternative(s: &str) -> Result<Alt, String> {
    let (pattern_src, template) = match find_arrow(s) {
        Some(idx) => (&s[..idx], Some(s[idx + 2..].trim().to_string())),
        None => (s, None),
    };
    let seq = parse_pattern(pattern_src)?;
    Ok(Alt { seq, template })
}

/// Byte index of the first `=>` outside a string literal, if any.
fn find_arrow(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut in_str = false;
    let mut escaped = false;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if in_str {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
            }
        } else if c == '"' {
            in_str = true;
        } else if c == '=' && bytes.get(i + 1) == Some(&b'>') {
            return Some(i);
        }
        i += 1;
    }
    None
}

// ── Pattern parser ──────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum PTok {
    Ident(String),
    Str(String),
    LParen,
    RParen,
    Pipe,
    Colon,
    Star,
    Plus,
    Opt,
    StarStar,
    PlusPlus,
}

fn lex_pattern(s: &str) -> Result<Vec<PTok>, String> {
    let chars: Vec<char> = s.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '"' {
            let mut text = String::new();
            i += 1;
            loop {
                match chars.get(i) {
                    None => return Err("unterminated string in grammar pattern".into()),
                    Some('\\') => {
                        match chars.get(i + 1) {
                            Some(&'"') => text.push('"'),
                            Some(&'\\') => text.push('\\'),
                            Some(&other) => {
                                text.push('\\');
                                text.push(other);
                            }
                            None => return Err("dangling escape in grammar pattern".into()),
                        }
                        i += 2;
                    }
                    Some('"') => {
                        i += 1;
                        break;
                    }
                    Some(&other) => {
                        text.push(other);
                        i += 1;
                    }
                }
            }
            toks.push(PTok::Str(text));
        } else if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            toks.push(PTok::Ident(chars[start..i].iter().collect()));
        } else {
            match c {
                '(' => toks.push(PTok::LParen),
                ')' => toks.push(PTok::RParen),
                '|' => toks.push(PTok::Pipe),
                ':' => toks.push(PTok::Colon),
                '?' => toks.push(PTok::Opt),
                '*' => {
                    if chars.get(i + 1) == Some(&'*') {
                        toks.push(PTok::StarStar);
                        i += 1;
                    } else {
                        toks.push(PTok::Star);
                    }
                }
                '+' => {
                    if chars.get(i + 1) == Some(&'+') {
                        toks.push(PTok::PlusPlus);
                        i += 1;
                    } else {
                        toks.push(PTok::Plus);
                    }
                }
                other => return Err(format!("unexpected character '{other}' in grammar pattern")),
            }
            i += 1;
        }
    }
    Ok(toks)
}

fn parse_pattern(src: &str) -> Result<Vec<Item>, String> {
    let toks = lex_pattern(src)?;
    let mut p = PatternParser { toks, pos: 0 };
    let items = p.sequence()?;
    if p.pos != p.toks.len() {
        return Err(format!("unexpected '{:?}' in grammar pattern", p.toks[p.pos]));
    }
    if items.is_empty() {
        return Err("empty alternative".into());
    }
    Ok(items)
}

struct PatternParser {
    toks: Vec<PTok>,
    pos: usize,
}

impl PatternParser {
    fn peek(&self) -> Option<&PTok> {
        self.toks.get(self.pos)
    }
    fn peek2(&self) -> Option<&PTok> {
        self.toks.get(self.pos + 1)
    }
    fn advance(&mut self) -> Option<PTok> {
        let t = self.toks.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    /// A sequence of items up to end-of-input, `)` or `|`.
    fn sequence(&mut self) -> Result<Vec<Item>, String> {
        let mut items = Vec::new();
        while !matches!(self.peek(), None | Some(PTok::RParen) | Some(PTok::Pipe)) {
            items.push(self.item()?);
        }
        Ok(items)
    }

    fn item(&mut self) -> Result<Item, String> {
        let label = if matches!(self.peek(), Some(PTok::Ident(_)))
            && matches!(self.peek2(), Some(PTok::Colon))
        {
            let name = match self.advance() {
                Some(PTok::Ident(n)) => n,
                _ => unreachable!(),
            };
            self.advance(); // the ':'
            Some(name)
        } else {
            None
        };
        let expr = self.postfix()?;
        Ok(Item { label, expr })
    }

    fn postfix(&mut self) -> Result<Expr, String> {
        let mut e = self.primary()?;
        loop {
            let (kind, separated) = match self.peek() {
                Some(PTok::Star) => (RepeatKind::Star, false),
                Some(PTok::Plus) => (RepeatKind::Plus, false),
                Some(PTok::Opt) => (RepeatKind::Opt, false),
                Some(PTok::StarStar) => (RepeatKind::Star, true),
                Some(PTok::PlusPlus) => (RepeatKind::Plus, true),
                _ => break,
            };
            self.advance();
            let sep = if separated {
                Some(Box::new(self.primary()?))
            } else {
                None
            };
            e = Expr::Repeat { inner: Box::new(e), kind, sep };
        }
        Ok(e)
    }

    fn primary(&mut self) -> Result<Expr, String> {
        match self.advance() {
            Some(PTok::Str(s)) => Ok(Expr::Lit(s)),
            Some(PTok::Ident(name)) => Ok(match name.as_str() {
                "WORD" => Expr::Class(TokKind::Word),
                "NUMBER" => Expr::Class(TokKind::Number),
                "STRING" => Expr::Class(TokKind::Str),
                _ => Expr::Rule(name),
            }),
            Some(PTok::LParen) => {
                let mut seqs = vec![self.sequence()?];
                while matches!(self.peek(), Some(PTok::Pipe)) {
                    self.advance();
                    seqs.push(self.sequence()?);
                }
                match self.advance() {
                    Some(PTok::RParen) => {}
                    _ => return Err("expected ')' in grammar pattern".into()),
                }
                Ok(Expr::Group(seqs))
            }
            other => Err(format!("unexpected {other:?} in grammar pattern")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn g(src: &str) -> Vec<Production> {
        parse_grammar(src).unwrap().productions
    }
    fn rule(s: &str) -> Expr {
        Expr::Rule(s.into())
    }
    fn lit(s: &str) -> Expr {
        Expr::Lit(s.into())
    }
    fn word() -> Expr {
        Expr::Class(TokKind::Word)
    }
    fn item(expr: Expr) -> Item {
        Item { label: None, expr }
    }
    fn litem(label: &str, expr: Expr) -> Item {
        Item { label: Some(label.into()), expr }
    }
    fn alt(seq: Vec<Item>, template: Option<&str>) -> Alt {
        Alt { seq, template: template.map(Into::into) }
    }

    #[test]
    fn simple_token_class() {
        assert_eq!(
            g("a = WORD"),
            vec![Production { name: "a".into(), alts: vec![alt(vec![item(word())], None)] }]
        );
    }

    #[test]
    fn label_rule_and_template() {
        assert_eq!(
            g("a = x:b => $x"),
            vec![Production {
                name: "a".into(),
                alts: vec![alt(vec![litem("x", rule("b"))], Some("$x"))],
            }]
        );
    }

    #[test]
    fn alternatives_on_one_line() {
        assert_eq!(
            g("a = STRING | WORD"),
            vec![Production {
                name: "a".into(),
                alts: vec![
                    alt(vec![item(Expr::Class(TokKind::Str))], None),
                    alt(vec![item(word())], None),
                ],
            }]
        );
    }

    #[test]
    fn alternatives_across_lines() {
        let src = "unary = \"!\" u:x => NOT $u\n      | y:z => $y";
        assert_eq!(
            g(src),
            vec![Production {
                name: "unary".into(),
                alts: vec![
                    alt(vec![item(lit("!")), litem("u", rule("x"))], Some("NOT $u")),
                    alt(vec![litem("y", rule("z"))], Some("$y")),
                ],
            }]
        );
    }

    #[test]
    fn header_then_first_alt_without_pipe() {
        let src = "a =\n    p:b => $p\n  | q:c => $q";
        assert_eq!(
            g(src),
            vec![Production {
                name: "a".into(),
                alts: vec![
                    alt(vec![litem("p", rule("b"))], Some("$p")),
                    alt(vec![litem("q", rule("c"))], Some("$q")),
                ],
            }]
        );
    }

    #[test]
    fn repetition_plus() {
        assert_eq!(
            g("a = b+"),
            vec![Production {
                name: "a".into(),
                alts: vec![alt(
                    vec![item(Expr::Repeat {
                        inner: Box::new(rule("b")),
                        kind: RepeatKind::Plus,
                        sep: None,
                    })],
                    None,
                )],
            }]
        );
    }

    #[test]
    fn repetition_separated() {
        let prods = g("a = b ++ (\"OR\" | \"+\")");
        let expected_sep = Expr::Group(vec![vec![item(lit("OR"))], vec![item(lit("+"))]]);
        assert_eq!(
            prods[0].alts[0].seq,
            vec![item(Expr::Repeat {
                inner: Box::new(rule("b")),
                kind: RepeatKind::Plus,
                sep: Some(Box::new(expected_sep)),
            })]
        );
    }

    #[test]
    fn group_alternation() {
        assert_eq!(
            g("a = (\"x\" | \"y\")")[0].alts[0].seq,
            vec![item(Expr::Group(vec![vec![item(lit("x"))], vec![item(lit("y"))]]))]
        );
    }

    #[test]
    fn label_on_repetition_binds_the_list() {
        // items:and ++ sep  — the label binds the whole repetition.
        let prods = g("or = items:and ++ (\"OR\" | \"+\") => x");
        let it = &prods[0].alts[0].seq[0];
        assert_eq!(it.label.as_deref(), Some("items"));
        assert!(matches!(it.expr, Expr::Repeat { kind: RepeatKind::Plus, sep: Some(_), .. }));
    }

    #[test]
    fn comments_and_blank_lines_ignored() {
        let src = "# a comment\n\na = WORD  # trailing comment\n";
        assert_eq!(g(src).len(), 1);
        assert_eq!(g(src)[0].alts[0].seq, vec![item(word())]);
    }

    #[test]
    fn two_productions() {
        let prods = g("a = WORD\nb = NUMBER");
        assert_eq!(prods.len(), 2);
        assert_eq!(prods[0].name, "a");
        assert_eq!(prods[1].name, "b");
    }

    #[test]
    fn pipe_inside_string_is_not_a_separator() {
        // A literal containing '|' must stay one alternative.
        let prods = g("a = \"a|b\"");
        assert_eq!(prods[0].alts.len(), 1);
        assert_eq!(prods[0].alts[0].seq, vec![item(lit("a|b"))]);
    }

    #[test]
    fn errors() {
        assert!(parse_grammar("a = ( WORD").is_err()); // unclosed group
        assert!(parse_grammar("| a").is_err()); // alternative before any production
        assert!(parse_grammar("a =").is_err()); // no alternatives
    }
}
