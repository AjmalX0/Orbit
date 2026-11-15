//! Syntax tree of `test` declarations (`orbit test`).

use super::{Expr, Ident};
use crate::source::Span;

/// `test "name" { workflow W ... }`: a declarative test of one workflow, run
/// by `orbit test`. Tests are type-checked with the rest of the program but
/// never appear in compiled output.
#[derive(Debug, Clone)]
pub struct TestDecl {
    pub name: String,
    pub name_span: Span,
    pub workflow: Option<Ident>,
    pub version: Option<(String, Span)>,
    pub input: Option<Expr>,
    pub mocks: Vec<MockDecl>,
    pub signals: Vec<TestSignal>,
    pub expectations: Vec<Expectation>,
    pub span: Span,
}

/// `mock "task" returns <expr>`, `mock "task" fails "CODE" times 2 then returns <expr>`, ...
#[derive(Debug, Clone)]
pub struct MockDecl {
    pub task: String,
    pub task_span: Span,
    /// Behaviours in order; each but the last carries `times N`.
    pub behaviours: Vec<MockBehaviour>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct MockBehaviour {
    pub outcome: MockOutcome,
    /// `times N`: how many calls this behaviour answers before the next one.
    pub times: Option<(i64, Span)>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum MockOutcome {
    /// `returns <expr>`; `input` is the task's input.
    Returns(Expr),
    /// `fails "CODE" ["message"] [retryable]`
    Fails {
        code: String,
        message: Option<String>,
        retryable: bool,
    },
    /// `hangs`: the worker never answers, so the attempt's timeout applies.
    Hangs,
}

/// `signal <name> { payload }` inside a test.
#[derive(Debug, Clone)]
pub struct TestSignal {
    pub name: Ident,
    pub payload: Option<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Expectation {
    pub kind: ExpectKind,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum ExpectKind {
    /// `expect status COMPLETED`
    Status(Ident),
    /// `expect output { ... }`: the listed fields must match.
    Output(Expr),
    /// `expect error "CODE"`
    Error(String, Span),
    /// `expect step <name> SUCCEEDED`, `... output <expr>`, `... attempts <n>`
    Step(Ident, StepExpectation),
    /// `expect compensated <step>`
    Compensated(Ident),
}

#[derive(Debug, Clone)]
pub enum StepExpectation {
    Status(Ident),
    Output(Expr),
    Attempts(i64, Span),
}
