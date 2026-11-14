//! Lexical analysis.
//!
//! ORBIT keywords are *contextual*: the lexer emits every word as an
//! [`TokenKind::Ident`] and the parser decides whether a word such as
//! `step` or `timeout` is a keyword based on its position. This keeps
//! common words (`input`, `version`, `status`, ...) usable as field names.
//! Only `true`, `false` and `null` are reserved.

use crate::diagnostic::Diagnostic;
use crate::source::{FileId, Span};

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    Ident(String),
    Str(String),
    Int(i64),
    Float(f64),
    /// A duration literal such as `10s`, normalised to milliseconds.
    Duration(u64),
    True,
    False,
    Null,
    LBrace,
    RBrace,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Lt,
    Gt,
    Le,
    Ge,
    EqEq,
    NotEq,
    Eq,
    Comma,
    Colon,
    Semi,
    Dot,
    Question,
    Bang,
    AndAnd,
    OrOr,
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Eof,
}

impl TokenKind {
    pub fn describe(&self) -> String {
        match self {
            TokenKind::Ident(s) => format!("`{s}`"),
            TokenKind::Str(_) => "string literal".into(),
            TokenKind::Int(_) => "integer literal".into(),
            TokenKind::Float(_) => "float literal".into(),
            TokenKind::Duration(_) => "duration literal".into(),
            TokenKind::True => "`true`".into(),
            TokenKind::False => "`false`".into(),
            TokenKind::Null => "`null`".into(),
            TokenKind::Eof => "end of file".into(),
            other => format!("`{}`", other.symbol()),
        }
    }

    pub fn symbol(&self) -> &'static str {
        match self {
            TokenKind::LBrace => "{",
            TokenKind::RBrace => "}",
            TokenKind::LParen => "(",
            TokenKind::RParen => ")",
            TokenKind::LBracket => "[",
            TokenKind::RBracket => "]",
            TokenKind::Lt => "<",
            TokenKind::Gt => ">",
            TokenKind::Le => "<=",
            TokenKind::Ge => ">=",
            TokenKind::EqEq => "==",
            TokenKind::NotEq => "!=",
            TokenKind::Eq => "=",
            TokenKind::Comma => ",",
            TokenKind::Colon => ":",
            TokenKind::Semi => ";",
            TokenKind::Dot => ".",
            TokenKind::Question => "?",
            TokenKind::Bang => "!",
            TokenKind::AndAnd => "&&",
            TokenKind::OrOr => "||",
            TokenKind::Plus => "+",
            TokenKind::Minus => "-",
            TokenKind::Star => "*",
            TokenKind::Slash => "/",
            TokenKind::Percent => "%",
            _ => "?",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
    /// True when at least one newline separates this token from the previous one.
    /// Infix operators must be on the same line as their left operand, which lets
    /// `a: 1 \n b: -2` parse as two fields rather than `1 - 2`.
    pub newline_before: bool,
}

pub struct Lexer<'a> {
    file: FileId,
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
    diagnostics: Vec<Diagnostic>,
}

/// Tokenise a whole file. Lexical errors are reported but lexing continues so
/// that the parser can report as many problems as possible in one run.
pub fn tokenize(file: FileId, src: &str) -> (Vec<Token>, Vec<Diagnostic>) {
    let mut lx = Lexer { file, src, bytes: src.as_bytes(), pos: 0, diagnostics: vec![] };
    let mut tokens = vec![];
    let mut newline = true;
    loop {
        newline |= lx.skip_trivia();
        let start = lx.pos;
        let Some(kind) = lx.next_kind() else {
            tokens.push(Token { kind: TokenKind::Eof, span: lx.span(start, start), newline_before: newline });
            break;
        };
        if let Some(kind) = kind {
            tokens.push(Token { kind, span: lx.span(start, lx.pos), newline_before: newline });
            newline = false;
        }
    }
    (tokens, lx.diagnostics)
}

impl<'a> Lexer<'a> {
    fn span(&self, start: usize, end: usize) -> Span {
        Span::new(self.file, start, end)
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn peek2(&self) -> Option<u8> {
        self.bytes.get(self.pos + 1).copied()
    }

    /// Skip whitespace and comments; returns whether a newline was crossed.
    fn skip_trivia(&mut self) -> bool {
        let mut newline = false;
        loop {
            match self.peek() {
                Some(b'\n') => {
                    newline = true;
                    self.pos += 1;
                }
                Some(c) if c.is_ascii_whitespace() => self.pos += 1,
                Some(b'/') if self.peek2() == Some(b'/') => {
                    while let Some(c) = self.peek() {
                        if c == b'\n' {
                            break;
                        }
                        self.pos += 1;
                    }
                }
                Some(b'/') if self.peek2() == Some(b'*') => {
                    let start = self.pos;
                    self.pos += 2;
                    let mut depth = 1;
                    while depth > 0 {
                        match (self.peek(), self.peek2()) {
                            (None, _) => {
                                self.diagnostics.push(
                                    Diagnostic::error("E0003", "unterminated block comment", self.span(start, start + 2))
                                        .with_help("close the comment with `*/`"),
                                );
                                return newline;
                            }
                            (Some(b'*'), Some(b'/')) => {
                                depth -= 1;
                                self.pos += 2;
                            }
                            (Some(b'/'), Some(b'*')) => {
                                depth += 1;
                                self.pos += 2;
                            }
                            (Some(c), _) => {
                                if c == b'\n' {
                                    newline = true;
                                }
                                self.pos += 1;
                            }
                        }
                    }
                }
                _ => return newline,
            }
        }
    }

    /// `None` = end of input; `Some(None)` = an invalid character was skipped.
    fn next_kind(&mut self) -> Option<Option<TokenKind>> {
        let start = self.pos;
        let c = self.peek()?;
        let two = |lx: &mut Self, k| {
            lx.pos += 2;
            Some(Some(k))
        };
        let one = |lx: &mut Self, k| {
            lx.pos += 1;
            Some(Some(k))
        };
        match (c, self.peek2()) {
            (b'=', Some(b'=')) => two(self, TokenKind::EqEq),
            (b'!', Some(b'=')) => two(self, TokenKind::NotEq),
            (b'<', Some(b'=')) => two(self, TokenKind::Le),
            (b'>', Some(b'=')) => two(self, TokenKind::Ge),
            (b'&', Some(b'&')) => two(self, TokenKind::AndAnd),
            (b'|', Some(b'|')) => two(self, TokenKind::OrOr),
            (b'{', _) => one(self, TokenKind::LBrace),
            (b'}', _) => one(self, TokenKind::RBrace),
            (b'(', _) => one(self, TokenKind::LParen),
            (b')', _) => one(self, TokenKind::RParen),
            (b'[', _) => one(self, TokenKind::LBracket),
            (b']', _) => one(self, TokenKind::RBracket),
            (b'<', _) => one(self, TokenKind::Lt),
            (b'>', _) => one(self, TokenKind::Gt),
            (b'=', _) => one(self, TokenKind::Eq),
            (b',', _) => one(self, TokenKind::Comma),
            (b':', _) => one(self, TokenKind::Colon),
            (b';', _) => one(self, TokenKind::Semi),
            (b'.', _) => one(self, TokenKind::Dot),
            (b'?', _) => one(self, TokenKind::Question),
            (b'!', _) => one(self, TokenKind::Bang),
            (b'+', _) => one(self, TokenKind::Plus),
            (b'-', _) => one(self, TokenKind::Minus),
            (b'*', _) => one(self, TokenKind::Star),
            (b'/', _) => one(self, TokenKind::Slash),
            (b'%', _) => one(self, TokenKind::Percent),
            (b'"', _) => Some(self.string()),
            (c, _) if c.is_ascii_digit() => Some(self.number()),
            (c, _) if c.is_ascii_alphabetic() || c == b'_' => {
                while matches!(self.peek(), Some(c) if c.is_ascii_alphanumeric() || c == b'_') {
                    self.pos += 1;
                }
                let word = &self.src[start..self.pos];
                Some(Some(match word {
                    "true" => TokenKind::True,
                    "false" => TokenKind::False,
                    "null" => TokenKind::Null,
                    _ => TokenKind::Ident(word.to_string()),
                }))
            }
            _ => {
                let ch = self.src[start..].chars().next().unwrap();
                self.pos += ch.len_utf8();
                self.diagnostics.push(Diagnostic::error(
                    "E0001",
                    format!("unexpected character `{}`", ch.escape_default()),
                    self.span(start, self.pos),
                ));
                Some(None)
            }
        }
    }

    fn string(&mut self) -> Option<TokenKind> {
        let start = self.pos;
        self.pos += 1;
        let mut out = String::new();
        loop {
            let Some(c) = self.src[self.pos..].chars().next() else {
                self.diagnostics.push(
                    Diagnostic::error("E0002", "unterminated string literal", self.span(start, self.pos))
                        .with_help("add a closing `\"`"),
                );
                return Some(TokenKind::Str(out));
            };
            match c {
                '"' => {
                    self.pos += 1;
                    return Some(TokenKind::Str(out));
                }
                '\n' => {
                    self.diagnostics.push(
                        Diagnostic::error("E0002", "unterminated string literal", self.span(start, self.pos))
                            .with_help("string literals cannot span lines; use `\\n`"),
                    );
                    return Some(TokenKind::Str(out));
                }
                '\\' => {
                    let esc_start = self.pos;
                    self.pos += 1;
                    let e = self.src[self.pos..].chars().next();
                    match e {
                        Some('n') => out.push('\n'),
                        Some('t') => out.push('\t'),
                        Some('r') => out.push('\r'),
                        Some('"') => out.push('"'),
                        Some('\\') => out.push('\\'),
                        Some('u') if self.src[self.pos..].starts_with("u{") => {
                            let rest = &self.src[self.pos + 2..];
                            if let Some(close) = rest.find('}') {
                                let hex = &rest[..close];
                                if let Some(ch) = u32::from_str_radix(hex, 16).ok().and_then(char::from_u32) {
                                    out.push(ch);
                                    self.pos += 2 + close; // the final `}` is consumed below
                                } else {
                                    self.diagnostics.push(Diagnostic::error(
                                        "E0004",
                                        format!("invalid unicode escape `\\u{{{hex}}}`"),
                                        self.span(esc_start, self.pos + 3 + close),
                                    ));
                                    self.pos += 2 + close;
                                }
                            }
                        }
                        Some(other) => self.diagnostics.push(
                            Diagnostic::error(
                                "E0004",
                                format!("unknown escape sequence `\\{other}`"),
                                self.span(esc_start, self.pos + other.len_utf8()),
                            )
                            .with_help("supported escapes: \\n \\t \\r \\\" \\\\ \\u{XXXX}"),
                        ),
                        None => continue,
                    }
                    if let Some(e) = self.src[self.pos..].chars().next() {
                        self.pos += e.len_utf8();
                    }
                }
                c => {
                    out.push(c);
                    self.pos += c.len_utf8();
                }
            }
        }
    }

    fn number(&mut self) -> Option<TokenKind> {
        let start = self.pos;
        let digits = |lx: &mut Self| {
            while matches!(lx.peek(), Some(c) if c.is_ascii_digit() || c == b'_') {
                lx.pos += 1;
            }
        };
        digits(self);
        let mut is_float = false;
        if self.peek() == Some(b'.') && matches!(self.peek2(), Some(c) if c.is_ascii_digit()) {
            is_float = true;
            self.pos += 1;
            digits(self);
        }
        let num_end = self.pos;
        let text: String = self.src[start..num_end].chars().filter(|c| *c != '_').collect();
        // Duration suffix?
        let unit_start = self.pos;
        while matches!(self.peek(), Some(c) if c.is_ascii_alphabetic()) {
            self.pos += 1;
        }
        let unit = &self.src[unit_start..self.pos];
        if !unit.is_empty() {
            let factor: f64 = match unit {
                "ms" => 1.0,
                "s" => 1_000.0,
                "m" => 60_000.0,
                "h" => 3_600_000.0,
                "d" => 86_400_000.0,
                _ => {
                    self.diagnostics.push(
                        Diagnostic::error(
                            "E0005",
                            format!("unknown duration unit `{unit}`"),
                            self.span(unit_start, self.pos),
                        )
                        .with_help("valid units are ms, s, m, h and d (for example `30s`)"),
                    );
                    return Some(TokenKind::Duration(0));
                }
            };
            let value: f64 = text.parse().unwrap_or(0.0);
            return Some(TokenKind::Duration((value * factor).round() as u64));
        }
        if is_float {
            Some(TokenKind::Float(text.parse().unwrap_or(0.0)))
        } else {
            match text.parse::<i64>() {
                Ok(v) => Some(TokenKind::Int(v)),
                Err(_) => {
                    self.diagnostics.push(Diagnostic::error(
                        "E0006",
                        "integer literal does not fit in a signed 64-bit integer",
                        self.span(start, num_end),
                    ));
                    Some(TokenKind::Int(0))
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<TokenKind> {
        let (t, d) = tokenize(0, src);
        assert!(d.is_empty(), "{d:?}");
        t.into_iter().map(|t| t.kind).collect()
    }

    #[test]
    fn all_token_categories() {
        use TokenKind::*;
        assert_eq!(
            kinds(r#"workflow X { a: "s\n" 1 2.5 10s 500ms 2m 1h true false null } // c
            /* multi
               line */ == != <= >= && || ! < > = , : . ? + - * / % ( ) [ ] ;"#),
            vec![
                Ident("workflow".into()),
                Ident("X".into()),
                LBrace,
                Ident("a".into()),
                Colon,
                Str("s\n".into()),
                Int(1),
                Float(2.5),
                Duration(10_000),
                Duration(500),
                Duration(120_000),
                Duration(3_600_000),
                True,
                False,
                Null,
                RBrace,
                EqEq,
                NotEq,
                Le,
                Ge,
                AndAnd,
                OrOr,
                Bang,
                Lt,
                Gt,
                Eq,
                Comma,
                Colon,
                Dot,
                Question,
                Plus,
                Minus,
                Star,
                Slash,
                Percent,
                LParen,
                RParen,
                LBracket,
                RBracket,
                Semi,
                Eof
            ]
        );
    }

    #[test]
    fn tracks_newlines() {
        let (t, _) = tokenize(0, "a\n-b");
        assert!(!t[0].newline_before || t[0].span.start == 0);
        assert!(t[1].newline_before);
        assert!(!t[2].newline_before);
    }

    #[test]
    fn reports_errors_with_spans() {
        let (_, d) = tokenize(0, "\"open\nx 5q @");
        let codes: Vec<_> = d.iter().map(|d| d.code).collect();
        assert_eq!(codes, vec!["E0002", "E0005", "E0001"]);
        assert_eq!(d[2].span.start, 11);
    }

    #[test]
    fn unicode_escape() {
        assert_eq!(kinds(r#""\u{1F680}""#)[0], TokenKind::Str("🚀".into()));
    }
}
