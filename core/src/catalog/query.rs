//! Tag search syntax.
//!
//! ```text
//! beach dog              both tags (terms separated by spaces are ANDed)
//! beach AND dog          explicit AND
//! (beach and dog)        grouping with parentheses
//! beach OR lake          either (AND binds tighter: `a b OR c` is `(a b) OR c`)
//! beach or lake          case-insensitive operators (and, or, not)
//! beach -dog             beach, without dog
//! beach NOT dog          negation
//! (beach OR lake) -dog   grouping
//! "birthday party"       quotes for spaces
//! person:alice           only tags of that kind (keyword, scene, object, person, place, event)
//! person:*               any tag of that kind: photos with someone recognized in them
//! -*                     untagged
//! ```
//!
//! Quote operators (`"OR"`, `"AND"`, `"NOT"`) to search for a tag of that name.
//!
//! A term matches a tag if it is one or more consecutive segments of the tag's path,
//! ignoring ASCII case: `alice`, `family`, `family|alice` and `people|family|alice` all
//! match `People|Family|Alice`. So searching a tag also finds everything under it
//! (`people` finds every person tagged under `People|…`).

use rusqlite::types::Value;

use super::TagKind;

#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    /// Matches everything (the empty query).
    All,
    Term { kind: Option<TagKind>, text: String },
    Not(Box<Expr>),
    And(Vec<Expr>),
    Or(Vec<Expr>),
}

impl Expr {
    /// Evaluates whether this expression matches a set of tags (path and optional kind).
    pub fn matches(&self, tags: &[(String, Option<TagKind>)]) -> bool {
        match self {
            Expr::All => true,
            Expr::Term { kind, text } => {
                tags.iter().any(|(t, k)| tag_matches(t, *k, *kind, text))
            }
            Expr::Not(inner) => !inner.matches(tags),
            Expr::And(items) => items.iter().all(|item| item.matches(tags)),
            Expr::Or(items) => items.iter().any(|item| item.matches(tags)),
        }
    }
}

fn tag_matches(tag_path: &str, tag_kind: Option<TagKind>, term_kind: Option<TagKind>, term_text: &str) -> bool {
    if let Some(expected_kind) = term_kind {
        if let Some(actual_kind) = tag_kind {
            if actual_kind != expected_kind {
                return false;
            }
        }
    }
    if term_text == "*" {
        return true;
    }
    let term_lc = term_text.to_ascii_lowercase();
    let tag_lc = tag_path.to_ascii_lowercase();
    if tag_lc == term_lc || tag_lc.contains(&term_lc) {
        return true;
    }
    let segs: Vec<&str> = tag_lc.split('|').map(str::trim).collect();
    for seg in segs {
        if seg == term_lc || seg.contains(&term_lc) {
            return true;
        }
    }
    false
}

#[derive(Debug, PartialEq)]
enum Token {
    Open,
    Close,
    And,
    Or,
    Not,
    Term { kind: Option<TagKind>, text: String },
}

fn tokenize(s: &str) -> Result<Vec<Token>, String> {
    let mut tokens = Vec::new();
    let mut chars = s.chars().peekable();
    while let Some(&c) = chars.peek() {
        match c {
            c if c.is_whitespace() || c == ',' || c == ';' => {
                chars.next();
            }
            '(' => {
                chars.next();
                tokens.push(Token::Open);
            }
            ')' => {
                chars.next();
                tokens.push(Token::Close);
            }
            '-' | '!' => {
                chars.next();
                tokens.push(Token::Not);
            }
            '&' => {
                chars.next();
                if chars.peek() == Some(&'&') {
                    chars.next();
                }
                tokens.push(Token::And);
            }
            _ => {
                let mut word = String::new();
                while let Some(&c) = chars.peek() {
                    if c.is_whitespace() || matches!(c, '(' | ')' | '"' | ',' | ';' | '&' | '!') {
                        break;
                    }
                    word.push(c);
                    chars.next();
                }
                let (kind, rest) = match word.split_once(':') {
                    Some((prefix, rest)) => match TagKind::from_name(prefix) {
                        Some(kind) => (Some(kind), rest.to_owned()),
                        None => (None, word.clone()),
                    },
                    None => (None, word.clone()),
                };
                if chars.peek() == Some(&'"') {
                    if !rest.is_empty() {
                        return Err(format!("unexpected quote after {word:?}"));
                    }
                    chars.next();
                    let mut text = String::new();
                    loop {
                        match chars.next() {
                            Some('"') => break,
                            Some(c) => text.push(c),
                            None => return Err("unclosed quote".into()),
                        }
                    }
                    tokens.push(Token::Term { kind, text });
                } else if kind.is_none() && (word == "OR" || word == "||") {
                    tokens.push(Token::Or);
                } else if kind.is_none() && (word.eq_ignore_ascii_case("AND") || word == "&&" || word == "&") {
                    tokens.push(Token::And);
                } else if kind.is_none() && (word == "NOT" || word == "not") {
                    tokens.push(Token::Not);
                } else if rest.is_empty() {
                    return Err(format!("{word:?} needs a tag after it"));
                } else {
                    tokens.push(Token::Term { kind, text: rest });
                }
            }
        }
    }
    Ok(tokens)
}

pub fn parse(s: &str) -> Result<Expr, String> {
    let tokens = tokenize(s)?;
    if tokens.is_empty() {
        return Ok(Expr::All);
    }
    let mut parser = Parser { tokens: tokens.into_iter().peekable() };
    let expr = parser.or()?;
    match parser.tokens.next() {
        None => Ok(expr),
        Some(Token::Close) => Err("unmatched ')'".into()),
        Some(token) => Err(format!("unexpected {token:?}")),
    }
}

struct Parser {
    tokens: std::iter::Peekable<std::vec::IntoIter<Token>>,
}

impl Parser {
    fn or(&mut self) -> Result<Expr, String> {
        let mut alternatives = vec![self.and()?];
        while self.tokens.next_if_eq(&Token::Or).is_some() {
            alternatives.push(self.and()?);
        }
        Ok(if alternatives.len() == 1 { alternatives.pop().unwrap() } else { Expr::Or(alternatives) })
    }

    fn and(&mut self) -> Result<Expr, String> {
        let mut all = Vec::new();
        loop {
            if !all.is_empty() && self.tokens.next_if_eq(&Token::And).is_some() {
                if !matches!(self.tokens.peek(), Some(Token::Open | Token::Not | Token::Term { .. })) {
                    return Err("AND needs a term after it".into());
                }
            }
            if matches!(self.tokens.peek(), Some(Token::Open | Token::Not | Token::Term { .. })) {
                all.push(self.unary()?);
            } else {
                break;
            }
        }
        match all.len() {
            0 => Err(match self.tokens.peek() {
                Some(Token::Or) => "OR needs a term on each side".into(),
                Some(Token::And) => "AND needs a term before it".into(),
                Some(Token::Close) => "empty or unmatched ')'".into(),
                _ => "missing term".into(),
            }),
            1 => Ok(all.pop().unwrap()),
            _ => Ok(Expr::And(all)),
        }
    }

    fn unary(&mut self) -> Result<Expr, String> {
        match self.tokens.next() {
            Some(Token::Not) => Ok(Expr::Not(Box::new(self.unary().map_err(|_| "negation needs a term after it".to_owned())?))),
            Some(Token::Open) => {
                let inner = self.or()?;
                match self.tokens.next() {
                    Some(Token::Close) => Ok(inner),
                    _ => Err("unclosed '('".into()),
                }
            }
            Some(Token::Term { kind, text }) => {
                let text = text.split('|').map(str::trim).collect::<Vec<_>>().join("|");
                if text.split('|').any(str::is_empty) {
                    return Err(format!("invalid tag {text:?}"));
                }
                Ok(Expr::Term { kind, text })
            }
            _ => Err("missing term".into()),
        }
    }
}

/// A condition on `assets a` and its parameters. `?1`, if any, is the minimum confidence.
pub(super) fn to_sql(expr: &Expr, min_confidence: f32) -> (String, Vec<Value>) {
    let mut sql = String::new();
    let mut values = Vec::new();
    write(expr, min_confidence, &mut sql, &mut values);
    (sql, values)
}

fn write(expr: &Expr, min_confidence: f32, sql: &mut String, values: &mut Vec<Value>) {
    let mut list = |items: &[Expr], sep: &str, sql: &mut String| {
        sql.push('(');
        for (i, item) in items.iter().enumerate() {
            if i > 0 {
                sql.push_str(sep);
            }
            write(item, min_confidence, sql, values);
        }
        sql.push(')');
    };
    match expr {
        Expr::All => sql.push('1'),
        Expr::And(items) => list(items, " AND ", sql),
        Expr::Or(items) => list(items, " OR ", sql),
        Expr::Not(inner) => {
            sql.push_str("NOT ");
            write(inner, min_confidence, sql, values);
        }
        Expr::Term { kind, text } => {
            if values.is_empty() {
                values.push(Value::Real(min_confidence.into()));
            }
            // A named tag selects a fraction of the library: collect its assets once from the
            // tag index. `*` selects nearly all of it, so probe each asset's own tags
            // instead. 200k assets × 6 tags: `tag5` 2 ms vs 800 ms probing; `-*` 3 s vs 140 ms.
            // CROSS JOIN fixes the join order to the one the comment above describes.
            if text == "*" {
                let join = if kind.is_some() { " CROSS JOIN tags g ON g.id = t.tag_id" } else { "" };
                sql.push_str(&format!("EXISTS (SELECT 1 FROM asset_tags t{join} WHERE t.asset_id = a.id AND "));
            } else {
                sql.push_str("a.id IN (SELECT t.asset_id FROM tags g CROSS JOIN asset_tags t ON t.tag_id = g.id WHERE ");
            }
            sql.push_str("NOT t.hidden AND (t.confidence IS NULL OR t.confidence >= ?1)");
            if text != "*" {
                values.push(Value::Text(format!("%|{}|%", escape_like(text))));
                sql.push_str(&format!(" AND ('|' || g.path || '|') LIKE ?{} ESCAPE '\\'", values.len()));
            }
            if let Some(kind) = kind {
                values.push(Value::Text(kind.name().into()));
                sql.push_str(&format!(" AND g.kind = ?{}", values.len()));
            }
            sql.push(')');
        }
    }
}

fn escape_like(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}
