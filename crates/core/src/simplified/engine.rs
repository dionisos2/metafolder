//! The grammar interpreter (spec-query "Simplified query language"). [`expand`]
//! runs the grammar over the lexed input with ordered choice (first matching
//! alternative wins, no backtracking once one succeeds) and renders each
//! production's template, producing normal DSL text. [`validate`] is the
//! load-time check: the `query` start production exists, every referenced rule
//! exists, templates parse, and the grammar is not left-recursive.

use std::collections::{HashMap, HashSet};

use super::grammar::{Expr, Grammar, Item, RepeatKind};
use super::lexer::{lex, Tok};
use super::template::{parse_template, render, Capture, Captures};

/// Guards against runaway recursion (e.g. an unvalidated left-recursive
/// grammar) so a bad grammar errors instead of overflowing the stack.
const MAX_DEPTH: usize = 1000;

/// Expands simplified-language `input` to normal DSL text using `grammar`.
/// Relative-date rules read the current time via the template `now()`.
pub fn expand(grammar: &Grammar, input: &str) -> Result<String, String> {
    expand_at(grammar, input, crate::date::now_ms())
}

/// [`expand`] with an explicit current time (Unix milliseconds) for `now()`,
/// so callers — chiefly tests — can make expansion deterministic.
pub fn expand_at(grammar: &Grammar, input: &str, now_ms: i64) -> Result<String, String> {
    let tokens = lex(input)?;
    let eng = Engine { grammar, tokens: &tokens, now_ms };
    match eng.match_prod("query", 0, 0)? {
        Some((out, next)) if next == tokens.len() => Ok(out),
        Some((_, next)) => Err(format!(
            "unexpected trailing input starting at '{}'",
            tokens[next].text
        )),
        None => Err("input does not match the simplified grammar".into()),
    }
}

struct Engine<'a> {
    grammar: &'a Grammar,
    tokens: &'a [Tok],
    now_ms: i64,
}

/// `Ok(None)` = no match (backtrack); `Err` = a hard error (missing production
/// at runtime, template/eval failure, depth limit).
type MatchProd = Result<Option<(String, usize)>, String>;
type MatchExpr = Result<Option<(Capture, Captures, usize)>, String>;

impl Engine<'_> {
    fn match_prod(&self, name: &str, pos: usize, depth: usize) -> MatchProd {
        if depth > MAX_DEPTH {
            return Err(format!("recursion limit exceeded at '{name}' (left recursion?)"));
        }
        let prod = self
            .grammar
            .productions
            .iter()
            .find(|p| p.name == name)
            .ok_or_else(|| format!("unknown production '{name}'"))?;
        for alt in &prod.alts {
            if let Some((caps, next)) = self.match_seq(&alt.seq, pos, depth + 1)? {
                let out = match &alt.template {
                    Some(src) => render(&parse_template(src)?, &caps, self.now_ms)?,
                    None => self.raw_text(pos, next),
                };
                return Ok(Some((out, next)));
            }
        }
        Ok(None)
    }

    fn match_seq(&self, seq: &[Item], pos: usize, depth: usize) -> Result<Option<(Captures, usize)>, String> {
        let mut caps = Captures::new();
        let mut p = pos;
        for item in seq {
            match self.match_expr(&item.expr, p, depth)? {
                Some((value, sub, next)) => {
                    caps.extend(sub);
                    if let Some(label) = &item.label {
                        caps.insert(label.clone(), value);
                    }
                    p = next;
                }
                None => return Ok(None),
            }
        }
        Ok(Some((caps, p)))
    }

    fn match_expr(&self, expr: &Expr, pos: usize, depth: usize) -> MatchExpr {
        match expr {
            Expr::Lit(s) => Ok(self
                .tokens
                .get(pos)
                .filter(|t| &t.text == s)
                .map(|t| (Capture::Text(t.text.clone()), Captures::new(), pos + 1))),
            Expr::Class(kind) => Ok(self
                .tokens
                .get(pos)
                .filter(|t| &t.kind == kind)
                .map(|t| (Capture::Text(t.text.clone()), Captures::new(), pos + 1))),
            Expr::Rule(name) => Ok(self
                .match_prod(name, pos, depth)?
                .map(|(out, next)| (Capture::Text(out), Captures::new(), next))),
            Expr::Group(seqs) => {
                for seq in seqs {
                    if let Some((caps, next)) = self.match_seq(seq, pos, depth)? {
                        return Ok(Some((Capture::Text(self.raw_text(pos, next)), caps, next)));
                    }
                }
                Ok(None)
            }
            Expr::Repeat { inner, kind, sep } => {
                self.match_repeat(inner, kind, sep.as_deref(), pos, depth)
            }
        }
    }

    fn match_repeat(
        &self,
        inner: &Expr,
        kind: &RepeatKind,
        sep: Option<&Expr>,
        pos: usize,
        depth: usize,
    ) -> MatchExpr {
        let mut list: Vec<String> = Vec::new();
        let mut subcaps = Captures::new();
        let mut p = pos;

        // First element.
        match self.match_expr(inner, p, depth)? {
            None => {
                return match kind {
                    RepeatKind::Plus => Ok(None),
                    _ => Ok(Some((Capture::List(list), subcaps, pos))),
                };
            }
            Some((val, sub, next)) => {
                push_value(&mut list, val);
                subcaps.extend(sub);
                p = next;
            }
        }
        if matches!(kind, RepeatKind::Opt) {
            return Ok(Some((Capture::List(list), subcaps, p)));
        }

        // Subsequent elements, optionally separated.
        loop {
            let elem_pos = match sep {
                Some(sep) => match self.match_expr(sep, p, depth)? {
                    // Separator consumed; an element must follow or we drop it.
                    Some((_, _, np)) => np,
                    None => break,
                },
                None => p,
            };
            match self.match_expr(inner, elem_pos, depth)? {
                Some((val, sub, next)) if next > elem_pos => {
                    push_value(&mut list, val);
                    subcaps.extend(sub);
                    p = next;
                }
                _ => break,
            }
        }
        Ok(Some((Capture::List(list), subcaps, p)))
    }

    /// The raw source text of `tokens[pos..next]` (the default output of a
    /// production without a template), token texts joined by a space.
    fn raw_text(&self, pos: usize, next: usize) -> String {
        self.tokens[pos..next]
            .iter()
            .map(|t| t.text.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    }
}

fn push_value(list: &mut Vec<String>, value: Capture) {
    match value {
        Capture::Text(s) => list.push(s),
        Capture::List(items) => list.extend(items),
    }
}

// ── Load-time validation ────────────────────────────────────────────────────

/// Validates a grammar: the `query` start production exists, every referenced
/// rule exists, templates parse, and there is no left recursion.
pub fn validate(grammar: &Grammar) -> Result<(), String> {
    if !grammar.productions.iter().any(|p| p.name == "query") {
        return Err("grammar has no 'query' start production".into());
    }
    let names: HashSet<&str> = grammar.productions.iter().map(|p| p.name.as_str()).collect();
    for p in &grammar.productions {
        for alt in &p.alts {
            check_refs(&p.name, &alt.seq, &names)?;
            if let Some(src) = &alt.template {
                parse_template(src).map_err(|e| format!("in production '{}': {e}", p.name))?;
            }
        }
    }
    detect_left_recursion(grammar)
}

fn check_refs(prod: &str, seq: &[Item], names: &HashSet<&str>) -> Result<(), String> {
    for item in seq {
        check_refs_expr(prod, &item.expr, names)?;
    }
    Ok(())
}

fn check_refs_expr(prod: &str, expr: &Expr, names: &HashSet<&str>) -> Result<(), String> {
    match expr {
        Expr::Rule(n) if !names.contains(n.as_str()) => {
            Err(format!("production '{prod}' references unknown rule '{n}'"))
        }
        Expr::Lit(lit) => check_literal(prod, lit),
        Expr::Rule(_) | Expr::Class(_) => Ok(()),
        Expr::Group(seqs) => {
            for s in seqs {
                check_refs(prod, s, names)?;
            }
            Ok(())
        }
        Expr::Repeat { inner, sep, .. } => {
            check_refs_expr(prod, inner, names)?;
            if let Some(sep) = sep {
                check_refs_expr(prod, sep, names)?;
            }
            Ok(())
        }
    }
}

/// A grammar literal matches exactly one input token by text, so a literal
/// that does not itself tokenize to a single token can never match — a silent
/// footgun (e.g. `"/fav"`, which lexes to `/` and `fav`). Reject it at load
/// time with a message that names the pieces and points at the fix.
fn check_literal(prod: &str, lit: &str) -> Result<(), String> {
    match lex(lit) {
        Ok(toks) if toks.len() == 1 => Ok(()),
        Ok(toks) if toks.is_empty() => Err(format!(
            "production '{prod}': literal \"{lit}\" cannot match — it is empty or whitespace-only"
        )),
        Ok(toks) => {
            let pieces = toks.iter().map(|t| t.text.as_str()).collect::<Vec<_>>().join(" , ");
            Err(format!(
                "production '{prod}': literal \"{lit}\" cannot match — it tokenizes into {} \
                 tokens ({pieces}); write it as separate literals (e.g. {})",
                toks.len(),
                toks.iter().map(|t| format!("\"{}\"", t.text)).collect::<Vec<_>>().join(" ")
            ))
        }
        Err(e) => Err(format!("production '{prod}': literal \"{lit}\" is not lexable: {e}")),
    }
}

fn detect_left_recursion(grammar: &Grammar) -> Result<(), String> {
    let mut edges: HashMap<String, Vec<String>> = HashMap::new();
    for p in &grammar.productions {
        let mut lc = Vec::new();
        for alt in &p.alts {
            lc.extend(left_rules(&alt.seq));
        }
        edges.insert(p.name.clone(), lc);
    }
    let mut gray = HashSet::new();
    let mut done = HashSet::new();
    for p in &grammar.productions {
        dfs(&p.name, &edges, &mut gray, &mut done)?;
    }
    Ok(())
}

fn dfs(
    node: &str,
    edges: &HashMap<String, Vec<String>>,
    gray: &mut HashSet<String>,
    done: &mut HashSet<String>,
) -> Result<(), String> {
    if done.contains(node) {
        return Ok(());
    }
    if !gray.insert(node.to_string()) {
        return Err(format!("left recursion involving production '{node}'"));
    }
    if let Some(succ) = edges.get(node) {
        for s in succ {
            if edges.contains_key(s) {
                dfs(s, edges, gray, done)?;
            }
        }
    }
    gray.remove(node);
    done.insert(node.to_string());
    Ok(())
}

/// The left-corner rule references of a sequence: rules that can appear first
/// without a token being consumed before them. A `*`/`?` item can be skipped,
/// so the scan continues past it; anything else blocks.
fn left_rules(seq: &[Item]) -> Vec<String> {
    let mut out = Vec::new();
    for item in seq {
        let (rules, skippable) = expr_left(&item.expr);
        out.extend(rules);
        if !skippable {
            break;
        }
    }
    out
}

fn expr_left(expr: &Expr) -> (Vec<String>, bool) {
    match expr {
        Expr::Lit(_) | Expr::Class(_) => (Vec::new(), false),
        Expr::Rule(n) => (vec![n.clone()], false),
        Expr::Group(seqs) => {
            let mut rules = Vec::new();
            for s in seqs {
                rules.extend(left_rules(s));
            }
            (rules, false)
        }
        Expr::Repeat { inner, kind, .. } => {
            let (rules, _) = expr_left(inner);
            (rules, matches!(kind, RepeatKind::Star | RepeatKind::Opt))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::simplified::grammar::parse_grammar;

    const G: &str = r#"
query = q:or => $q
or  = items:and ++ "OR" => {join(" OR ", $items)}
and = items:pred +       => {join(" AND ", $items)}
pred =
    f:WORD ":"  n:NUMBER => $f = $n
  | f:WORD ":"  w:WORD   => $f = {str($w)}
  | f:WORD ">=" n:number => $f >= $n
  | "!" p:pred           => NOT $p
  | "(" q:query ")"      => ($q)
  | "tag"                => tag = "x"
number = n:NUMBER "MB" => {num($n) * 1048576}
       | n:NUMBER      => $n
"#;

    fn ex(input: &str) -> Result<String, String> {
        expand(&parse_grammar(G).unwrap(), input)
    }

    #[test]
    fn field_string_value_is_quoted() {
        assert_eq!(ex("genre:jazz").unwrap(), "genre = \"jazz\"");
    }

    #[test]
    fn field_number_value_is_bare() {
        assert_eq!(ex("rating:5").unwrap(), "rating = 5");
    }

    #[test]
    fn unit_conversion() {
        assert_eq!(ex("size>=100MB").unwrap(), "size >= 104857600");
    }

    #[test]
    fn juxtaposition_is_and() {
        assert_eq!(ex("a:x b:y").unwrap(), "a = \"x\" AND b = \"y\"");
    }

    #[test]
    fn or_separator() {
        assert_eq!(ex("a:x OR b:y").unwrap(), "a = \"x\" OR b = \"y\"");
    }

    #[test]
    fn precedence_or_below_and() {
        assert_eq!(ex("a:x b:y OR c:z").unwrap(), "a = \"x\" AND b = \"y\" OR c = \"z\"");
    }

    #[test]
    fn prefix_not_and_parens() {
        assert_eq!(ex("!a:x").unwrap(), "NOT a = \"x\"");
        assert_eq!(ex("(a:x)").unwrap(), "(a = \"x\")");
    }

    #[test]
    fn ordered_choice_bare_word_is_fallback() {
        assert_eq!(ex("tag").unwrap(), "tag = \"x\"");
        // The operator forms win when an operator follows.
        assert_eq!(ex("tag:jazz").unwrap(), "tag = \"jazz\"");
    }

    #[test]
    fn trailing_input_errors() {
        assert!(ex("a:x junk").is_err());
    }

    #[test]
    fn no_match_errors() {
        assert!(ex("123").is_err());
    }

    #[test]
    fn validate_accepts_the_grammar() {
        assert!(validate(&parse_grammar(G).unwrap()).is_ok());
    }

    #[test]
    fn validate_rejects_missing_query() {
        assert!(validate(&parse_grammar("a = WORD").unwrap()).is_err());
    }

    #[test]
    fn validate_rejects_unknown_rule() {
        assert!(validate(&parse_grammar("query = x:foo => $x").unwrap()).is_err());
    }

    #[test]
    fn validate_rejects_multi_token_literal() {
        // "/fav" lexes into two tokens (/ , fav), so it could never match a
        // single token — a silent footgun, caught at load time.
        let g = parse_grammar("query = \"/fav\" => rating >= 4").unwrap();
        let err = validate(&g).unwrap_err();
        assert!(err.contains("/fav") && err.contains("token"), "unhelpful error: {err}");
    }

    #[test]
    fn validate_accepts_split_literals() {
        // The two-token form is the correct way to match `/fav`.
        let g = parse_grammar("query = \"/\" \"fav\" => rating >= 4").unwrap();
        assert!(validate(&g).is_ok());
    }

    #[test]
    fn validate_rejects_whitespace_only_literal() {
        let g = parse_grammar("query = \" \" => x").unwrap();
        assert!(validate(&g).is_err());
    }

    #[test]
    fn validate_rejects_direct_left_recursion() {
        let g = parse_grammar("query = q:query => $q").unwrap();
        assert!(validate(&g).is_err());
    }

    #[test]
    fn validate_rejects_indirect_left_recursion() {
        let g = parse_grammar("query = a:a => $a\na = b:b => $b\nb = a:a => $a").unwrap();
        assert!(validate(&g).is_err());
    }

    #[test]
    fn validate_allows_recursion_behind_a_token() {
        // Right/guarded recursion is fine: a token is consumed before the
        // self-reference.
        let g = parse_grammar("query = \"(\" q:query => $q\n      | WORD => x").unwrap();
        assert!(validate(&g).is_ok());
    }
}
