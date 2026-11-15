//! `orbit fmt`: a conservative, comment-preserving formatter.
//!
//! The formatter normalises indentation (4 spaces per nesting level), spacing
//! between tokens, trailing whitespace and blank lines (at most one in a row).
//! It keeps the author's line breaks and every comment, so it never reflows
//! code. Before returning, it re-lexes its own output and checks that the
//! token sequence (including the line-break information the grammar depends
//! on) and the comments are unchanged; if not, it refuses to format.

use crate::lexer::{tokenize_with_comments, Comment, Token, TokenKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatError {
    /// The source has lexical errors; fix them first.
    Lex(String),
    /// Internal safety check failed: formatting would have changed meaning.
    Unsafe(String),
}

impl std::fmt::Display for FormatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FormatError::Lex(m) => write!(f, "cannot format a file with lexical errors: {m}"),
            FormatError::Unsafe(m) => write!(
                f,
                "formatter safety check failed ({m}); the file was left unchanged"
            ),
        }
    }
}

enum Item<'a> {
    Tok(&'a Token),
    Comment(&'a Comment),
}

impl Item<'_> {
    fn start(&self) -> usize {
        match self {
            Item::Tok(t) => t.span.start as usize,
            Item::Comment(c) => c.span.start as usize,
        }
    }
    fn end(&self) -> usize {
        match self {
            Item::Tok(t) => t.span.end as usize,
            Item::Comment(c) => c.span.end as usize,
        }
    }
}

const NO_CALL_WORDS: &[&str] = &[
    "if", "then", "else", "when", "return", "with", "in", "for", "after", "using",
];

fn is_infix(k: &TokenKind) -> bool {
    matches!(
        k,
        TokenKind::EqEq
            | TokenKind::NotEq
            | TokenKind::Le
            | TokenKind::Ge
            | TokenKind::AndAnd
            | TokenKind::OrOr
            | TokenKind::Plus
            | TokenKind::Minus
            | TokenKind::Star
            | TokenKind::Slash
            | TokenKind::Percent
            | TokenKind::Lt
            | TokenKind::Gt
            | TokenKind::Eq
    )
}

/// Format ORBIT source text.
pub fn format_source(src: &str) -> Result<String, FormatError> {
    // Files checked out with Windows line endings keep them.
    if src.contains("\r\n") {
        return format_lf(&src.replace("\r\n", "\n")).map(|out| out.replace('\n', "\r\n"));
    }
    format_lf(src)
}

fn format_lf(src: &str) -> Result<String, FormatError> {
    let (tokens, comments, diags) = tokenize_with_comments(0, src);
    if let Some(d) = diags.first() {
        return Err(FormatError::Lex(d.message.clone()));
    }
    let tokens: Vec<&Token> = tokens.iter().filter(|t| t.kind != TokenKind::Eof).collect();
    let mut items: Vec<Item> = tokens
        .iter()
        .map(|t| Item::Tok(t))
        .chain(comments.iter().map(Item::Comment))
        .collect();
    items.sort_by_key(|i| i.start());

    let mut out = String::new();
    let mut line = String::new();
    // Open delimiters ({, (, [) for indentation; generic `<` depth for types.
    let mut depth: usize = 0;
    let mut generic: usize = 0;
    let mut prev: Option<&Item> = None;
    let mut prev_tok: Option<&TokenKind> = None;
    let mut prev_word: Option<String> = None;
    // Did the previous token close a generic (`List<T>`)? Then it is not an operator.
    let mut prev_closed_generic = false;
    // Was the previous token a unary minus (`-1`, `-x`)? Then no space follows.
    let mut prev_unary_minus = false;

    let flush = |out: &mut String, line: &mut String| {
        out.push_str(line.trim_end());
        out.push('\n');
        line.clear();
    };

    for item in &items {
        let gap = &src[prev.map(|p| p.end()).unwrap_or(0)..item.start()];
        let newlines = gap.matches('\n').count();
        let starts_line = prev.is_none() || newlines > 0;
        if starts_line && prev.is_some() {
            flush(&mut out, &mut line);
            // At most one blank line, and none right after an opening brace.
            let after_open = matches!(
                prev_tok,
                Some(TokenKind::LBrace | TokenKind::LParen | TokenKind::LBracket)
            ) && matches!(prev, Some(Item::Tok(_)));
            let before_close = matches!(item, Item::Tok(t) if matches!(t.kind, TokenKind::RBrace | TokenKind::RParen | TokenKind::RBracket));
            if newlines >= 2 && !after_open && !before_close {
                out.push('\n');
            }
        }
        match item {
            Item::Comment(c) => {
                let text = &src[c.span.start as usize..c.span.end as usize];
                if starts_line {
                    line.push_str(&"    ".repeat(depth));
                } else {
                    line.push(' ');
                }
                if c.block && text.contains('\n') {
                    // Multi-line block comments are kept verbatim.
                    line.push_str(text);
                } else {
                    line.push_str(text.trim_end());
                }
            }
            Item::Tok(t) => {
                let k = &t.kind;
                let text = &src[t.span.start as usize..t.span.end as usize];
                let closing = matches!(
                    k,
                    TokenKind::RBrace | TokenKind::RParen | TokenKind::RBracket
                );
                if closing {
                    depth = depth.saturating_sub(1);
                }
                if starts_line {
                    // Continuation lines of an expression get one extra level.
                    let word = matches!(k, TokenKind::Ident(w) if w == "then" || w == "else");
                    let operator_before = prev_tok.is_some_and(is_infix) && !prev_closed_generic;
                    let continues =
                        operator_before || word || (is_infix(k) && !matches!(k, TokenKind::Minus));
                    let extra = usize::from(continues && !closing);
                    line.push_str(&"    ".repeat(depth + extra));
                } else if let Some(p) = prev_tok {
                    let after_comment = matches!(prev, Some(Item::Comment(_)));
                    if !(prev_unary_minus && !after_comment)
                        && needs_space(p, k, prev_word.as_deref(), generic, after_comment)
                    {
                        line.push(' ');
                    }
                }
                // A minus is unary at the start of a line or after an operator,
                // an opener, a separator or a keyword such as `return`.
                prev_unary_minus = matches!(k, TokenKind::Minus)
                    && (starts_line
                        || prev_tok.is_none_or(|p| {
                            is_infix(p)
                                || matches!(p, TokenKind::Comma | TokenKind::Colon | TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace)
                                || matches!(p, TokenKind::Ident(w) if NO_CALL_WORDS.contains(&w.as_str()))
                        }));
                line.push_str(text);
                prev_closed_generic = false;
                match k {
                    TokenKind::LBrace | TokenKind::LParen | TokenKind::LBracket => depth += 1,
                    TokenKind::Lt
                        if prev_word
                            .as_deref()
                            .is_some_and(|w| w == "List" || w == "Map") =>
                    {
                        generic += 1
                    }
                    TokenKind::Gt if generic > 0 => {
                        generic -= 1;
                        prev_closed_generic = true;
                    }
                    _ => {}
                }
                prev_word = match k {
                    TokenKind::Ident(w) => Some(w.clone()),
                    _ => None,
                };
                prev_tok = Some(k);
            }
        }
        prev = Some(item);
    }
    if !line.trim().is_empty() {
        flush(&mut out, &mut line);
    }
    let formatted = out.trim_end().to_string() + "\n";
    verify(src, &formatted)?;
    Ok(formatted)
}

/// Is a space needed between two adjacent tokens on the same line?
fn needs_space(
    a: &TokenKind,
    b: &TokenKind,
    a_word: Option<&str>,
    generic: usize,
    after_comment: bool,
) -> bool {
    use TokenKind::*;
    if after_comment {
        return true;
    }
    // Generic type brackets: `List<Map<String, Int>>`.
    if generic > 0 && matches!(b, Gt) || matches!(a, Lt) && generic > 0 {
        return false;
    }
    if matches!(b, Lt) && a_word.is_some_and(|w| w == "List" || w == "Map") {
        return false;
    }
    match (a, b) {
        (_, Comma | Semi | Colon | Dot | RParen | RBracket | Question) => false,
        (Dot | LParen | LBracket, _) => false,
        (Bang, _) => false,
        (Ident(w), LParen) if !NO_CALL_WORDS.contains(&w.as_str()) => false,
        // `expect step s output [...]` and `mock "t" returns [...]` are not indexing.
        (Ident(w), LBracket) if w == "output" || w == "returns" => true,
        (Ident(_) | RParen | RBracket | Str(_), LBracket) => false,
        (LBrace, RBrace) => false,
        _ => true,
    }
}

fn verify(original: &str, formatted: &str) -> Result<(), FormatError> {
    let key = |src: &str| {
        let (tokens, comments, diags) = tokenize_with_comments(0, src);
        let toks: Vec<(TokenKind, bool)> = tokens
            .into_iter()
            .map(|t| (t.kind, t.newline_before))
            .collect();
        let texts: Vec<String> = comments
            .iter()
            .map(|c| {
                src[c.span.start as usize..c.span.end as usize]
                    .trim_end()
                    .to_string()
            })
            .collect();
        (toks, texts, diags.len())
    };
    let (a, ca, _) = key(original);
    let (b, cb, db) = key(formatted);
    if db > 0 {
        return Err(FormatError::Unsafe("output does not lex".into()));
    }
    if a != b {
        let at = a
            .iter()
            .zip(&b)
            .position(|(x, y)| x != y)
            .unwrap_or(a.len().min(b.len()));
        return Err(FormatError::Unsafe(format!(
            "token stream differs at token {at}"
        )));
    }
    let norm = |v: Vec<String>| {
        v.into_iter()
            .map(|c| c.lines().map(str::trim_end).collect::<Vec<_>>().join("\n"))
            .collect::<Vec<_>>()
    };
    if norm(ca) != norm(cb) {
        return Err(FormatError::Unsafe("comments differ".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalises_spacing_and_indentation_and_keeps_comments() {
        let src = "// header\nworkflow W{\nversion   \"1.0.0\"\n\n\n  input{ items:List< Map<String,Int> >? , n:Int=-1 }\nstep a{ worker \"x\"   // trailing\n with {n:input.n*2+len(input.items),m: -3}\n  when !input.flag&&input.n>=0\n}\n}\n";
        let out = format_source(src).unwrap();
        assert_eq!(
            out,
            "// header\nworkflow W {\n    version \"1.0.0\"\n\n    input { items: List<Map<String, Int>>?, n: Int = -1 }\n    step a { worker \"x\" // trailing\n        with { n: input.n * 2 + len(input.items), m: -3 }\n        when !input.flag && input.n >= 0\n    }\n}\n"
        );
        assert_eq!(format_source(&out).unwrap(), out, "idempotent");
    }

    #[test]
    fn continuation_lines_are_indented() {
        let src = "workflow W {\nreturn {\nmsg: if a\nthen \"x\"\nelse \"y\"\n}\n}\n";
        let out = format_source(src).unwrap();
        assert!(
            out.contains("        msg: if a\n            then \"x\"\n            else \"y\"\n"),
            "{out}"
        );
    }

    #[test]
    fn generic_close_is_not_a_continuation() {
        let src = "workflow W {\noutput {\na: List<{ b: String }>\nc: String\n}\n}\n";
        let out = format_source(src).unwrap();
        assert!(
            out.contains("        a: List<{ b: String }>\n        c: String\n"),
            "{out}"
        );
    }

    #[test]
    fn refuses_lexically_invalid_input() {
        assert!(matches!(
            format_source("workflow W { x: \"open\n}"),
            Err(FormatError::Lex(_))
        ));
    }

    #[test]
    fn crlf_files_keep_their_line_endings() {
        let lf = "workflow W {\n    version \"1\"\n    return 1\n}\n";
        let crlf = lf.replace('\n', "\r\n");
        let out = format_source(&crlf).unwrap();
        assert_eq!(out, crlf);
        assert_eq!(format_source(lf).unwrap(), lf);
    }
}
