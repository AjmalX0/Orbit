//! Parsing of `test` declarations.
//!
//! ```text
//! test "large orders wait for approval" {
//!     workflow OrderApproval
//!     input { order_id: "T1", amount: 2500.0 }
//!     mock "payments.charge" returns { transaction_id: "tx-" + input.order_id }
//!     mock "risk.analyze" fails "TEMPORARY" retryable times 2 then returns { score: 0.1 }
//!     signal review { approved: true }
//!     expect status COMPLETED
//! }
//! ```

use super::{PResult, Parser};
use crate::ast::*;
use crate::diagnostic::Diagnostic;
use crate::lexer::TokenKind;

pub(super) const TEST_MEMBERS: &[&str] =
    &["workflow", "version", "input", "mock", "signal", "expect"];
const EXPECT_KINDS: &[&str] = &["status", "output", "error", "step", "compensated"];
const MOCK_OUTCOMES: &[&str] = &["returns", "fails", "hangs"];

impl Parser {
    pub(super) fn test_decl(&mut self) -> PResult<TestDecl> {
        let start = self.bump().span;
        let (name, name_span) =
            self.string("a test name string, e.g. \"approves small orders\"")?;
        let mut t = TestDecl {
            name,
            name_span,
            workflow: None,
            version: None,
            input: None,
            mocks: vec![],
            signals: vec![],
            expectations: vec![],
            span: start,
        };
        let span = self.block("test", TEST_MEMBERS, |p| {
            let word = match p.kind() {
                TokenKind::Ident(w) => w.clone(),
                _ => return Ok(false),
            };
            let kw = p.span();
            let dup = |p: &mut Parser, set: bool| {
                if set {
                    p.diagnostics.push(Diagnostic::error(
                        "E0106",
                        format!("duplicate `{word}`"),
                        kw,
                    ));
                }
            };
            match word.as_str() {
                "workflow" => {
                    p.bump();
                    p.eat_colon();
                    dup(p, t.workflow.is_some());
                    t.workflow = Some(p.ident("a workflow name")?);
                }
                "version" => {
                    p.bump();
                    p.eat_colon();
                    dup(p, t.version.is_some());
                    t.version = Some(p.string("a version string")?);
                }
                "input" => {
                    p.bump();
                    p.eat_colon();
                    dup(p, t.input.is_some());
                    t.input = Some(p.expr());
                }
                "mock" => t.mocks.push(p.mock()?),
                "signal" => {
                    p.bump();
                    let name = p.ident("a signal name")?;
                    let payload = if p.at(&TokenKind::LBrace) && !p.tok().newline_before {
                        Some(p.expr())
                    } else {
                        None
                    };
                    let end = payload.as_ref().map(|e| e.span()).unwrap_or(name.span);
                    t.signals.push(TestSignal {
                        span: kw.to(end),
                        name,
                        payload,
                    });
                }
                "expect" => t.expectations.push(p.expectation()?),
                _ => return Ok(false),
            }
            Ok(true)
        });
        t.span = start.to(span);
        Ok(t)
    }

    fn mock(&mut self) -> PResult<MockDecl> {
        let start = self.bump().span;
        let (task, task_span) = self.string("a task name string, e.g. \"payments.charge\"")?;
        let mut behaviours = vec![];
        loop {
            let mut b = self.mock_behaviour()?;
            if self.at_word("times") && !self.tok().newline_before {
                let s = self.bump().span;
                match self.kind().clone() {
                    TokenKind::Int(n) => {
                        let end = self.bump().span;
                        b.times = Some((n, end));
                        b.span = b.span.to(end);
                    }
                    _ => {
                        self.error_here("a number of calls, e.g. `times 2`");
                        return Err(());
                    }
                }
                if !self.at_word("then") {
                    self.diagnostics.push(
                        Diagnostic::error(
                            "E0100",
                            format!("expected `then`, found {}", self.kind().describe()),
                            self.span(),
                        )
                        .with_label(s, "`times` must be followed by `then <behaviour>`")
                        .with_help(
                            "e.g. `fails \"TEMPORARY\" retryable times 2 then returns { ... }`",
                        ),
                    );
                    return Err(());
                }
                self.bump();
                behaviours.push(b);
                continue;
            }
            behaviours.push(b);
            break;
        }
        let end = behaviours.last().map(|b| b.span).unwrap_or(task_span);
        Ok(MockDecl {
            task,
            task_span,
            behaviours,
            span: start.to(end),
        })
    }

    fn mock_behaviour(&mut self) -> PResult<MockBehaviour> {
        let start = self.span();
        let word = match self.kind() {
            TokenKind::Ident(w) => w.clone(),
            _ => String::new(),
        };
        match word.as_str() {
            "returns" => {
                self.bump();
                let e = self.expr();
                Ok(MockBehaviour {
                    span: start.to(e.span()),
                    outcome: MockOutcome::Returns(e),
                    times: None,
                })
            }
            "fails" => {
                self.bump();
                let (code, mut end) =
                    self.string("an error code string, e.g. \"CARD_DECLINED\"")?;
                let mut message = None;
                if let TokenKind::Str(m) = self.kind().clone() {
                    if !self.tok().newline_before {
                        end = self.bump().span;
                        message = Some(m);
                    }
                }
                let mut retryable = false;
                if self.at_word("retryable") && !self.tok().newline_before {
                    end = self.bump().span;
                    retryable = true;
                }
                Ok(MockBehaviour {
                    outcome: MockOutcome::Fails {
                        code,
                        message,
                        retryable,
                    },
                    times: None,
                    span: start.to(end),
                })
            }
            "hangs" => {
                let end = self.bump().span;
                Ok(MockBehaviour {
                    outcome: MockOutcome::Hangs,
                    times: None,
                    span: start.to(end),
                })
            }
            _ => {
                let mut d = Diagnostic::error(
                    "E0100",
                    format!(
                        "expected `returns`, `fails` or `hangs`, found {}",
                        self.kind().describe()
                    ),
                    start,
                );
                if let Some(s) = crate::diagnostic::suggest(&word, MOCK_OUTCOMES.iter().copied()) {
                    d = d.with_help(format!("did you mean `{s}`?"));
                }
                self.diagnostics.push(d);
                Err(())
            }
        }
    }

    fn expectation(&mut self) -> PResult<Expectation> {
        let start = self.bump().span;
        let word = match self.kind() {
            TokenKind::Ident(w) => w.clone(),
            _ => String::new(),
        };
        let kind = match word.as_str() {
            "status" => {
                self.bump();
                ExpectKind::Status(self.ident("a workflow status, e.g. `COMPLETED`")?)
            }
            "output" => {
                self.bump();
                ExpectKind::Output(self.expr())
            }
            "error" => {
                self.bump();
                let (code, span) = self.string("an error code string, e.g. \"CARD_DECLINED\"")?;
                ExpectKind::Error(code, span)
            }
            "compensated" => {
                self.bump();
                ExpectKind::Compensated(self.ident("a step name")?)
            }
            "step" => {
                self.bump();
                let mut step = self.ident("a step name")?;
                // Handler steps are named `on_<kind>:<step>`; event steps
                // `on_event:<signal>:<step>`.
                let parts = if step.name == "on_event" { 2 } else { 1 };
                for _ in 0..parts {
                    if !(self.at(&TokenKind::Colon) && step.name.starts_with("on_")) {
                        break;
                    }
                    self.bump();
                    let rest = self.ident("a handler step name")?;
                    step = Ident {
                        name: format!("{}:{}", step.name, rest.name),
                        span: step.span.to(rest.span),
                    };
                }
                let check = if self.at_word("output") {
                    self.bump();
                    StepExpectation::Output(self.expr())
                } else if self.at_word("attempts") {
                    self.bump();
                    match self.kind().clone() {
                        TokenKind::Int(n) => StepExpectation::Attempts(n, self.bump().span),
                        _ => {
                            self.error_here("a number of attempts");
                            return Err(());
                        }
                    }
                } else {
                    StepExpectation::Status(
                        self.ident("a step status (e.g. `SUCCEEDED`), `output` or `attempts`")?,
                    )
                };
                ExpectKind::Step(step, check)
            }
            _ => {
                let mut d = Diagnostic::error(
                    "E0100",
                    format!(
                        "expected `status`, `output`, `error`, `step` or `compensated`, found {}",
                        self.kind().describe()
                    ),
                    self.span(),
                );
                if let Some(s) = crate::diagnostic::suggest(&word, EXPECT_KINDS.iter().copied()) {
                    d = d.with_help(format!("did you mean `{s}`?"));
                }
                self.diagnostics.push(d);
                return Err(());
            }
        };
        Ok(Expectation {
            kind,
            span: start.to(self.prev_span()),
        })
    }
}
