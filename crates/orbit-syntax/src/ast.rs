//! Abstract syntax tree for ORBIT source files.

use crate::source::Span;

#[derive(Debug, Clone, PartialEq)]
pub struct Ident {
    pub name: String,
    pub span: Span,
}

#[derive(Debug, Clone, Default)]
pub struct Program {
    pub declarations: Vec<Declaration>,
}

#[derive(Debug, Clone)]
pub enum Declaration {
    Workflow(Workflow),
    Schema(Schema),
    Task(TaskDecl),
    Import(Import),
}

#[derive(Debug, Clone)]
pub struct Import {
    pub path: String,
    pub span: Span,
}

/// `schema Name { field: Type ... }`
#[derive(Debug, Clone)]
pub struct Schema {
    pub name: Ident,
    pub fields: Vec<Field>,
    pub span: Span,
}

/// `task "name" { version "1.0.0" input T output T }` — a worker contract.
#[derive(Debug, Clone)]
pub struct TaskDecl {
    pub name: String,
    pub name_span: Span,
    pub version: Option<(String, Span)>,
    pub input: Option<TypeExpr>,
    pub output: Option<TypeExpr>,
    pub description: Option<String>,
    /// `compensated_by "<task>"`: steps calling this task must compensate with it.
    pub compensated_by: Option<(String, Span)>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Field {
    pub name: Ident,
    pub ty: TypeExpr,
    pub default: Option<Expr>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum TypeExpr {
    Named(Ident),
    List(Box<TypeExpr>, Span),
    Map(Box<TypeExpr>, Box<TypeExpr>, Span),
    Optional(Box<TypeExpr>, Span),
    Record(Vec<Field>, Span),
}

impl TypeExpr {
    pub fn span(&self) -> Span {
        match self {
            TypeExpr::Named(i) => i.span,
            TypeExpr::List(_, s)
            | TypeExpr::Map(_, _, s)
            | TypeExpr::Optional(_, s)
            | TypeExpr::Record(_, s) => *s,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Workflow {
    pub name: Ident,
    pub version: Option<(String, Span)>,
    pub description: Option<String>,
    pub timeout: Option<(u64, Span)>,
    pub input: Vec<Field>,
    pub output: Option<(Vec<Field>, Span)>,
    pub steps: Vec<Step>,
    pub signals: Vec<SignalDecl>,
    pub queries: Vec<QueryDecl>,
    pub ret: Option<Expr>,
    pub span: Span,
}

/// `signal <name> { <fields> }`: a typed message the workflow can receive.
#[derive(Debug, Clone)]
pub struct SignalDecl {
    pub name: Ident,
    pub fields: Vec<Field>,
    pub span: Span,
}

/// `query <name> = <expr>`: a read-only view of the execution state.
#[derive(Debug, Clone)]
pub struct QueryDecl {
    pub name: Ident,
    pub expr: Expr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum StepAction {
    /// `worker "task.name"`
    Worker(String, Span),
    /// `sleep 10s` — a durable timer executed by the runtime itself.
    Sleep(u64, Span),
    /// `workflow "Name"` — run another workflow as a child execution.
    Workflow(String, Span),
    /// `await <signal>` — wait for the next signal of this name.
    Await(Ident),
    /// `compute <expr>` — evaluate a pure expression in the runtime.
    Compute(Expr, Span),
}

/// `for item in <list expression>` — fan a step out over a list.
#[derive(Debug, Clone)]
pub struct ForEach {
    pub variable: Ident,
    pub items: Expr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Step {
    pub name: Ident,
    pub action: Option<StepAction>,
    pub body: StepBody,
    pub compensation: Option<Compensation>,
    pub span: Span,
}

/// Members shared by steps and compensation blocks.
#[derive(Debug, Clone, Default)]
pub struct StepBody {
    pub language: Option<Ident>,
    pub after: Vec<Ident>,
    pub with: Option<Expr>,
    pub timeout: Option<(u64, Span)>,
    pub queue_timeout: Option<(u64, Span)>,
    pub retry: Option<Retry>,
    pub idempotency_key: Option<Expr>,
    pub condition: Option<Expr>,
    pub for_each: Option<ForEach>,
    /// `max_parallel 4` — concurrency limit for a `for` step.
    pub max_parallel: Option<(i64, Span)>,
}

#[derive(Debug, Clone)]
pub struct Compensation {
    pub action: Option<StepAction>,
    pub body: StepBody,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Retry {
    pub members: Vec<(Ident, Expr)>,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

impl BinOp {
    pub fn symbol(self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Rem => "%",
            BinOp::Eq => "==",
            BinOp::Ne => "!=",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
            BinOp::And => "&&",
            BinOp::Or => "||",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Not,
    Neg,
}

#[derive(Debug, Clone)]
pub enum Expr {
    Null(Span),
    Bool(bool, Span),
    Int(i64, Span),
    Float(f64, Span),
    Str(String, Span),
    Duration(u64, Span),
    /// `a.b.c` — the first segment is the root (`input`, a step, `execution`).
    Path(Vec<Ident>, Span),
    Index(Box<Expr>, Box<Expr>, Span),
    Call(Ident, Vec<Expr>, Span),
    Object(Vec<(Ident, Expr)>, Span),
    List(Vec<Expr>, Span),
    Binary(BinOp, Box<Expr>, Box<Expr>, Span),
    Unary(UnOp, Box<Expr>, Span),
    /// `if c then a else b`
    If(Box<Expr>, Box<Expr>, Box<Expr>, Span),
    /// `x => body`, only valid as an argument of `map`, `filter`, `any`,
    /// `all` and `find`.
    Lambda(Ident, Box<Expr>, Span),
    /// Placeholder produced after a parse error.
    Error(Span),
}

impl Expr {
    pub fn span(&self) -> Span {
        match self {
            Expr::Null(s)
            | Expr::Bool(_, s)
            | Expr::Int(_, s)
            | Expr::Float(_, s)
            | Expr::Str(_, s)
            | Expr::Duration(_, s)
            | Expr::Path(_, s)
            | Expr::Index(_, _, s)
            | Expr::Call(_, _, s)
            | Expr::Object(_, s)
            | Expr::List(_, s)
            | Expr::Binary(_, _, _, s)
            | Expr::Unary(_, _, s)
            | Expr::If(_, _, _, s)
            | Expr::Lambda(_, _, s)
            | Expr::Error(s) => *s,
        }
    }

    /// Visit every `Path` in this expression.
    pub fn walk_paths<'a>(&'a self, f: &mut dyn FnMut(&'a [Ident], Span)) {
        match self {
            Expr::Path(p, s) => f(p, *s),
            Expr::Index(a, b, _) | Expr::Binary(_, a, b, _) => {
                a.walk_paths(f);
                b.walk_paths(f);
            }
            Expr::Call(_, args, _) | Expr::List(args, _) => {
                args.iter().for_each(|a| a.walk_paths(f))
            }
            Expr::Object(fields, _) => fields.iter().for_each(|(_, e)| e.walk_paths(f)),
            Expr::Unary(_, e, _) | Expr::Lambda(_, e, _) => e.walk_paths(f),
            Expr::If(c, a, b, _) => {
                c.walk_paths(f);
                a.walk_paths(f);
                b.walk_paths(f);
            }
            _ => {}
        }
    }
}
