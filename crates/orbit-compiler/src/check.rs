//! Expression type checking and lowering to IR expressions.

use crate::types::{assignable, field_names, join};
use orbit_ir::{self as ir, FieldType, Type};
use orbit_syntax::ast::{self, BinOp, Expr, UnOp};
use orbit_syntax::diagnostic::{suggest, Diagnostic};
use orbit_syntax::Span;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};

/// What an expression is allowed to read.
pub struct Env<'a> {
    pub input: Option<&'a Type>,
    /// Step name -> type seen by references (optional if the step is conditional).
    pub steps: &'a BTreeMap<String, Type>,
    /// If set, only these steps may be referenced (with the reason shown otherwise).
    pub allowed: Option<(&'a HashSet<String>, &'a str)>,
    /// Context name used in messages, e.g. "default value".
    pub context: &'a str,
    /// Loop variables in scope (`for item in ...`), with their types.
    pub locals: &'a [(String, Type)],
}

pub fn execution_type() -> Type {
    Type::record(
        Some("Execution"),
        ["id", "workflow", "version"]
            .iter()
            .map(|n| FieldType {
                name: n.to_string(),
                ty: Type::String,
                default: None,
            })
            .collect(),
    )
}

pub struct Checker<'e, 'd> {
    pub env: &'e Env<'e>,
    pub diags: &'d mut Vec<Diagnostic>,
}

/// Evaluate a constant expression (used for field defaults).
pub fn constant(e: &Expr, ty: &Type, diags: &mut Vec<Diagnostic>) -> Option<Value> {
    let steps = BTreeMap::new();
    let env = Env {
        input: None,
        steps: &steps,
        allowed: None,
        context: "a default value",
        locals: &[],
    };
    let before = diags.len();
    let (_, lowered) = Checker { env: &env, diags }.check(e, Some(ty));
    if diags.len() != before {
        return None;
    }
    struct Empty(Value);
    impl ir::Scope for Empty {
        fn input(&self) -> &Value {
            &self.0
        }
        fn step_output(&self, _: &str) -> Option<&Value> {
            None
        }
        fn execution_field(&self, _: &str) -> Option<Value> {
            None
        }
    }
    match ir::eval(&lowered, &Empty(json!({}))) {
        Ok(v) => Some(v),
        Err(err) => {
            diags.push(Diagnostic::error(
                "E0205",
                format!("invalid default value: {err}"),
                e.span(),
            ));
            None
        }
    }
}

fn lit(v: Value) -> ir::Expr {
    ir::Expr::Literal { value: v }
}

impl Checker<'_, '_> {
    fn error(&mut self, d: Diagnostic) {
        self.diags.push(d);
    }

    /// Report a mismatch between an inferred and expected type.
    fn expect(&mut self, found: &Type, expected: &Type, span: Span) {
        if let Err(reason) = assignable(found, expected) {
            let optional =
                found.is_optional() && assignable(found.unwrap_optional(), expected).is_ok();
            let mut d = if optional {
                Diagnostic::error("E0405", format!("value may be `null` but `{expected}` is required"), span).with_help(
                    "provide a fallback with `coalesce(value, default)`, or declare the target as optional with `?`",
                )
            } else {
                Diagnostic::error(
                    "E0400",
                    format!("type mismatch: expected `{expected}`, found `{found}`"),
                    span,
                )
            };
            if !optional && !reason.starts_with("expected") {
                d = d.with_note(reason);
            }
            self.error(d);
        }
    }

    pub fn check(&mut self, e: &Expr, expected: Option<&Type>) -> (Type, ir::Expr) {
        // Expected-type directed checking for literals gives precise spans.
        if let Some(exp) = expected {
            match (e, exp.unwrap_optional()) {
                (Expr::Object(fields, span), Type::Record { fields: want, .. })
                    if !matches!(exp, Type::Json) =>
                {
                    return self.check_object_against(fields, *span, want, exp);
                }
                (Expr::Object(fields, _), Type::Map { value, .. }) => {
                    let value = (**value).clone();
                    let lowered = fields
                        .iter()
                        .map(|(k, v)| ir::ObjectField {
                            name: k.name.clone(),
                            value: self.check(v, Some(&value)).1,
                        })
                        .collect();
                    return (
                        exp.unwrap_optional().clone(),
                        ir::Expr::Object { fields: lowered },
                    );
                }
                (Expr::List(items, _), Type::List { item }) => {
                    let item = (**item).clone();
                    let lowered = items.iter().map(|i| self.check(i, Some(&item)).1).collect();
                    return (exp.clone(), ir::Expr::List { items: lowered });
                }
                (Expr::Str(s, span), Type::Uuid) => {
                    if !orbit_ir::types::is_uuid(s) {
                        self.error(Diagnostic::error(
                            "E0400",
                            format!("`{s}` is not a valid UUID"),
                            *span,
                        ));
                    }
                    return (Type::Uuid, lit(json!(s)));
                }
                (Expr::Str(s, span), Type::Timestamp) => {
                    if !orbit_ir::types::is_timestamp(s) {
                        self.error(
                            Diagnostic::error(
                                "E0400",
                                format!("`{s}` is not a valid RFC 3339 timestamp"),
                                *span,
                            )
                            .with_help("use the form `2026-01-31T09:00:00Z`"),
                        );
                    }
                    return (Type::Timestamp, lit(json!(s)));
                }
                _ => {}
            }
        }
        let (ty, lowered) = self.infer(e);
        if let Some(exp) = expected {
            self.expect(&ty, exp, e.span());
        }
        (ty, lowered)
    }

    fn check_object_against(
        &mut self,
        fields: &[(ast::Ident, Expr)],
        span: Span,
        want: &[FieldType],
        exp: &Type,
    ) -> (Type, ir::Expr) {
        let mut out = vec![];
        for (k, v) in fields {
            match want.iter().find(|f| f.name == k.name) {
                Some(f) => {
                    let (_, l) = self.check(v, Some(&f.ty));
                    out.push(ir::ObjectField {
                        name: k.name.clone(),
                        value: l,
                    });
                }
                None => {
                    let mut d = Diagnostic::error(
                        "E0404",
                        format!("unknown field `{}` for `{}`", k.name, exp.unwrap_optional()),
                        k.span,
                    );
                    if let Some(s) = suggest(&k.name, want.iter().map(|f| f.name.as_str())) {
                        d = d.with_help(format!("did you mean `{s}`?"));
                    } else {
                        d = d.with_note(format!(
                            "expected fields: {}",
                            want.iter()
                                .map(|f| f.name.as_str())
                                .collect::<Vec<_>>()
                                .join(", ")
                        ));
                    }
                    self.error(d);
                    let _ = self.infer(v);
                }
            }
        }
        let missing: Vec<&FieldType> = want
            .iter()
            .filter(|f| f.required() && !fields.iter().any(|(k, _)| k.name == f.name))
            .collect();
        if !missing.is_empty() {
            let list = missing
                .iter()
                .map(|f| format!("`{}: {}`", f.name, f.ty))
                .collect::<Vec<_>>()
                .join(", ");
            self.error(
                Diagnostic::error(
                    "E0403",
                    format!(
                        "missing required field{} {list}",
                        if missing.len() > 1 { "s" } else { "" }
                    ),
                    span,
                )
                .with_note(format!("required by `{}`", exp.unwrap_optional())),
            );
        }
        (
            exp.unwrap_optional().clone(),
            ir::Expr::Object { fields: out },
        )
    }

    fn infer(&mut self, e: &Expr) -> (Type, ir::Expr) {
        match e {
            Expr::Null(_) => (Type::Null, lit(Value::Null)),
            Expr::Bool(b, _) => (Type::Bool, lit(json!(b))),
            Expr::Int(i, _) => (Type::Int, lit(json!(i))),
            Expr::Float(f, _) => (Type::Float, lit(json!(f))),
            Expr::Str(s, _) => (Type::String, lit(json!(s))),
            Expr::Duration(ms, _) => (Type::Duration, lit(json!(ms))),
            Expr::Error(_) => (Type::Json, lit(Value::Null)),
            Expr::Path(segs, span) => self.path(segs, *span),
            Expr::Index(target, index, span) => {
                let (tt, tl) = self.infer(target);
                let (it, il) = self.infer(index);
                let opt = tt.is_optional();
                let ty = match tt.unwrap_optional() {
                    Type::Json => Type::Json,
                    Type::List { item } => {
                        self.expect(&it, &Type::Int, index.span());
                        item.as_ref().clone().optional()
                    }
                    Type::Map { value, .. } => {
                        self.expect(&it, &Type::String, index.span());
                        value.as_ref().clone().optional()
                    }
                    Type::Record { fields, .. } => {
                        // `x["field"]` on records with a literal key, or `(expr).field`.
                        match index.as_ref() {
                            Expr::Str(k, ks) => match fields.iter().find(|f| &f.name == k) {
                                Some(f) => f.ty.clone(),
                                None => {
                                    self.unknown_field(k, *ks, &tt);
                                    Type::Json
                                }
                            },
                            _ => {
                                self.error(Diagnostic::error(
                                    "E0406",
                                    "records can only be indexed with a literal field name",
                                    index.span(),
                                ));
                                Type::Json
                            }
                        }
                    }
                    other => {
                        self.error(Diagnostic::error(
                            "E0406",
                            format!("type `{other}` cannot be indexed"),
                            *span,
                        ));
                        Type::Json
                    }
                };
                let ty = if opt { ty.optional() } else { ty };
                (
                    ty,
                    ir::Expr::Index {
                        target: Box::new(tl),
                        index: Box::new(il),
                    },
                )
            }
            Expr::Object(fields, _) => {
                let mut ftypes = vec![];
                let mut lowered = vec![];
                for (k, v) in fields {
                    let (t, l) = self.infer(v);
                    let t = if t == Type::Null { Type::Json } else { t };
                    ftypes.push(FieldType {
                        name: k.name.clone(),
                        ty: t,
                        default: None,
                    });
                    lowered.push(ir::ObjectField {
                        name: k.name.clone(),
                        value: l,
                    });
                }
                (
                    Type::record(None, ftypes),
                    ir::Expr::Object { fields: lowered },
                )
            }
            Expr::List(items, _) => {
                let mut ty: Option<Type> = None;
                let mut lowered = vec![];
                for item in items {
                    let (t, l) = self.infer(item);
                    lowered.push(l);
                    ty = Some(match ty {
                        None => t,
                        Some(prev) => {
                            match join(&prev, &t) {
                                Some(j) => j,
                                None => {
                                    self.error(
                                    Diagnostic::error("E0400", format!("list items have incompatible types `{prev}` and `{t}`"), item.span())
                                        .with_help("use `List<JSON>` if mixed values are intended"),
                                );
                                    Type::Json
                                }
                            }
                        }
                    });
                }
                (
                    Type::List {
                        item: Box::new(ty.unwrap_or(Type::Json)),
                    },
                    ir::Expr::List { items: lowered },
                )
            }
            Expr::Unary(op, operand, span) => {
                let (t, l) = self.infer(operand);
                let ty = match op {
                    UnOp::Not => {
                        self.expect(&t, &Type::Bool, operand.span());
                        Type::Bool
                    }
                    UnOp::Neg => {
                        if !t.is_numeric() && t != Type::Json {
                            self.error(Diagnostic::error(
                                "E0406",
                                format!("cannot negate `{t}`"),
                                *span,
                            ));
                        }
                        if t == Type::Json {
                            Type::Float
                        } else {
                            t
                        }
                    }
                };
                let op = match op {
                    UnOp::Not => ir::UnOp::Not,
                    UnOp::Neg => ir::UnOp::Neg,
                };
                (
                    ty,
                    ir::Expr::Unary {
                        operator: op,
                        operand: Box::new(l),
                    },
                )
            }
            Expr::Binary(op, l, r, span) => self.binary(*op, l, r, *span),
            Expr::If(c, a, b, _) => {
                let (ct, cl) = self.infer(c);
                if assignable(&ct, &Type::Bool.optional()).is_err() {
                    self.error(Diagnostic::error(
                        "E0408",
                        format!("`if` condition must be `Bool`, found `{ct}`"),
                        c.span(),
                    ));
                }
                let (at, al) = self.infer(a);
                let (bt, bl) = self.infer(b);
                let ty = match join(&at, &bt) {
                    Some(t) => t,
                    None => {
                        self.error(
                            Diagnostic::error(
                                "E0400",
                                format!("`if` branches have incompatible types `{at}` and `{bt}`"),
                                b.span(),
                            )
                            .with_label(a.span(), format!("this is `{at}`")),
                        );
                        Type::Json
                    }
                };
                (
                    ty,
                    ir::Expr::If {
                        condition: Box::new(cl),
                        then: Box::new(al),
                        otherwise: Box::new(bl),
                    },
                )
            }
            Expr::Call(name, args, span) => self.call(name, args, *span),
        }
    }

    fn unknown_field(&mut self, name: &str, span: Span, on: &Type) {
        let mut d = Diagnostic::error(
            "E0402",
            format!("no field `{name}` on type `{}`", on.unwrap_optional()),
            span,
        );
        let names = field_names(on);
        if let Some(s) = suggest(name, names.iter().copied()) {
            d = d.with_help(format!("did you mean `{s}`?"));
        } else if !names.is_empty() {
            d = d.with_note(format!("available fields: {}", names.join(", ")));
        }
        self.error(d);
    }

    /// Walk `.field` segments through a type.
    fn walk_fields(&mut self, mut ty: Type, segs: &[ast::Ident]) -> Type {
        for seg in segs {
            let opt = ty.is_optional() && ty != Type::Json;
            let next = match ty.unwrap_optional() {
                Type::Json => Type::Json,
                Type::Record { fields, .. } => match fields.iter().find(|f| f.name == seg.name) {
                    Some(f) => f.ty.clone(),
                    None => {
                        self.unknown_field(&seg.name, seg.span, &ty);
                        return Type::Json;
                    }
                },
                Type::Map { value, .. } => value.as_ref().clone().optional(),
                other => {
                    self.error(Diagnostic::error(
                        "E0402",
                        format!("type `{other}` has no field `{}`", seg.name),
                        seg.span,
                    ));
                    return Type::Json;
                }
            };
            ty = if opt { next.optional() } else { next };
        }
        ty
    }

    fn path(&mut self, segs: &[ast::Ident], span: Span) -> (Type, ir::Expr) {
        let root = &segs[0];
        let rest: Vec<String> = segs[1..].iter().map(|s| s.name.clone()).collect();
        if let Some((_, t)) = self.env.locals.iter().find(|(n, _)| *n == root.name) {
            let ty = self.walk_fields(t.clone(), &segs[1..]);
            return (
                ty,
                ir::Expr::Local {
                    name: root.name.clone(),
                    path: rest,
                },
            );
        }
        match root.name.as_str() {
            "input" => match self.env.input {
                Some(t) => {
                    let ty = self.walk_fields(t.clone(), &segs[1..]);
                    (ty, ir::Expr::Input { path: rest })
                }
                None => {
                    self.error(Diagnostic::error(
                        "E0401",
                        format!("`input` cannot be used in {}", self.env.context),
                        root.span,
                    ));
                    (Type::Json, lit(Value::Null))
                }
            },
            "execution" => {
                if self.env.input.is_none() {
                    self.error(Diagnostic::error(
                        "E0401",
                        format!("`execution` cannot be used in {}", self.env.context),
                        root.span,
                    ));
                    return (Type::Json, lit(Value::Null));
                }
                if segs.len() != 2 {
                    self.error(
                        Diagnostic::error("E0402", "expected `execution.<field>`", span).with_note(
                            "available: execution.id, execution.workflow, execution.version",
                        ),
                    );
                    return (Type::Json, lit(Value::Null));
                }
                let ty = self.walk_fields(execution_type(), &segs[1..]);
                (
                    ty,
                    ir::Expr::Execution {
                        field: rest[0].clone(),
                    },
                )
            }
            name => match self.env.steps.get(name) {
                Some(t) => {
                    if let Some((allowed, why)) = self.env.allowed {
                        if !allowed.contains(name) {
                            self.error(
                                Diagnostic::error(
                                    "E0308",
                                    format!("step `{name}` cannot be referenced here"),
                                    root.span,
                                )
                                .with_note(why.to_string()),
                            );
                        }
                    }
                    let ty = self.walk_fields(t.clone(), &segs[1..]);
                    (
                        ty,
                        ir::Expr::Step {
                            step: name.to_string(),
                            path: rest,
                        },
                    )
                }
                None => {
                    let mut d = Diagnostic::error(
                        "E0401",
                        format!("unknown reference `{name}`"),
                        root.span,
                    );
                    let mut candidates: Vec<&str> =
                        self.env.steps.keys().map(String::as_str).collect();
                    if self.env.input.is_some() {
                        candidates.extend(["input", "execution"]);
                    }
                    if let Some(s) = suggest(name, candidates.iter().copied()) {
                        d = d.with_help(format!("did you mean `{s}`?"));
                    } else if self.env.input.is_some() {
                        d = d.with_note("expressions can read `input.<field>`, `<step>.<field>` and `execution.<field>`");
                    } else {
                        d = d.with_note(format!("{} must be a constant", self.env.context));
                    }
                    self.error(d);
                    (Type::Json, lit(Value::Null))
                }
            },
        }
    }

    fn binary(&mut self, op: BinOp, l: &Expr, r: &Expr, span: Span) -> (Type, ir::Expr) {
        let (lt, ll) = self.infer(l);
        let (rt, rl) = self.infer(r);
        let iop = match op {
            BinOp::Add => ir::BinOp::Add,
            BinOp::Sub => ir::BinOp::Sub,
            BinOp::Mul => ir::BinOp::Mul,
            BinOp::Div => ir::BinOp::Div,
            BinOp::Rem => ir::BinOp::Rem,
            BinOp::Eq => ir::BinOp::Eq,
            BinOp::Ne => ir::BinOp::Ne,
            BinOp::Lt => ir::BinOp::Lt,
            BinOp::Le => ir::BinOp::Le,
            BinOp::Gt => ir::BinOp::Gt,
            BinOp::Ge => ir::BinOp::Ge,
            BinOp::And => ir::BinOp::And,
            BinOp::Or => ir::BinOp::Or,
        };
        let lowered = ir::Expr::Binary {
            operator: iop,
            left: Box::new(ll),
            right: Box::new(rl),
        };
        let dynamic = lt == Type::Json || rt == Type::Json;
        let bad = |this: &mut Self, help: Option<&str>| {
            let mut d = Diagnostic::error(
                "E0406",
                format!(
                    "operator `{}` cannot be applied to `{lt}` and `{rt}`",
                    op.symbol()
                ),
                span,
            )
            .with_label(l.span(), format!("`{lt}`"))
            .with_label(r.span(), format!("`{rt}`"));
            if let Some(h) = help {
                d = d.with_help(h.to_string());
            }
            this.error(d);
        };
        let optional_help = "one operand may be `null`; use `coalesce(value, default)` first";
        let ty = match op {
            BinOp::And | BinOp::Or => {
                if !dynamic && (lt != Type::Bool || rt != Type::Bool) {
                    bad(
                        self,
                        (lt.is_optional() || rt.is_optional()).then_some(optional_help),
                    );
                }
                Type::Bool
            }
            BinOp::Eq | BinOp::Ne => {
                if !dynamic && join(&lt, &rt).is_none() {
                    bad(self, Some("values of different types are never equal"));
                }
                Type::Bool
            }
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                let ok = dynamic
                    || (lt.is_numeric() && rt.is_numeric())
                    || (lt == Type::String && rt == Type::String);
                if !ok {
                    bad(
                        self,
                        (lt.is_optional() || rt.is_optional()).then_some(optional_help),
                    );
                }
                Type::Bool
            }
            BinOp::Add if lt == Type::String && rt == Type::String => Type::String,
            BinOp::Add if matches!((&lt, &rt), (Type::List { .. }, Type::List { .. })) => {
                match join(&lt, &rt) {
                    Some(t) => t,
                    None => {
                        bad(self, None);
                        Type::Json
                    }
                }
            }
            BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => {
                if dynamic {
                    Type::Json
                } else if lt.is_numeric() && rt.is_numeric() {
                    if lt == Type::Int && rt == Type::Int {
                        Type::Int
                    } else {
                        Type::Float
                    }
                } else {
                    let help = if lt.is_optional() || rt.is_optional() {
                        Some(optional_help)
                    } else if op == BinOp::Add && (lt == Type::String || rt == Type::String) {
                        Some("convert the other operand with `string(value)`")
                    } else {
                        None
                    };
                    bad(self, help);
                    Type::Json
                }
            }
        };
        (ty, lowered)
    }

    fn call(&mut self, name: &ast::Ident, args: &[Expr], span: Span) -> (Type, ir::Expr) {
        let typed: Vec<(Type, ir::Expr)> = args.iter().map(|a| self.infer(a)).collect();
        let lowered = ir::Expr::Call {
            function: name.name.clone(),
            args: typed.iter().map(|t| t.1.clone()).collect(),
        };
        let tys: Vec<&Type> = typed.iter().map(|t| &t.0).collect();
        let arity = |this: &mut Self, n: std::ops::RangeInclusive<usize>, sig: &str| -> bool {
            if !n.contains(&args.len()) {
                this.error(
                    Diagnostic::error(
                        "E0407",
                        format!(
                            "`{}` takes {} argument(s), found {}",
                            name.name,
                            if n.start() == n.end() {
                                n.start().to_string()
                            } else if *n.end() == usize::MAX {
                                format!("at least {}", n.start())
                            } else {
                                format!("{}-{}", n.start(), n.end())
                            },
                            args.len()
                        ),
                        span,
                    )
                    .with_note(format!("signature: {sig}")),
                );
                false
            } else {
                true
            }
        };
        let arg_err = |this: &mut Self, i: usize, want: &str| {
            this.error(Diagnostic::error(
                "E0400",
                format!(
                    "argument {} of `{}` must be {want}, found `{}`",
                    i + 1,
                    name.name,
                    tys[i]
                ),
                args[i].span(),
            ));
        };
        let is = |t: &Type, f: fn(&Type) -> bool| *t == Type::Json || f(t.unwrap_optional());
        let ty = match name.name.as_str() {
            "len" => {
                if arity(self, 1..=1, "len(String | List<T> | Map<String, T>) -> Int")
                    && !is(tys[0], |t| {
                        matches!(t, Type::String | Type::List { .. } | Type::Map { .. })
                    })
                {
                    arg_err(self, 0, "a String, List or Map");
                }
                Type::Int
            }
            "coalesce" => {
                if arity(self, 1..=usize::MAX, "coalesce(T?, ..., T) -> T") {
                    let mut acc = tys[0].clone();
                    for (i, t) in tys.iter().enumerate().skip(1) {
                        acc = match join(&acc, t) {
                            Some(j) => j,
                            None => {
                                arg_err(self, i, &format!("compatible with `{acc}`"));
                                Type::Json
                            }
                        };
                    }
                    // The result is non-null if any argument is non-null.
                    if tys.iter().any(|t| !t.is_optional()) {
                        acc.unwrap_optional().clone()
                    } else {
                        acc
                    }
                } else {
                    Type::Json
                }
            }
            "is_null" => {
                arity(self, 1..=1, "is_null(T?) -> Bool");
                Type::Bool
            }
            "string" => {
                arity(self, 1..=1, "string(T) -> String");
                Type::String
            }
            "int" | "float" => {
                if arity(
                    self,
                    1..=1,
                    &format!(
                        "{}(Int | Float | String | Bool) -> {}",
                        name.name,
                        if name.name == "int" { "Int" } else { "Float" }
                    ),
                ) && !is(tys[0], |t| {
                    matches!(t, Type::Int | Type::Float | Type::String | Type::Bool)
                }) {
                    arg_err(self, 0, "a number, String or Bool");
                }
                if name.name == "int" {
                    Type::Int
                } else {
                    Type::Float
                }
            }
            "contains" => {
                arity(self, 2..=2, "contains(String, String) | contains(List<T>, T) | contains(Map<String, T>, String) -> Bool");
                Type::Bool
            }
            "lower" | "upper" | "trim" => {
                if arity(self, 1..=1, &format!("{}(String) -> String", name.name)) {
                    if !is(tys[0], |t| *t == Type::String) {
                        arg_err(self, 0, "a String");
                    }
                    if tys[0].is_optional() {
                        Type::String.optional()
                    } else {
                        Type::String
                    }
                } else {
                    Type::String
                }
            }
            "join" => {
                if arity(self, 2..=2, "join(List<String>, String) -> String") {
                    if !is(tys[0], |t| matches!(t, Type::List { .. })) {
                        arg_err(self, 0, "a List");
                    }
                    if !is(tys[1], |t| *t == Type::String) {
                        arg_err(self, 1, "a String");
                    }
                }
                Type::String
            }
            "keys" => {
                if arity(self, 1..=1, "keys(Map<String, T>) -> List<String>")
                    && !is(tys[0], |t| {
                        matches!(t, Type::Map { .. } | Type::Record { .. })
                    })
                {
                    arg_err(self, 0, "a Map");
                }
                Type::List {
                    item: Box::new(Type::String),
                }
            }
            "min" | "max" => {
                if arity(
                    self,
                    1..=usize::MAX,
                    &format!("{}(number, ...) -> number", name.name),
                ) {
                    for (i, t) in tys.iter().enumerate() {
                        if !t.is_numeric() && **t != Type::Json {
                            arg_err(self, i, "Int or Float");
                        }
                    }
                }
                if tys.iter().all(|t| **t == Type::Int) {
                    Type::Int
                } else {
                    Type::Float
                }
            }
            "abs" => {
                if arity(self, 1..=1, "abs(number) -> number")
                    && !tys[0].is_numeric()
                    && *tys[0] != Type::Json
                {
                    arg_err(self, 0, "Int or Float");
                }
                tys.first().map(|t| (*t).clone()).unwrap_or(Type::Float)
            }
            other => {
                let mut d =
                    Diagnostic::error("E0407", format!("unknown function `{other}`"), name.span);
                if let Some(s) = suggest(other, ir::expr::BUILTINS.iter().copied()) {
                    d = d.with_help(format!("did you mean `{s}`?"));
                } else {
                    d = d.with_note(format!(
                        "built-in functions: {}",
                        ir::expr::BUILTINS.join(", ")
                    ));
                }
                self.error(d);
                Type::Json
            }
        };
        (ty, lowered)
    }
}
