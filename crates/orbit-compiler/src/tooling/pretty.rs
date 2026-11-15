//! Print IR expressions back as ORBIT source text, for diagrams, documentation
//! and diffs. The output parses to the same expression (durations appear as
//! their millisecond values, since the IR does not keep the unit).

use orbit_ir::{BinOp, Expr, UnOp};
use serde_json::Value;

fn op(o: BinOp) -> (&'static str, u8) {
    match o {
        BinOp::Or => ("||", 1),
        BinOp::And => ("&&", 2),
        BinOp::Eq => ("==", 3),
        BinOp::Ne => ("!=", 3),
        BinOp::Lt => ("<", 4),
        BinOp::Le => ("<=", 4),
        BinOp::Gt => (">", 4),
        BinOp::Ge => (">=", 4),
        BinOp::Add => ("+", 5),
        BinOp::Sub => ("-", 5),
        BinOp::Mul => ("*", 6),
        BinOp::Div => ("/", 6),
        BinOp::Rem => ("%", 6),
    }
}

/// Precedence of an expression when it appears as an operand.
fn prec(e: &Expr) -> u8 {
    match e {
        Expr::Binary { operator, .. } => op(*operator).1,
        // `if` and lambdas extend as far right as possible.
        Expr::If { .. } | Expr::Lambda { .. } => 0,
        Expr::Unary { .. } => 7,
        _ => 8,
    }
}

/// ORBIT source text for an expression.
pub fn expr(e: &Expr) -> String {
    let mut out = String::new();
    write(e, &mut out);
    out
}

fn path(root: &str, segments: &[String], out: &mut String) {
    out.push_str(root);
    for s in segments {
        out.push('.');
        out.push_str(s);
    }
}

fn operand(e: &Expr, min: u8, out: &mut String) {
    if prec(e) < min {
        out.push('(');
        write(e, out);
        out.push(')');
    } else {
        write(e, out);
    }
}

/// A JSON literal as ORBIT source.
pub fn literal(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => serde_json::to_string(s).unwrap_or_default(),
        Value::Array(a) => format!("[{}]", a.iter().map(literal).collect::<Vec<_>>().join(", ")),
        Value::Object(m) if m.is_empty() => "{}".into(),
        Value::Object(m) => format!(
            "{{ {} }}",
            m.iter()
                .map(|(k, v)| format!("{k}: {}", literal(v)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn write(e: &Expr, out: &mut String) {
    match e {
        Expr::Literal { value } => out.push_str(&literal(value)),
        Expr::Input { path: p } => path("input", p, out),
        Expr::Step { step, path: p } => path(step, p, out),
        Expr::Execution { field } => {
            out.push_str("execution.");
            out.push_str(field);
        }
        Expr::Local { name, path: p } => path(name, p, out),
        Expr::Index { target, index } => {
            operand(target, 8, out);
            out.push('[');
            write(index, out);
            out.push(']');
        }
        Expr::Object { fields } if fields.is_empty() => out.push_str("{}"),
        Expr::Object { fields } => {
            out.push_str("{ ");
            for (i, f) in fields.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                out.push_str(&f.name);
                out.push_str(": ");
                write(&f.value, out);
            }
            out.push_str(" }");
        }
        Expr::List { items } => {
            out.push('[');
            for (i, x) in items.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write(x, out);
            }
            out.push(']');
        }
        Expr::Binary {
            operator,
            left,
            right,
        } => {
            let (sym, p) = op(*operator);
            operand(left, p, out);
            out.push(' ');
            out.push_str(sym);
            out.push(' ');
            operand(right, p + 1, out);
        }
        Expr::Unary {
            operator,
            operand: x,
        } => {
            out.push(match operator {
                UnOp::Not => '!',
                UnOp::Neg => '-',
            });
            operand(x, 7, out);
        }
        Expr::If {
            condition,
            then,
            otherwise,
        } => {
            out.push_str("if ");
            write(condition, out);
            out.push_str(" then ");
            write(then, out);
            out.push_str(" else ");
            write(otherwise, out);
        }
        Expr::Call { function, args } => {
            out.push_str(function);
            out.push('(');
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                write(a, out);
            }
            out.push(')');
        }
        Expr::Lambda { param, body } => {
            out.push_str(param);
            out.push_str(" => ");
            write(body, out);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile_str;

    fn condition(src_when: &str) -> String {
        let src = format!(
            "workflow W {{ version \"1.0.0\" input {{ a: Int, b: Int, s: String, xs: List<Int> }}\n\
             step one {{ compute 1 }}\n\
             step two {{ compute 2 when {src_when} }} }}"
        );
        let c = compile_str("w.orbit", &src);
        assert!(!c.has_errors(), "{}", c.render_diagnostics(false));
        expr(c.workflows[0].steps[1].condition.as_ref().unwrap())
    }

    #[test]
    fn round_trips_precedence() {
        for src in [
            "input.a >= 10 && input.b < 3 || input.s == \"x\"",
            "(input.a + input.b) * 2 > 10",
            "input.a - (input.b - 1) > 0",
            "!(input.a > 1) || one > 0",
            "any(input.xs, x => x > input.a)",
            "-input.a < 0",
            "(if input.a > 0 then 1 else 2) == 1",
            "input.xs[0] == 1",
        ] {
            assert_eq!(condition(src), src);
        }
    }
}
