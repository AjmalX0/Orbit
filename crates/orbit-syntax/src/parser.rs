//! Recursive-descent parser producing an [`ast::Program`].
//!
//! The parser recovers from errors at member boundaries so that one run can
//! report many independent mistakes.

use crate::ast::*;
use crate::diagnostic::{suggest, Diagnostic};
use crate::lexer::{tokenize, Token, TokenKind};
use crate::source::{FileId, Span};

pub struct ParseResult {
    pub program: Program,
    pub diagnostics: Vec<Diagnostic>,
}

/// Lex and parse one source file.
pub fn parse(file: FileId, src: &str) -> ParseResult {
    let (tokens, mut diagnostics) = tokenize(file, src);
    let mut p = Parser {
        tokens,
        pos: 0,
        diagnostics: vec![],
    };
    let program = p.program();
    diagnostics.append(&mut p.diagnostics);
    ParseResult {
        program,
        diagnostics,
    }
}

const TOP_LEVEL: &[&str] = &["workflow", "schema", "task", "import"];
const WORKFLOW_MEMBERS: &[&str] = &[
    "version",
    "description",
    "timeout",
    "input",
    "output",
    "step",
    "return",
    "on",
];
const STEP_MEMBERS: &[&str] = &[
    "worker",
    "sleep",
    "using",
    "after",
    "with",
    "timeout",
    "queue_timeout",
    "retry",
    "idempotency_key",
    "when",
    "compensate",
];
const TASK_MEMBERS: &[&str] = &["version", "input", "output", "description"];

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
    diagnostics: Vec<Diagnostic>,
}

type PResult<T> = Result<T, ()>;

impl Parser {
    // ---------------------------------------------------------------- helpers

    fn tok(&self) -> &Token {
        &self.tokens[self.pos]
    }

    fn kind(&self) -> &TokenKind {
        &self.tokens[self.pos].kind
    }

    fn span(&self) -> Span {
        self.tokens[self.pos].span
    }

    fn prev_span(&self) -> Span {
        self.tokens[self.pos.saturating_sub(1)].span
    }

    fn bump(&mut self) -> Token {
        let t = self.tokens[self.pos].clone();
        if self.pos < self.tokens.len() - 1 {
            self.pos += 1;
        }
        t
    }

    fn at(&self, k: &TokenKind) -> bool {
        self.kind() == k
    }

    fn at_word(&self, w: &str) -> bool {
        matches!(self.kind(), TokenKind::Ident(s) if s == w)
    }

    fn eat(&mut self, k: &TokenKind) -> bool {
        if self.at(k) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn error_here(&mut self, expected: &str) {
        let found = self.kind().describe();
        let span = self.span();
        self.diagnostics.push(Diagnostic::error(
            "E0100",
            format!("expected {expected}, found {found}"),
            span,
        ));
    }

    fn expect(&mut self, k: TokenKind) -> PResult<Span> {
        if self.at(&k) {
            Ok(self.bump().span)
        } else {
            self.error_here(&format!("`{}`", k.symbol()));
            Err(())
        }
    }

    fn ident(&mut self, what: &str) -> PResult<Ident> {
        match self.kind().clone() {
            TokenKind::Ident(name) => {
                let span = self.bump().span;
                Ok(Ident { name, span })
            }
            _ => {
                self.error_here(what);
                Err(())
            }
        }
    }

    fn string(&mut self, what: &str) -> PResult<(String, Span)> {
        match self.kind().clone() {
            TokenKind::Str(s) => Ok((s, self.bump().span)),
            _ => {
                self.error_here(what);
                Err(())
            }
        }
    }

    fn duration(&mut self) -> PResult<(u64, Span)> {
        match self.kind().clone() {
            TokenKind::Duration(ms) => Ok((ms, self.bump().span)),
            TokenKind::Int(_) | TokenKind::Float(_) => {
                let span = self.bump().span;
                self.diagnostics.push(
                    Diagnostic::error("E0101", "duration is missing a unit", span)
                        .with_help("add a unit such as `ms`, `s`, `m`, `h` or `d`, e.g. `30s`"),
                );
                Err(())
            }
            _ => {
                self.error_here("a duration (e.g. `30s`)");
                Err(())
            }
        }
    }

    fn eat_separators(&mut self) {
        while matches!(self.kind(), TokenKind::Comma | TokenKind::Semi) {
            self.bump();
        }
    }

    /// Optional `:` after a member keyword (`idempotency_key: x` == `idempotency_key x`).
    fn eat_colon(&mut self) {
        self.eat(&TokenKind::Colon);
    }

    /// Skip tokens until something that plausibly starts a new member.
    fn recover(&mut self, members: &[&str]) {
        let mut depth = 0i32;
        loop {
            match self.kind() {
                TokenKind::Eof => return,
                TokenKind::LBrace => depth += 1,
                TokenKind::RBrace => {
                    if depth == 0 {
                        return;
                    }
                    depth -= 1;
                }
                TokenKind::Ident(w)
                    if depth == 0 && self.tok().newline_before && members.contains(&w.as_str()) =>
                {
                    return
                }
                _ => {}
            }
            self.bump();
        }
    }

    fn unknown_member(&mut self, context: &str, members: &[&str]) {
        let span = self.span();
        let found = self.kind().describe();
        let mut d = Diagnostic::error("E0102", format!("unexpected {found} in {context}"), span);
        if let TokenKind::Ident(w) = self.kind() {
            if let Some(s) = suggest(w, members.iter().copied()) {
                d = d.with_help(format!("did you mean `{s}`?"));
            } else {
                d = d.with_note(format!("valid members here: {}", members.join(", ")));
            }
        }
        self.diagnostics.push(d);
        self.bump();
        self.recover(members);
    }

    /// Parse `{ ... }` calling `member` for each element until `}`.
    fn block(
        &mut self,
        context: &str,
        members: &[&str],
        mut member: impl FnMut(&mut Self) -> PResult<bool>,
    ) -> Span {
        let open = self.span();
        if self.expect(TokenKind::LBrace).is_err() {
            self.recover(members);
            return open;
        }
        loop {
            self.eat_separators();
            if self.at(&TokenKind::RBrace) {
                return open.to(self.bump().span);
            }
            if self.at(&TokenKind::Eof) {
                self.diagnostics.push(
                    Diagnostic::error("E0103", format!("unclosed `{{` in {context}"), open)
                        .with_help("add a matching `}`"),
                );
                return open;
            }
            match member(self) {
                Ok(true) => {}
                Ok(false) => self.unknown_member(context, members),
                Err(()) => self.recover(members),
            }
        }
    }

    // ---------------------------------------------------------------- program

    fn program(&mut self) -> Program {
        let mut program = Program::default();
        loop {
            self.eat_separators();
            if self.at(&TokenKind::Eof) {
                return program;
            }
            let decl = match self.kind() {
                TokenKind::Ident(w) if w == "workflow" => {
                    self.workflow().map(Declaration::Workflow)
                }
                TokenKind::Ident(w) if w == "schema" => self.schema().map(Declaration::Schema),
                TokenKind::Ident(w) if w == "task" => self.task().map(Declaration::Task),
                TokenKind::Ident(w) if w == "import" => {
                    let start = self.bump().span;
                    self.string("an import path string").map(|(path, s)| {
                        Declaration::Import(Import {
                            path,
                            span: start.to(s),
                        })
                    })
                }
                _ => {
                    let span = self.span();
                    let mut d = Diagnostic::error(
                        "E0104",
                        format!("expected a declaration, found {}", self.kind().describe()),
                        span,
                    )
                    .with_note(
                        "top-level declarations are `workflow`, `schema`, `task` and `import`",
                    );
                    if let TokenKind::Ident(w) = self.kind() {
                        if let Some(s) = suggest(w, TOP_LEVEL.iter().copied()) {
                            d = d.with_help(format!("did you mean `{s}`?"));
                        }
                    }
                    self.diagnostics.push(d);
                    self.bump();
                    self.recover(TOP_LEVEL);
                    if self.at(&TokenKind::RBrace) {
                        self.bump();
                    }
                    continue;
                }
            };
            match decl {
                Ok(d) => program.declarations.push(d),
                Err(()) => {
                    self.recover(TOP_LEVEL);
                    if self.at(&TokenKind::RBrace) {
                        self.bump();
                    }
                }
            }
        }
    }

    fn schema(&mut self) -> PResult<Schema> {
        let start = self.bump().span;
        let name = self.ident("a schema name")?;
        let (fields, span) = self.record_fields("schema")?;
        Ok(Schema {
            name,
            fields,
            span: start.to(span),
        })
    }

    fn task(&mut self) -> PResult<TaskDecl> {
        let start = self.bump().span;
        let (name, name_span) = self.string("a task name string, e.g. \"payments.charge\"")?;
        let mut t = TaskDecl {
            name,
            name_span,
            version: None,
            input: None,
            output: None,
            description: None,
            span: start,
        };
        let span = self.block("task contract", TASK_MEMBERS, |p| {
            let word = match p.kind() {
                TokenKind::Ident(w) => w.clone(),
                _ => return Ok(false),
            };
            match word.as_str() {
                "version" => {
                    p.bump();
                    p.eat_colon();
                    t.version = Some(p.string("a version string")?);
                }
                "description" => {
                    p.bump();
                    p.eat_colon();
                    t.description = Some(p.string("a description string")?.0);
                }
                "input" => {
                    p.bump();
                    p.eat_colon();
                    t.input = Some(p.type_expr()?);
                }
                "output" => {
                    p.bump();
                    p.eat_colon();
                    t.output = Some(p.type_expr()?);
                }
                _ => return Ok(false),
            }
            Ok(true)
        });
        t.span = start.to(span);
        Ok(t)
    }

    fn record_fields(&mut self, context: &str) -> PResult<(Vec<Field>, Span)> {
        let mut fields = vec![];
        let span = self.block(context, &[], |p| {
            if !matches!(p.kind(), TokenKind::Ident(_)) {
                p.error_here("a field name");
                return Err(());
            }
            fields.push(p.field()?);
            Ok(true)
        });
        Ok((fields, span))
    }

    fn field(&mut self) -> PResult<Field> {
        let name = self.ident("a field name")?;
        self.expect(TokenKind::Colon)?;
        let ty = self.type_expr()?;
        let default = if self.eat(&TokenKind::Eq) {
            Some(self.expr())
        } else {
            None
        };
        let end = default
            .as_ref()
            .map(|d| d.span())
            .unwrap_or_else(|| ty.span());
        Ok(Field {
            span: name.span.to(end),
            name,
            ty,
            default,
        })
    }

    fn type_expr(&mut self) -> PResult<TypeExpr> {
        let mut ty = if self.at(&TokenKind::LBrace) {
            let (fields, span) = self.record_fields("record type")?;
            TypeExpr::Record(fields, span)
        } else {
            let name = self.ident("a type")?;
            match name.name.as_str() {
                "List" if self.at(&TokenKind::Lt) => {
                    self.bump();
                    let inner = self.type_expr()?;
                    let end = self.expect(TokenKind::Gt)?;
                    TypeExpr::List(Box::new(inner), name.span.to(end))
                }
                "Map" if self.at(&TokenKind::Lt) => {
                    self.bump();
                    let k = self.type_expr()?;
                    self.expect(TokenKind::Comma)?;
                    let v = self.type_expr()?;
                    let end = self.expect(TokenKind::Gt)?;
                    TypeExpr::Map(Box::new(k), Box::new(v), name.span.to(end))
                }
                _ => TypeExpr::Named(name),
            }
        };
        while self.at(&TokenKind::Question) {
            let end = self.bump().span;
            let span = ty.span().to(end);
            ty = TypeExpr::Optional(Box::new(ty), span);
        }
        Ok(ty)
    }

    // --------------------------------------------------------------- workflow

    fn workflow(&mut self) -> PResult<Workflow> {
        let start = self.bump().span;
        let name = self.ident("a workflow name")?;
        let mut wf = Workflow {
            name,
            version: None,
            description: None,
            timeout: None,
            input: vec![],
            output: None,
            steps: vec![],
            ret: None,
            span: start,
        };
        let span = self.block("workflow", WORKFLOW_MEMBERS, |p| {
            let word = match p.kind() {
                TokenKind::Ident(w) => w.clone(),
                _ => return Ok(false),
            };
            match word.as_str() {
                "version" => {
                    p.bump();
                    p.eat_colon();
                    wf.version = Some(p.string("a version string")?);
                }
                "description" => {
                    p.bump();
                    p.eat_colon();
                    wf.description = Some(p.string("a description string")?.0);
                }
                "timeout" => {
                    p.bump();
                    p.eat_colon();
                    wf.timeout = Some(p.duration()?);
                }
                "input" => {
                    p.bump();
                    wf.input.extend(p.record_fields("workflow input")?.0);
                }
                "output" => {
                    let s = p.bump().span;
                    let (fields, span) = p.record_fields("workflow output")?;
                    wf.output = Some((fields, s.to(span)));
                }
                "step" => wf.steps.push(p.step()?),
                "return" => {
                    p.bump();
                    wf.ret = Some(p.expr());
                }
                "on" => {
                    let s = p.bump().span;
                    let what = p.ident("`event` or `failure`")?;
                    if matches!(p.kind(), TokenKind::Str(_)) {
                        p.bump();
                    }
                    let mut span = p.span();
                    if p.at(&TokenKind::LBrace) {
                        let mut depth = 0;
                        loop {
                            match p.kind() {
                                TokenKind::LBrace => depth += 1,
                                TokenKind::RBrace => depth -= 1,
                                TokenKind::Eof => break,
                                _ => {}
                            }
                            span = p.bump().span;
                            if depth == 0 {
                                break;
                            }
                        }
                    }
                    p.diagnostics.push(
                        Diagnostic::error(
                            "E0105",
                            format!(
                                "`on {}` handlers are reserved but not supported in ORBIT v0.1",
                                what.name
                            ),
                            s.to(span),
                        )
                        .with_help(
                            "use a `compensate { ... }` block on the step that needs undoing",
                        ),
                    );
                }
                _ => return Ok(false),
            }
            Ok(true)
        });
        wf.span = start.to(span);
        Ok(wf)
    }

    fn step(&mut self) -> PResult<Step> {
        let start = self.bump().span;
        let name = self.ident("a step name")?;
        let mut action = None;
        let mut body = StepBody::default();
        let mut compensation = None;
        let span = self.block("step", STEP_MEMBERS, |p| {
            if p.at_word("compensate") {
                let s = p.bump().span;
                let mut c_action = None;
                let mut c_body = StepBody::default();
                let span = p.block("compensate block", STEP_MEMBERS, |p| {
                    p.step_member(&mut c_action, &mut c_body)
                });
                compensation = Some(Compensation {
                    action: c_action,
                    body: c_body,
                    span: s.to(span),
                });
                return Ok(true);
            }
            p.step_member(&mut action, &mut body)
        });
        Ok(Step {
            name,
            action,
            body,
            compensation,
            span: start.to(span),
        })
    }

    fn step_member(
        &mut self,
        action: &mut Option<StepAction>,
        body: &mut StepBody,
    ) -> PResult<bool> {
        let word = match self.kind() {
            TokenKind::Ident(w) => w.clone(),
            _ => return Ok(false),
        };
        let kw_span = self.span();
        let dup = |p: &mut Self, set: bool| {
            if set {
                p.diagnostics.push(Diagnostic::error(
                    "E0106",
                    format!("duplicate `{word}`"),
                    kw_span,
                ));
            }
        };
        match word.as_str() {
            "worker" | "sleep" => {
                self.bump();
                self.eat_colon();
                let new = if word == "worker" {
                    let (w, s) = self.string("a worker task name string")?;
                    StepAction::Worker(w, s)
                } else {
                    let (d, s) = self.duration()?;
                    StepAction::Sleep(d, s)
                };
                if action.is_some() {
                    self.diagnostics.push(
                        Diagnostic::error(
                            "E0106",
                            "a step can only have one `worker` or `sleep`",
                            kw_span,
                        )
                        .with_help("split the work into separate steps"),
                    );
                }
                *action = Some(new);
            }
            "using" => {
                self.bump();
                self.eat_colon();
                dup(self, body.language.is_some());
                body.language = Some(self.ident("a language name")?);
            }
            "after" => {
                self.bump();
                self.eat_colon();
                loop {
                    body.after.push(self.ident("a step name")?);
                    if !self.eat(&TokenKind::Comma) {
                        break;
                    }
                }
            }
            "with" => {
                self.bump();
                self.eat_colon();
                dup(self, body.with.is_some());
                body.with = Some(self.expr());
            }
            "timeout" => {
                self.bump();
                self.eat_colon();
                dup(self, body.timeout.is_some());
                body.timeout = Some(self.duration()?);
            }
            "queue_timeout" => {
                self.bump();
                self.eat_colon();
                dup(self, body.queue_timeout.is_some());
                body.queue_timeout = Some(self.duration()?);
            }
            "idempotency_key" => {
                self.bump();
                self.eat_colon();
                dup(self, body.idempotency_key.is_some());
                body.idempotency_key = Some(self.expr());
            }
            "when" => {
                self.bump();
                self.eat_colon();
                dup(self, body.condition.is_some());
                body.condition = Some(self.expr());
            }
            "retry" => {
                let s = self.bump().span;
                dup(self, body.retry.is_some());
                let mut members = vec![];
                let span = self.block("retry policy", &[], |p| {
                    let key = p.ident("a retry setting")?;
                    p.expect(TokenKind::Colon)?;
                    members.push((key, p.expr()));
                    Ok(true)
                });
                body.retry = Some(Retry {
                    members,
                    span: s.to(span),
                });
            }
            _ => return Ok(false),
        }
        Ok(true)
    }

    // ------------------------------------------------------------ expressions

    pub fn expr(&mut self) -> Expr {
        self.binary(1)
            .unwrap_or_else(|_| Expr::Error(self.prev_span()))
    }

    fn binop(&self) -> Option<(BinOp, u8)> {
        if self.tok().newline_before {
            return None;
        }
        Some(match self.kind() {
            TokenKind::OrOr => (BinOp::Or, 1),
            TokenKind::AndAnd => (BinOp::And, 2),
            TokenKind::EqEq => (BinOp::Eq, 3),
            TokenKind::NotEq => (BinOp::Ne, 3),
            TokenKind::Lt => (BinOp::Lt, 4),
            TokenKind::Le => (BinOp::Le, 4),
            TokenKind::Gt => (BinOp::Gt, 4),
            TokenKind::Ge => (BinOp::Ge, 4),
            TokenKind::Plus => (BinOp::Add, 5),
            TokenKind::Minus => (BinOp::Sub, 5),
            TokenKind::Star => (BinOp::Mul, 6),
            TokenKind::Slash => (BinOp::Div, 6),
            TokenKind::Percent => (BinOp::Rem, 6),
            _ => return None,
        })
    }

    fn binary(&mut self, min_prec: u8) -> PResult<Expr> {
        let mut lhs = self.unary()?;
        while let Some((op, prec)) = self.binop() {
            if prec < min_prec {
                break;
            }
            self.bump();
            let rhs = self.binary(prec + 1)?;
            let span = lhs.span().to(rhs.span());
            lhs = Expr::Binary(op, Box::new(lhs), Box::new(rhs), span);
        }
        Ok(lhs)
    }

    fn unary(&mut self) -> PResult<Expr> {
        let op = match self.kind() {
            TokenKind::Bang => UnOp::Not,
            TokenKind::Minus => UnOp::Neg,
            _ => return self.postfix(),
        };
        let start = self.bump().span;
        let e = self.unary()?;
        // Fold negative numeric literals.
        if op == UnOp::Neg {
            match e {
                Expr::Int(v, s) => return Ok(Expr::Int(-v, start.to(s))),
                Expr::Float(v, s) => return Ok(Expr::Float(-v, start.to(s))),
                _ => {}
            }
        }
        let span = start.to(e.span());
        Ok(Expr::Unary(op, Box::new(e), span))
    }

    fn postfix(&mut self) -> PResult<Expr> {
        let mut e = self.primary()?;
        loop {
            if self.tok().newline_before {
                break;
            }
            match self.kind() {
                TokenKind::LBracket => {
                    self.bump();
                    let idx = self.expr();
                    let end = self.expect(TokenKind::RBracket)?;
                    let span = e.span().to(end);
                    e = Expr::Index(Box::new(e), Box::new(idx), span);
                }
                TokenKind::Dot => {
                    // `.field` after an index or call: desugar to a path-like index.
                    self.bump();
                    let field = self.ident("a field name")?;
                    let span = e.span().to(field.span);
                    match e {
                        Expr::Path(mut segs, s) => {
                            segs.push(field);
                            e = Expr::Path(segs, s.to(span));
                        }
                        other => {
                            let key = Expr::Str(field.name, field.span);
                            e = Expr::Index(Box::new(other), Box::new(key), span);
                        }
                    }
                }
                _ => break,
            }
        }
        Ok(e)
    }

    fn primary(&mut self) -> PResult<Expr> {
        let t = self.tok().clone();
        let span = t.span;
        match t.kind {
            TokenKind::Null => {
                self.bump();
                Ok(Expr::Null(span))
            }
            TokenKind::True => {
                self.bump();
                Ok(Expr::Bool(true, span))
            }
            TokenKind::False => {
                self.bump();
                Ok(Expr::Bool(false, span))
            }
            TokenKind::Int(v) => {
                self.bump();
                Ok(Expr::Int(v, span))
            }
            TokenKind::Float(v) => {
                self.bump();
                Ok(Expr::Float(v, span))
            }
            TokenKind::Duration(v) => {
                self.bump();
                Ok(Expr::Duration(v, span))
            }
            TokenKind::Str(s) => {
                self.bump();
                Ok(Expr::Str(s, span))
            }
            TokenKind::LParen => {
                self.bump();
                let e = self.expr();
                self.expect(TokenKind::RParen)?;
                Ok(e)
            }
            TokenKind::LBracket => {
                self.bump();
                let mut items = vec![];
                loop {
                    self.eat_separators();
                    if self.at(&TokenKind::RBracket) {
                        break;
                    }
                    if self.at(&TokenKind::Eof) {
                        self.error_here("`]`");
                        return Err(());
                    }
                    items.push(self.expr());
                    if !self.eat(&TokenKind::Comma)
                        && !self.at(&TokenKind::RBracket)
                        && !self.tok().newline_before
                    {
                        self.error_here("`,` or `]`");
                        return Err(());
                    }
                }
                let end = self.bump().span;
                Ok(Expr::List(items, span.to(end)))
            }
            TokenKind::LBrace => {
                self.bump();
                let mut fields: Vec<(Ident, Expr)> = vec![];
                loop {
                    self.eat_separators();
                    if self.at(&TokenKind::RBrace) {
                        break;
                    }
                    let key = match self.kind().clone() {
                        TokenKind::Ident(name) => Ident {
                            name,
                            span: self.bump().span,
                        },
                        TokenKind::Str(name) => Ident {
                            name,
                            span: self.bump().span,
                        },
                        _ => {
                            self.error_here("a field name or `}`");
                            return Err(());
                        }
                    };
                    if let Some((prev, _)) = fields.iter().find(|(k, _)| k.name == key.name) {
                        self.diagnostics.push(
                            Diagnostic::error(
                                "E0107",
                                format!("duplicate field `{}` in object", key.name),
                                key.span,
                            )
                            .with_label(prev.span, "first defined here"),
                        );
                    }
                    // Shorthand `{ order_id }` means `{ order_id: order_id }`... but
                    // in ORBIT bare names resolve to steps, so we require `:`.
                    self.expect(TokenKind::Colon)?;
                    let value = self.expr();
                    fields.push((key, value));
                }
                let end = self.bump().span;
                Ok(Expr::Object(fields, span.to(end)))
            }
            TokenKind::Ident(name) if name == "if" => {
                self.bump();
                let c = self.expr();
                if !self.at_word("then") {
                    self.error_here("`then`");
                    return Err(());
                }
                self.bump();
                let a = self.expr();
                if !self.at_word("else") {
                    self.error_here("`else`");
                    return Err(());
                }
                self.bump();
                let b = self.expr();
                let s = span.to(b.span());
                Ok(Expr::If(Box::new(c), Box::new(a), Box::new(b), s))
            }
            TokenKind::Ident(name) => {
                self.bump();
                let id = Ident { name, span };
                if self.at(&TokenKind::LParen) && !self.tok().newline_before {
                    self.bump();
                    let mut args = vec![];
                    loop {
                        if self.at(&TokenKind::RParen) {
                            break;
                        }
                        args.push(self.expr());
                        if !self.eat(&TokenKind::Comma) {
                            break;
                        }
                    }
                    let end = self.expect(TokenKind::RParen)?;
                    return Ok(Expr::Call(id, args, span.to(end)));
                }
                Ok(Expr::Path(vec![id], span))
            }
            _ => {
                self.error_here("an expression");
                Err(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(src: &str) -> Program {
        let r = parse(0, src);
        assert!(r.diagnostics.is_empty(), "{:#?}", r.diagnostics);
        r.program
    }

    fn errs(src: &str) -> Vec<&'static str> {
        parse(0, src).diagnostics.iter().map(|d| d.code).collect()
    }

    #[test]
    fn parses_rfc_example() {
        let p = ok(include_str!("../../../examples/order_processing.orbit"));
        let wf = p
            .declarations
            .iter()
            .find_map(|d| {
                if let Declaration::Workflow(w) = d {
                    Some(w)
                } else {
                    None
                }
            })
            .unwrap();
        assert_eq!(wf.name.name, "OrderProcessing");
        assert!(wf.steps.len() >= 5);
        assert!(wf.ret.is_some());
    }

    #[test]
    fn precedence() {
        let p = ok("workflow W { return 1 + 2 * 3 == 7 && !false }");
        let Declaration::Workflow(w) = &p.declarations[0] else {
            panic!()
        };
        let Some(Expr::Binary(BinOp::And, lhs, _, _)) = &w.ret else {
            panic!("{:?}", w.ret)
        };
        let Expr::Binary(BinOp::Eq, add, _, _) = lhs.as_ref() else {
            panic!()
        };
        let Expr::Binary(BinOp::Add, _, mul, _) = add.as_ref() else {
            panic!()
        };
        assert!(matches!(mul.as_ref(), Expr::Binary(BinOp::Mul, _, _, _)));
    }

    #[test]
    fn left_associative() {
        let p = ok("workflow W { return 10 - 3 - 2 }");
        let Declaration::Workflow(w) = &p.declarations[0] else {
            panic!()
        };
        let Some(Expr::Binary(BinOp::Sub, lhs, rhs, _)) = &w.ret else {
            panic!()
        };
        assert!(matches!(lhs.as_ref(), Expr::Binary(BinOp::Sub, _, _, _)));
        assert!(matches!(rhs.as_ref(), Expr::Int(2, _)));
    }

    #[test]
    fn newline_ends_expression() {
        let p = ok("workflow W { return { a: 1\n b: -2 } }");
        let Declaration::Workflow(w) = &p.declarations[0] else {
            panic!()
        };
        let Some(Expr::Object(f, _)) = &w.ret else {
            panic!()
        };
        assert_eq!(f.len(), 2);
    }

    #[test]
    fn recovers_and_suggests() {
        let r = parse(
            0,
            "workflow W {\n step a {\n wrker \"x\"\n timeout 5\n }\n}",
        );
        let codes: Vec<_> = r.diagnostics.iter().map(|d| d.code).collect();
        assert_eq!(codes, vec!["E0102", "E0101"]);
        assert_eq!(
            r.diagnostics[0].help.as_deref(),
            Some("did you mean `worker`?")
        );
    }

    #[test]
    fn unclosed_block() {
        assert_eq!(errs("workflow W {\n step a {"), vec!["E0103", "E0103"]);
    }

    #[test]
    fn types() {
        ok("schema S { a: List<Map<String, Int>>? b: { c: UUID } = {c: \"x\"} }");
    }
}
