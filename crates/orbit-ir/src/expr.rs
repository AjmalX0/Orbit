//! Expression IR and its deterministic evaluator.
//!
//! Expressions are pure: they read the workflow input, completed step
//! outputs and execution metadata, and never perform I/O or read the clock.
//! This keeps evaluation replayable from history.
//!
//! Member access is *null-propagating*: `a.b` evaluates to `null` when `a` is
//! `null` (for example a step skipped by `when`). The type checker marks such
//! expressions as optional.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Number, Value};
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnOp {
    Not,
    Neg,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObjectField {
    pub name: String,
    pub value: Expr,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Expr {
    Literal {
        value: Value,
    },
    /// `input.a.b`
    Input {
        path: Vec<String>,
    },
    /// `<step>.a.b` — the output of a completed (or skipped → `null`) step.
    Step {
        step: String,
        path: Vec<String>,
    },
    /// `execution.id`, `execution.workflow`, `execution.version`
    Execution {
        field: String,
    },
    Index {
        target: Box<Expr>,
        index: Box<Expr>,
    },
    Object {
        fields: Vec<ObjectField>,
    },
    List {
        items: Vec<Expr>,
    },
    Binary {
        operator: BinOp,
        left: Box<Expr>,
        right: Box<Expr>,
    },
    Unary {
        operator: UnOp,
        operand: Box<Expr>,
    },
    If {
        condition: Box<Expr>,
        then: Box<Expr>,
        otherwise: Box<Expr>,
    },
    Call {
        function: String,
        args: Vec<Expr>,
    },
}

impl Expr {
    pub fn literal(value: Value) -> Expr {
        Expr::Literal { value }
    }

    /// Every step this expression reads from.
    pub fn step_refs(&self, out: &mut Vec<String>) {
        match self {
            Expr::Step { step, .. } => {
                if !out.contains(step) {
                    out.push(step.clone())
                }
            }
            Expr::Index { target, index } => {
                target.step_refs(out);
                index.step_refs(out);
            }
            Expr::Object { fields } => fields.iter().for_each(|f| f.value.step_refs(out)),
            Expr::List { items } | Expr::Call { args: items, .. } => {
                items.iter().for_each(|e| e.step_refs(out))
            }
            Expr::Binary { left, right, .. } => {
                left.step_refs(out);
                right.step_refs(out);
            }
            Expr::Unary { operand, .. } => operand.step_refs(out),
            Expr::If {
                condition,
                then,
                otherwise,
            } => {
                condition.step_refs(out);
                then.step_refs(out);
                otherwise.step_refs(out);
            }
            Expr::Literal { .. } | Expr::Input { .. } | Expr::Execution { .. } => {}
        }
    }
}

/// Data an expression can read at runtime.
pub trait Scope {
    fn input(&self) -> &Value;
    /// `Some(Value::Null)` for a skipped step, `None` if the step has not completed.
    fn step_output(&self, step: &str) -> Option<&Value>;
    fn execution_field(&self, field: &str) -> Option<Value>;
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalError(pub String);

impl fmt::Display for EvalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for EvalError {}

fn err<T>(msg: impl Into<String>) -> Result<T, EvalError> {
    Err(EvalError(msg.into()))
}

fn walk(mut v: Value, path: &[String]) -> Value {
    for seg in path {
        v = match v {
            Value::Object(mut m) => m.remove(seg).unwrap_or(Value::Null),
            _ => return Value::Null,
        };
    }
    v
}

pub fn eval(expr: &Expr, scope: &dyn Scope) -> Result<Value, EvalError> {
    match expr {
        Expr::Literal { value } => Ok(value.clone()),
        Expr::Input { path } => Ok(walk(scope.input().clone(), path)),
        Expr::Step { step, path } => match scope.step_output(step) {
            Some(v) => Ok(walk(v.clone(), path)),
            None => err(format!("step `{step}` has not completed")),
        },
        Expr::Execution { field } => scope
            .execution_field(field)
            .ok_or_else(|| EvalError(format!("unknown execution field `{field}`"))),
        Expr::Index { target, index } => {
            let t = eval(target, scope)?;
            let i = eval(index, scope)?;
            Ok(match (t, i) {
                (Value::Null, _) => Value::Null,
                (Value::Array(items), Value::Number(n)) => {
                    let Some(mut idx) = n.as_i64() else {
                        return err("list index must be an integer");
                    };
                    if idx < 0 {
                        idx += items.len() as i64;
                    }
                    items
                        .into_iter()
                        .nth(idx.max(0) as usize)
                        .filter(|_| idx >= 0)
                        .unwrap_or(Value::Null)
                }
                (Value::Object(mut m), Value::String(k)) => m.remove(&k).unwrap_or(Value::Null),
                (t, i) => {
                    return err(format!(
                        "cannot index {} with {}",
                        crate::types::describe(&t),
                        crate::types::describe(&i)
                    ))
                }
            })
        }
        Expr::Object { fields } => {
            let mut m = Map::new();
            for f in fields {
                m.insert(f.name.clone(), eval(&f.value, scope)?);
            }
            Ok(Value::Object(m))
        }
        Expr::List { items } => Ok(Value::Array(
            items
                .iter()
                .map(|e| eval(e, scope))
                .collect::<Result<_, _>>()?,
        )),
        Expr::Unary { operator, operand } => {
            let v = eval(operand, scope)?;
            match (operator, v) {
                (UnOp::Not, Value::Bool(b)) => Ok(Value::Bool(!b)),
                (UnOp::Neg, Value::Number(n)) => match n.as_i64() {
                    Some(i) => i
                        .checked_neg()
                        .map(Value::from)
                        .ok_or_else(|| EvalError("integer overflow".into())),
                    None => Ok(float(-n.as_f64().unwrap_or(0.0))),
                },
                (op, v) => err(format!(
                    "cannot apply {op:?} to {}",
                    crate::types::describe(&v)
                )),
            }
        }
        Expr::Binary {
            operator,
            left,
            right,
        } => {
            // Short-circuit logical operators.
            if matches!(operator, BinOp::And | BinOp::Or) {
                let l = eval(left, scope)?;
                let Value::Bool(lb) = l else {
                    return err("logical operator expects Bool");
                };
                if (*operator == BinOp::And && !lb) || (*operator == BinOp::Or && lb) {
                    return Ok(Value::Bool(lb));
                }
                let r = eval(right, scope)?;
                return match r {
                    Value::Bool(rb) => Ok(Value::Bool(rb)),
                    _ => err("logical operator expects Bool"),
                };
            }
            let l = eval(left, scope)?;
            let r = eval(right, scope)?;
            binary(*operator, l, r)
        }
        Expr::If {
            condition,
            then,
            otherwise,
        } => match eval(condition, scope)? {
            Value::Bool(true) => eval(then, scope),
            Value::Bool(false) => eval(otherwise, scope),
            Value::Null => eval(otherwise, scope),
            v => err(format!(
                "`if` condition must be Bool, found {}",
                crate::types::describe(&v)
            )),
        },
        Expr::Call { function, args } => {
            let vals = args
                .iter()
                .map(|a| eval(a, scope))
                .collect::<Result<Vec<_>, _>>()?;
            call(function, vals)
        }
    }
}

fn float(f: f64) -> Value {
    Number::from_f64(f)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

fn num(v: &Value) -> Option<f64> {
    v.as_f64()
}

/// Structural equality where `1 == 1.0`.
pub fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => match (x.as_i64(), y.as_i64()) {
            (Some(i), Some(j)) => i == j,
            _ => x.as_f64() == y.as_f64(),
        },
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(a, b)| values_equal(a, b))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).is_some_and(|w| values_equal(v, w)))
        }
        _ => a == b,
    }
}

fn binary(op: BinOp, l: Value, r: Value) -> Result<Value, EvalError> {
    use BinOp::*;
    match op {
        Eq => return Ok(Value::Bool(values_equal(&l, &r))),
        Ne => return Ok(Value::Bool(!values_equal(&l, &r))),
        _ => {}
    }
    match (op, &l, &r) {
        (Add, Value::String(a), Value::String(b)) => Ok(Value::String(format!("{a}{b}"))),
        (Add, Value::Array(a), Value::Array(b)) => {
            Ok(Value::Array(a.iter().chain(b).cloned().collect()))
        }
        (Lt | Le | Gt | Ge, Value::String(a), Value::String(b)) => Ok(Value::Bool(match op {
            Lt => a < b,
            Le => a <= b,
            Gt => a > b,
            _ => a >= b,
        })),
        (_, Value::Number(a), Value::Number(b)) => {
            if let (Some(x), Some(y)) = (a.as_i64(), b.as_i64()) {
                let overflow = || EvalError("integer overflow".into());
                return Ok(match op {
                    Add => x.checked_add(y).ok_or_else(overflow)?.into(),
                    Sub => x.checked_sub(y).ok_or_else(overflow)?.into(),
                    Mul => x.checked_mul(y).ok_or_else(overflow)?.into(),
                    Div => {
                        if y == 0 {
                            return err("division by zero");
                        }
                        x.checked_div(y).ok_or_else(overflow)?.into()
                    }
                    Rem => {
                        if y == 0 {
                            return err("division by zero");
                        }
                        x.checked_rem(y).ok_or_else(overflow)?.into()
                    }
                    Lt => (x < y).into(),
                    Le => (x <= y).into(),
                    Gt => (x > y).into(),
                    Ge => (x >= y).into(),
                    _ => unreachable!(),
                });
            }
            let (x, y) = (num(&l).unwrap_or(0.0), num(&r).unwrap_or(0.0));
            Ok(match op {
                Add => float(x + y),
                Sub => float(x - y),
                Mul => float(x * y),
                Div => {
                    if y == 0.0 {
                        return err("division by zero");
                    }
                    float(x / y)
                }
                Rem => {
                    if y == 0.0 {
                        return err("division by zero");
                    }
                    float(x % y)
                }
                Lt => (x < y).into(),
                Le => (x <= y).into(),
                Gt => (x > y).into(),
                Ge => (x >= y).into(),
                _ => unreachable!(),
            })
        }
        (op, l, r) => err(format!(
            "operator {op:?} is not defined for {} and {}",
            crate::types::describe(l),
            crate::types::describe(r)
        )),
    }
}

/// Built-in pure functions. Signatures are checked by the compiler.
pub const BUILTINS: &[&str] = &[
    "len", "coalesce", "string", "int", "float", "contains", "lower", "upper", "trim", "join",
    "keys", "min", "max", "abs", "is_null",
];

pub fn call(function: &str, args: Vec<Value>) -> Result<Value, EvalError> {
    let arg = |i: usize| args.get(i).cloned().unwrap_or(Value::Null);
    match function {
        "len" => match arg(0) {
            Value::String(s) => Ok(Value::from(s.chars().count() as i64)),
            Value::Array(a) => Ok(Value::from(a.len() as i64)),
            Value::Object(o) => Ok(Value::from(o.len() as i64)),
            Value::Null => Ok(Value::from(0)),
            v => err(format!(
                "len() not defined for {}",
                crate::types::describe(&v)
            )),
        },
        "coalesce" => Ok(args
            .into_iter()
            .find(|v| !v.is_null())
            .unwrap_or(Value::Null)),
        "is_null" => Ok(Value::Bool(arg(0).is_null())),
        "string" => Ok(Value::String(match arg(0) {
            Value::String(s) => s,
            Value::Null => "null".into(),
            v => v.to_string(),
        })),
        "int" => match arg(0) {
            Value::Number(n) => Ok(n
                .as_i64()
                .map(Value::from)
                .unwrap_or_else(|| Value::from(n.as_f64().unwrap_or(0.0).trunc() as i64))),
            Value::String(s) => s
                .trim()
                .parse::<i64>()
                .map(Value::from)
                .map_err(|_| EvalError(format!("cannot convert {s:?} to Int"))),
            Value::Bool(b) => Ok(Value::from(b as i64)),
            v => err(format!(
                "int() not defined for {}",
                crate::types::describe(&v)
            )),
        },
        "float" => match arg(0) {
            Value::Number(n) => Ok(float(n.as_f64().unwrap_or(0.0))),
            Value::String(s) => s
                .trim()
                .parse::<f64>()
                .map(float)
                .map_err(|_| EvalError(format!("cannot convert {s:?} to Float"))),
            v => err(format!(
                "float() not defined for {}",
                crate::types::describe(&v)
            )),
        },
        "contains" => match (arg(0), arg(1)) {
            (Value::String(s), Value::String(sub)) => Ok(Value::Bool(s.contains(&sub))),
            (Value::Array(a), x) => Ok(Value::Bool(a.iter().any(|v| values_equal(v, &x)))),
            (Value::Object(o), Value::String(k)) => Ok(Value::Bool(o.contains_key(&k))),
            (Value::Null, _) => Ok(Value::Bool(false)),
            _ => err("contains() expects (String, String), (List<T>, T) or (Map, String)"),
        },
        "lower" | "upper" | "trim" => match arg(0) {
            Value::String(s) => Ok(Value::String(match function {
                "lower" => s.to_lowercase(),
                "upper" => s.to_uppercase(),
                _ => s.trim().to_string(),
            })),
            Value::Null => Ok(Value::Null),
            v => err(format!(
                "{function}() expects String, found {}",
                crate::types::describe(&v)
            )),
        },
        "join" => match (arg(0), arg(1)) {
            (Value::Array(a), Value::String(sep)) => Ok(Value::String(
                a.iter()
                    .map(|v| match v {
                        Value::String(s) => s.clone(),
                        v => v.to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join(&sep),
            )),
            _ => err("join() expects (List<String>, String)"),
        },
        "keys" => match arg(0) {
            Value::Object(o) => Ok(Value::Array(
                o.keys().map(|k| Value::String(k.clone())).collect(),
            )),
            _ => err("keys() expects a Map"),
        },
        "min" | "max" => {
            let mut best: Option<Value> = None;
            for v in args {
                if !v.is_number() {
                    return err(format!("{function}() expects numbers"));
                }
                let better = match &best {
                    None => true,
                    Some(b) => {
                        let (x, y) = (v.as_f64().unwrap(), b.as_f64().unwrap());
                        if function == "min" {
                            x < y
                        } else {
                            x > y
                        }
                    }
                };
                if better {
                    best = Some(v);
                }
            }
            Ok(best.unwrap_or(Value::Null))
        }
        "abs" => match arg(0) {
            Value::Number(n) => match n.as_i64() {
                Some(i) => i
                    .checked_abs()
                    .map(Value::from)
                    .ok_or_else(|| EvalError("integer overflow".into())),
                None => Ok(float(n.as_f64().unwrap_or(0.0).abs())),
            },
            v => err(format!(
                "abs() expects a number, found {}",
                crate::types::describe(&v)
            )),
        },
        other => err(format!("unknown function `{other}`")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct S(Value, Value);
    impl Scope for S {
        fn input(&self) -> &Value {
            &self.0
        }
        fn step_output(&self, step: &str) -> Option<&Value> {
            self.1.get(step)
        }
        fn execution_field(&self, f: &str) -> Option<Value> {
            (f == "id").then(|| json!("exec-1"))
        }
    }

    fn b(op: BinOp, l: Expr, r: Expr) -> Expr {
        Expr::Binary {
            operator: op,
            left: Box::new(l),
            right: Box::new(r),
        }
    }

    #[test]
    fn evaluates() {
        let s = S(
            json!({"a": 2, "name": "x"}),
            json!({"risk": {"score": 0.5}, "skipped": null}),
        );
        let e = b(
            BinOp::Lt,
            Expr::Step {
                step: "risk".into(),
                path: vec!["score".into()],
            },
            Expr::literal(json!(0.8)),
        );
        assert_eq!(eval(&e, &s).unwrap(), json!(true));
        let e = b(
            BinOp::Add,
            Expr::literal(json!("id-")),
            Expr::Input {
                path: vec!["name".into()],
            },
        );
        assert_eq!(eval(&e, &s).unwrap(), json!("id-x"));
        let e = Expr::Step {
            step: "skipped".into(),
            path: vec!["x".into(), "y".into()],
        };
        assert_eq!(eval(&e, &s).unwrap(), Value::Null);
        let e = b(BinOp::Div, Expr::literal(json!(7)), Expr::literal(json!(2)));
        assert_eq!(eval(&e, &s).unwrap(), json!(3));
        let e = b(BinOp::Div, Expr::literal(json!(7)), Expr::literal(json!(0)));
        assert!(eval(&e, &s).is_err());
        assert_eq!(
            eval(&Expr::Execution { field: "id".into() }, &s).unwrap(),
            json!("exec-1")
        );
        assert!(values_equal(&json!(1), &json!(1.0)));
        assert_eq!(
            call("coalesce", vec![Value::Null, json!(3)]).unwrap(),
            json!(3)
        );
        assert_eq!(
            call("join", vec![json!(["a", "b"]), json!(",")]).unwrap(),
            json!("a,b")
        );
    }
}
