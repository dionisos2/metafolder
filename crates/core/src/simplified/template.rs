//! Output templates (spec-query "Template language"). A template is literal
//! text with `$name` interpolations and `{ expr }` evaluated blocks. The base
//! type is text; captures are text; conversion to a number is explicit
//! (`num`), so there is no implicit coercion.

use std::collections::HashMap;

/// The value bound to a capture during matching: a single text, or the list of
/// texts produced by a repetition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Capture {
    Text(String),
    List(Vec<String>),
}

/// Map of capture label → value, consumed by [`render`].
pub type Captures = HashMap<String, Capture>;

/// A parsed template (opaque; build with [`parse_template`], use with
/// [`render`]).
#[derive(Debug, Clone, PartialEq)]
pub struct Template {
    parts: Vec<TplPart>,
}

#[derive(Debug, Clone, PartialEq)]
enum TplPart {
    Text(String),
    Var(String),
    Eval(TExpr),
}

#[derive(Debug, Clone, PartialEq)]
enum TExpr {
    Var(String),
    NumLit(f64),
    StrLit(String),
    Bin(Box<TExpr>, char, Box<TExpr>),
    Call(String, Vec<TExpr>),
}

/// Parses template source.
pub fn parse_template(src: &str) -> Result<Template, String> {
    let chars: Vec<char> = src.chars().collect();
    let mut parts = Vec::new();
    let mut text = String::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '$' => {
                flush(&mut parts, &mut text);
                i += 1;
                let start = i;
                while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                    i += 1;
                }
                if i == start {
                    return Err("expected a name after '$'".into());
                }
                parts.push(TplPart::Var(chars[start..i].iter().collect()));
            }
            '{' => {
                flush(&mut parts, &mut text);
                i += 1;
                let start = i;
                let mut depth = 1;
                let mut in_str = false;
                let mut escaped = false;
                while i < chars.len() {
                    let c = chars[i];
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
                    } else if c == '{' {
                        depth += 1;
                    } else if c == '}' {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    i += 1;
                }
                if i >= chars.len() {
                    return Err("unterminated '{' in template".into());
                }
                let inner: String = chars[start..i].iter().collect();
                i += 1; // skip '}'
                parts.push(TplPart::Eval(parse_texpr(&inner)?));
            }
            c => {
                text.push(c);
                i += 1;
            }
        }
    }
    flush(&mut parts, &mut text);
    Ok(Template { parts })
}

fn flush(parts: &mut Vec<TplPart>, text: &mut String) {
    if !text.is_empty() {
        parts.push(TplPart::Text(std::mem::take(text)));
    }
}

/// Renders a template against the given captures.
pub fn render(t: &Template, caps: &Captures) -> Result<String, String> {
    let mut out = String::new();
    for part in &t.parts {
        match part {
            TplPart::Text(s) => out.push_str(s),
            TplPart::Var(name) => match lookup(caps, name)? {
                Capture::Text(s) => out.push_str(s),
                Capture::List(_) => {
                    return Err(format!("cannot interpolate list '${name}' directly; use join()"))
                }
            },
            TplPart::Eval(e) => out.push_str(&render_value(eval(e, caps)?)?),
        }
    }
    Ok(out)
}

fn lookup<'a>(caps: &'a Captures, name: &str) -> Result<&'a Capture, String> {
    caps.get(name).ok_or_else(|| format!("unknown capture '${name}'"))
}

// ── Evaluation ──────────────────────────────────────────────────────────────

enum TValue {
    Text(String),
    Num(f64),
    List(Vec<String>),
}

fn eval(e: &TExpr, caps: &Captures) -> Result<TValue, String> {
    match e {
        TExpr::Var(name) => Ok(match lookup(caps, name)? {
            Capture::Text(s) => TValue::Text(s.clone()),
            Capture::List(items) => TValue::List(items.clone()),
        }),
        TExpr::NumLit(n) => Ok(TValue::Num(*n)),
        TExpr::StrLit(s) => Ok(TValue::Text(s.clone())),
        TExpr::Bin(a, op, b) => {
            let x = as_num(eval(a, caps)?)?;
            let y = as_num(eval(b, caps)?)?;
            let r = match op {
                '+' => x + y,
                '-' => x - y,
                '*' => x * y,
                '/' => {
                    if y == 0.0 {
                        return Err("division by zero".into());
                    }
                    x / y
                }
                '%' => {
                    if y == 0.0 {
                        return Err("modulo by zero".into());
                    }
                    x % y
                }
                _ => return Err(format!("unknown operator '{op}'")),
            };
            Ok(TValue::Num(r))
        }
        TExpr::Call(name, args) => eval_call(name, args, caps),
    }
}

fn eval_call(name: &str, args: &[TExpr], caps: &Captures) -> Result<TValue, String> {
    let arity = |n: usize| -> Result<(), String> {
        if args.len() == n {
            Ok(())
        } else {
            Err(format!("{name}() takes {n} argument(s), got {}", args.len()))
        }
    };
    match name {
        "num" => {
            arity(1)?;
            match eval(&args[0], caps)? {
                TValue::Num(n) => Ok(TValue::Num(n)),
                TValue::Text(s) => s
                    .trim()
                    .parse::<f64>()
                    .map(TValue::Num)
                    .map_err(|_| format!("num(): '{s}' is not a number")),
                TValue::List(_) => Err("num() expects text, got a list".into()),
            }
        }
        "str" => {
            arity(1)?;
            let text = match eval(&args[0], caps)? {
                TValue::Text(s) => s,
                TValue::Num(n) => fmt_num(n),
                TValue::List(_) => return Err("str() expects text, got a list".into()),
            };
            Ok(TValue::Text(quote_dsl(&text)))
        }
        "join" => {
            arity(2)?;
            let sep = match eval(&args[0], caps)? {
                TValue::Text(s) => s,
                _ => return Err("join(): separator must be text".into()),
            };
            let list = match eval(&args[1], caps)? {
                TValue::List(items) => items,
                _ => return Err("join(): second argument must be a list".into()),
            };
            Ok(TValue::Text(list.join(&sep)))
        }
        other => Err(format!("unknown function '{other}()'")),
    }
}

fn as_num(v: TValue) -> Result<f64, String> {
    match v {
        TValue::Num(n) => Ok(n),
        TValue::Text(_) => Err("expected a number — convert text with num()".into()),
        TValue::List(_) => Err("expected a number, got a list".into()),
    }
}

fn render_value(v: TValue) -> Result<String, String> {
    match v {
        TValue::Text(s) => Ok(s),
        TValue::Num(n) => Ok(fmt_num(n)),
        TValue::List(_) => Err("a { } block produced a list; join() it into text".into()),
    }
}

/// Formats a number: an integer when integral, otherwise the float form.
fn fmt_num(v: f64) -> String {
    if v.is_finite() && v.fract() == 0.0 && v.abs() < 9e15 {
        format!("{}", v as i64)
    } else {
        format!("{v}")
    }
}

/// Renders text as a DSL string literal — idempotent: leaves an already-quoted
/// string untouched, otherwise wraps and escapes.
fn quote_dsl(s: &str) -> String {
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        if c == '"' || c == '\\' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

// ── `{ }` expression parser ─────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq)]
enum ETok {
    Var(String),
    Num(f64),
    Str(String),
    Ident(String),
    LParen,
    RParen,
    Comma,
    Op(char),
}

fn lex_texpr(s: &str) -> Result<Vec<ETok>, String> {
    let chars: Vec<char> = s.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '$' {
            i += 1;
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            if i == start {
                return Err("expected a name after '$' in { }".into());
            }
            toks.push(ETok::Var(chars[start..i].iter().collect()));
        } else if c.is_ascii_digit() {
            let start = i;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            if chars.get(i) == Some(&'.') && chars.get(i + 1).is_some_and(|d| d.is_ascii_digit()) {
                i += 1;
                while i < chars.len() && chars[i].is_ascii_digit() {
                    i += 1;
                }
            }
            let text: String = chars[start..i].iter().collect();
            toks.push(ETok::Num(text.parse().map_err(|_| format!("bad number '{text}'"))?));
        } else if c == '"' {
            let mut text = String::new();
            i += 1;
            loop {
                match chars.get(i) {
                    None => return Err("unterminated string in { }".into()),
                    Some('\\') => {
                        match chars.get(i + 1) {
                            Some(&'"') => text.push('"'),
                            Some(&'\\') => text.push('\\'),
                            Some(&'n') => text.push('\n'),
                            Some(&'t') => text.push('\t'),
                            Some(&other) => text.push(other),
                            None => return Err("dangling escape in { } string".into()),
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
            toks.push(ETok::Str(text));
        } else if c.is_ascii_alphabetic() || c == '_' {
            let start = i;
            while i < chars.len() && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                i += 1;
            }
            toks.push(ETok::Ident(chars[start..i].iter().collect()));
        } else {
            match c {
                '(' => toks.push(ETok::LParen),
                ')' => toks.push(ETok::RParen),
                ',' => toks.push(ETok::Comma),
                '+' | '-' | '*' | '/' | '%' => toks.push(ETok::Op(c)),
                other => return Err(format!("unexpected character '{other}' in {{ }}")),
            }
            i += 1;
        }
    }
    Ok(toks)
}

fn parse_texpr(src: &str) -> Result<TExpr, String> {
    let toks = lex_texpr(src)?;
    let mut p = EParser { toks, pos: 0 };
    let e = p.additive()?;
    if p.pos != p.toks.len() {
        return Err(format!("unexpected trailing input in {{ }}: {:?}", p.toks[p.pos]));
    }
    Ok(e)
}

struct EParser {
    toks: Vec<ETok>,
    pos: usize,
}

impl EParser {
    fn peek(&self) -> Option<&ETok> {
        self.toks.get(self.pos)
    }
    fn advance(&mut self) -> Option<ETok> {
        let t = self.toks.get(self.pos).cloned();
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    fn additive(&mut self) -> Result<TExpr, String> {
        let mut e = self.multiplicative()?;
        while let Some(ETok::Op(op @ ('+' | '-'))) = self.peek() {
            let op = *op;
            self.advance();
            let rhs = self.multiplicative()?;
            e = TExpr::Bin(Box::new(e), op, Box::new(rhs));
        }
        Ok(e)
    }

    fn multiplicative(&mut self) -> Result<TExpr, String> {
        let mut e = self.primary()?;
        while let Some(ETok::Op(op @ ('*' | '/' | '%'))) = self.peek() {
            let op = *op;
            self.advance();
            let rhs = self.primary()?;
            e = TExpr::Bin(Box::new(e), op, Box::new(rhs));
        }
        Ok(e)
    }

    fn primary(&mut self) -> Result<TExpr, String> {
        match self.advance() {
            Some(ETok::Num(n)) => Ok(TExpr::NumLit(n)),
            Some(ETok::Str(s)) => Ok(TExpr::StrLit(s)),
            Some(ETok::Var(name)) => Ok(TExpr::Var(name)),
            Some(ETok::Ident(name)) => {
                match self.advance() {
                    Some(ETok::LParen) => {}
                    _ => return Err(format!("'{name}' must be a function call (did you mean ${name}?)")),
                }
                let mut args = Vec::new();
                if !matches!(self.peek(), Some(ETok::RParen)) {
                    args.push(self.additive()?);
                    while matches!(self.peek(), Some(ETok::Comma)) {
                        self.advance();
                        args.push(self.additive()?);
                    }
                }
                match self.advance() {
                    Some(ETok::RParen) => {}
                    _ => return Err("expected ')' in { }".into()),
                }
                Ok(TExpr::Call(name, args))
            }
            Some(ETok::LParen) => {
                let e = self.additive()?;
                match self.advance() {
                    Some(ETok::RParen) => {}
                    _ => return Err("expected ')' in { }".into()),
                }
                Ok(e)
            }
            other => Err(format!("unexpected {other:?} in {{ }}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> Capture {
        Capture::Text(s.into())
    }
    fn l(items: &[&str]) -> Capture {
        Capture::List(items.iter().map(|s| s.to_string()).collect())
    }
    fn cap(pairs: Vec<(&str, Capture)>) -> Captures {
        pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
    }
    fn rs(src: &str, caps: Captures) -> Result<String, String> {
        render(&parse_template(src)?, &caps)
    }

    #[test]
    fn var_text() {
        assert_eq!(rs("$x", cap(vec![("x", t("hi"))])).unwrap(), "hi");
    }

    #[test]
    fn literal_and_var() {
        assert_eq!(rs("NOT $u", cap(vec![("u", t("a"))])).unwrap(), "NOT a");
    }

    #[test]
    fn equality_template() {
        assert_eq!(
            rs("$f = $v", cap(vec![("f", t("genre")), ("v", t("\"jazz\""))])).unwrap(),
            "genre = \"jazz\""
        );
    }

    #[test]
    fn unit_arithmetic() {
        assert_eq!(rs("{num($n) * 1048576}", cap(vec![("n", t("100"))])).unwrap(), "104857600");
    }

    #[test]
    fn str_quotes_bareword() {
        assert_eq!(rs("{str($w)}", cap(vec![("w", t("jazz"))])).unwrap(), "\"jazz\"");
    }

    #[test]
    fn str_idempotent_on_quoted() {
        assert_eq!(
            rs("{str($w)}", cap(vec![("w", t("\"jazz blues\""))])).unwrap(),
            "\"jazz blues\""
        );
    }

    #[test]
    fn join_list() {
        assert_eq!(
            rs("{join(\" OR \", $items)}", cap(vec![("items", l(&["a", "b", "c"]))])).unwrap(),
            "a OR b OR c"
        );
    }

    #[test]
    fn arithmetic_precedence() {
        assert_eq!(
            rs(
                "{num($a) + num($b) * num($c)}",
                cap(vec![("a", t("2")), ("b", t("3")), ("c", t("4"))])
            )
            .unwrap(),
            "14"
        );
    }

    #[test]
    fn float_division() {
        assert_eq!(rs("{num($n) / 2}", cap(vec![("n", t("5"))])).unwrap(), "2.5");
    }

    #[test]
    fn surrounding_text_and_eval() {
        assert_eq!(
            rs("mfr_path ->* {str($v)}", cap(vec![("v", t("/m"))])).unwrap(),
            "mfr_path ->* \"/m\""
        );
    }

    #[test]
    fn no_implicit_coercion() {
        // '+' requires numbers; a text capture without num() is an error.
        assert!(rs("{$a + $b}", cap(vec![("a", t("1")), ("b", t("2"))])).is_err());
    }

    #[test]
    fn num_on_nonnumeric_errors() {
        assert!(rs("{num($x)}", cap(vec![("x", t("abc"))])).is_err());
    }

    #[test]
    fn interpolating_list_directly_errors() {
        assert!(rs("$items", cap(vec![("items", l(&["a"]))])).is_err());
    }

    #[test]
    fn missing_capture_errors() {
        assert!(rs("$x", cap(vec![])).is_err());
    }

    #[test]
    fn unterminated_brace_errors() {
        assert!(parse_template("{num($n)").is_err());
    }
}
