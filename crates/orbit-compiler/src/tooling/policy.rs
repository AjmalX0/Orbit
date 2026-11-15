//! Organisation policies checked at compile time (`orbit check --policy`).
//!
//! A policy is a TOML file with a list of `[[rule]]` tables. Each rule has a
//! `type`, an `id` (default: the type) used in reports and waivers, a
//! `severity` (`error`, the default, or `warning`) and an optional scope
//! (`workflows` / `exclude_workflows` glob patterns, defaulting to the
//! `[policy]` table's). Violations are reported as compiler diagnostics with
//! codes in the `P01xx` range; problems with waivers are `P0001` and `P0002`.
//!
//! Rules work on the compiled IR. Task patterns are globs where `*` matches
//! any characters, dots included (`payments.*`, `*.charge`, `*`); they match
//! worker task names, including built-in tasks such as `orbit.http.request`,
//! and never child workflows. See `docs/src/tooling/policy.md` for the
//! reference, including exactly what `require_approval` proves.

use super::source::SourceIndex;
use super::{glob_any, parse_duration, Scope};
use crate::Compilation;
use orbit_ir::{Action, BinOp, Expr, StepIr, Type, UnOp, WorkflowIr, WORKFLOW_LIMITS};
use orbit_syntax::{Diagnostic, Span};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

/// Rule types, their diagnostic codes and the keys each accepts besides the
/// common ones (`id`, `type`, `severity`, `description`, `workflows`,
/// `exclude_workflows`).
pub const RULES: &[(&str, &str, &[&str])] = &[
    (
        "require_compensation",
        "P0101",
        &["tasks", "compensate_with"],
    ),
    (
        "require_retry",
        "P0102",
        &["tasks", "min_attempts", "max_attempts"],
    ),
    ("max_attempts", "P0103", &["tasks", "max"]),
    (
        "require_timeout",
        "P0104",
        &["tasks", "max", "queue_timeout"],
    ),
    (
        "require_approval",
        "P0105",
        &["tasks", "signals", "when", "require_condition"],
    ),
    ("require_handler", "P0106", &["handlers"]),
    ("forbid_tasks", "P0107", &["tasks"]),
    ("allow_tasks", "P0108", &["tasks"]),
    ("max_fan_out", "P0109", &["max", "require_max_parallel"]),
    ("require_limits", "P0110", &["keys"]),
    (
        "require_signal_timeout",
        "P0111",
        &["signals", "max", "allow_workflow_timeout"],
    ),
];

/// A waiver or exemption is malformed or names an unknown rule.
pub const INVALID_WAIVER: &str = "P0001";
/// A waiver or exemption suppresses nothing.
pub const UNUSED_WAIVER: &str = "P0002";

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct Header {
    name: Option<String>,
    description: Option<String>,
    workflows: Option<Vec<String>>,
    exclude_workflows: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, Default, Clone)]
#[serde(deny_unknown_fields)]
struct RuleSpec {
    id: Option<String>,
    #[serde(rename = "type")]
    kind: String,
    severity: Option<String>,
    description: Option<String>,
    workflows: Option<Vec<String>>,
    exclude_workflows: Option<Vec<String>>,
    tasks: Option<Vec<String>>,
    compensate_with: Option<Vec<String>>,
    signals: Option<Vec<String>>,
    when: Option<String>,
    require_condition: Option<bool>,
    min_attempts: Option<u32>,
    max_attempts: Option<u32>,
    max: Option<toml::Value>,
    queue_timeout: Option<bool>,
    handlers: Option<Vec<String>>,
    require_max_parallel: Option<bool>,
    keys: Option<Vec<String>>,
    allow_workflow_timeout: Option<bool>,
}

impl RuleSpec {
    fn present_keys(&self) -> Vec<&'static str> {
        let mut out = vec![];
        let mut k = |set: bool, name: &'static str| {
            if set {
                out.push(name)
            }
        };
        k(self.tasks.is_some(), "tasks");
        k(self.compensate_with.is_some(), "compensate_with");
        k(self.signals.is_some(), "signals");
        k(self.when.is_some(), "when");
        k(self.require_condition.is_some(), "require_condition");
        k(self.min_attempts.is_some(), "min_attempts");
        k(self.max_attempts.is_some(), "max_attempts");
        k(self.max.is_some(), "max");
        k(self.queue_timeout.is_some(), "queue_timeout");
        k(self.handlers.is_some(), "handlers");
        k(self.require_max_parallel.is_some(), "require_max_parallel");
        k(self.keys.is_some(), "keys");
        k(
            self.allow_workflow_timeout.is_some(),
            "allow_workflow_timeout",
        );
        out
    }
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
struct ExemptionSpec {
    rule: String,
    workflow: String,
    step: Option<String>,
    reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PolicyFile {
    policy: Option<Header>,
    #[serde(default)]
    rule: Vec<RuleSpec>,
    #[serde(default)]
    exemption: Vec<ExemptionSpec>,
}

/// `input.<path> <op> <number>`, the condition of a `require_approval` rule.
#[derive(Debug, Clone, PartialEq)]
pub struct Threshold {
    pub path: Vec<String>,
    pub op: Cmp,
    pub value: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmp {
    Lt,
    Le,
    Gt,
    Ge,
    Eq,
    Ne,
}

impl Cmp {
    fn negate(self) -> Cmp {
        match self {
            Cmp::Lt => Cmp::Ge,
            Cmp::Le => Cmp::Gt,
            Cmp::Gt => Cmp::Le,
            Cmp::Ge => Cmp::Lt,
            Cmp::Eq => Cmp::Ne,
            Cmp::Ne => Cmp::Eq,
        }
    }

    fn flip(self) -> Cmp {
        match self {
            Cmp::Lt => Cmp::Gt,
            Cmp::Le => Cmp::Ge,
            Cmp::Gt => Cmp::Lt,
            Cmp::Ge => Cmp::Le,
            other => other,
        }
    }

    fn symbol(self) -> &'static str {
        match self {
            Cmp::Lt => "<",
            Cmp::Le => "<=",
            Cmp::Gt => ">",
            Cmp::Ge => ">=",
            Cmp::Eq => "==",
            Cmp::Ne => "!=",
        }
    }

    fn from_binop(o: BinOp) -> Option<Cmp> {
        Some(match o {
            BinOp::Lt => Cmp::Lt,
            BinOp::Le => Cmp::Le,
            BinOp::Gt => Cmp::Gt,
            BinOp::Ge => Cmp::Ge,
            BinOp::Eq => Cmp::Eq,
            BinOp::Ne => Cmp::Ne,
            _ => return None,
        })
    }
}

impl std::fmt::Display for Threshold {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "input.{} {} {}",
            self.path.join("."),
            self.op.symbol(),
            self.value
        )
    }
}

impl Threshold {
    pub fn parse(s: &str) -> Result<Threshold, String> {
        let bad = || {
            format!("`when = {s:?}`: expected `input.<field> <op> <number>` with <op> one of >=, >, <=, <, ==, !=")
        };
        let (pos, op, len) = [
            (">=", Cmp::Ge),
            ("<=", Cmp::Le),
            ("==", Cmp::Eq),
            ("!=", Cmp::Ne),
            (">", Cmp::Gt),
            ("<", Cmp::Lt),
        ]
        .iter()
        .filter_map(|(sym, op)| s.find(sym).map(|p| (p, *op, sym.len())))
        .min_by_key(|(p, _, len)| (*p, std::cmp::Reverse(*len)))
        .ok_or_else(bad)?;
        let left = s[..pos].trim();
        let right = s[pos + len..].trim();
        let path = left.strip_prefix("input.").ok_or_else(bad)?;
        let path: Vec<String> = path.split('.').map(str::to_string).collect();
        if path
            .iter()
            .any(|p| p.is_empty() || !p.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        {
            return Err(bad());
        }
        let value: f64 = right.parse().map_err(|_| bad())?;
        Ok(Threshold { path, op, value })
    }

    fn negate(&self) -> Threshold {
        Threshold {
            op: self.op.negate(),
            ..self.clone()
        }
    }
}

/// Does `a` (on the same input path) imply `b`? Sound for real numbers.
fn atom_implies(a: &Threshold, b: &Threshold) -> bool {
    if a.path != b.path {
        return false;
    }
    let (x, y) = (a.value, b.value);
    use Cmp::*;
    match (a.op, b.op) {
        (Eq, op) => match op {
            Eq => x == y,
            Ne => x != y,
            Ge => x >= y,
            Gt => x > y,
            Le => x <= y,
            Lt => x < y,
        },
        (Ge, Ge) => x >= y,
        (Ge, Gt) => x > y,
        (Gt, Ge) => x >= y,
        (Gt, Gt) => x >= y,
        (Le, Le) => x <= y,
        (Le, Lt) => x < y,
        (Lt, Le) => x <= y,
        (Lt, Lt) => x <= y,
        (Ge, Ne) => x > y,
        (Gt, Ne) => x >= y,
        (Le, Ne) => x < y,
        (Lt, Ne) => x <= y,
        (Ne, Ne) => x == y,
        _ => false,
    }
}

/// A condition in negation normal form, with comparisons of an input field
/// against a number as atoms and everything else opaque.
#[derive(Debug, Clone)]
enum Formula {
    Const(bool),
    Atom(Threshold),
    /// Anything else (possibly negated; the analysis never looks inside).
    Opaque(Expr),
    And(Box<Formula>, Box<Formula>),
    Or(Box<Formula>, Box<Formula>),
}

/// Compute steps whose fields can be substituted into conditions:
/// unconditional, not fanned out, in the same scope.
type Computes<'a> = HashMap<&'a str, &'a Expr>;

fn resolve<'a>(e: &'a Expr, computes: &Computes<'a>, depth: u8) -> &'a Expr {
    if depth > 8 {
        return e;
    }
    if let Expr::Step { step, path } = e {
        if let Some(body) = computes.get(step.as_str()) {
            return match (path.as_slice(), body) {
                ([], b) => resolve(b, computes, depth + 1),
                ([field], Expr::Object { fields }) => {
                    match fields.iter().find(|f| &f.name == field) {
                        Some(f) => resolve(&f.value, computes, depth + 1),
                        None => e,
                    }
                }
                _ => e,
            };
        }
    }
    e
}

fn number(e: &Expr) -> Option<f64> {
    match e {
        Expr::Literal { value } => value.as_f64(),
        Expr::Unary {
            operator: UnOp::Neg,
            operand,
        } => number(operand).map(|n| -n),
        _ => None,
    }
}

fn formula(e: &Expr, negated: bool, computes: &Computes, depth: u8) -> Formula {
    let e = resolve(e, computes, depth);
    match e {
        Expr::Literal {
            value: serde_json::Value::Bool(b),
        } => Formula::Const(*b != negated),
        Expr::Unary {
            operator: UnOp::Not,
            operand,
        } => formula(operand, !negated, computes, depth + 1),
        Expr::Binary {
            operator: op @ (BinOp::And | BinOp::Or),
            left,
            right,
        } => {
            let (l, r) = (
                Box::new(formula(left, negated, computes, depth + 1)),
                Box::new(formula(right, negated, computes, depth + 1)),
            );
            // De Morgan: a negated `&&` is an `||` of negations.
            if (*op == BinOp::And) != negated {
                Formula::And(l, r)
            } else {
                Formula::Or(l, r)
            }
        }
        Expr::Binary {
            operator,
            left,
            right,
        } if Cmp::from_binop(*operator).is_some() => {
            let cmp = Cmp::from_binop(*operator).unwrap();
            let (l, r) = (
                resolve(left, computes, depth + 1),
                resolve(right, computes, depth + 1),
            );
            let atom = match (l, r, number(l), number(r)) {
                (Expr::Input { path }, _, _, Some(n)) => Some((path.clone(), cmp, n)),
                (_, Expr::Input { path }, Some(n), _) => Some((path.clone(), cmp.flip(), n)),
                _ => None,
            };
            match atom {
                Some((path, op, value)) => Formula::Atom(Threshold {
                    path,
                    op: if negated { op.negate() } else { op },
                    value,
                }),
                None => Formula::Opaque(e.clone()),
            }
        }
        other => Formula::Opaque(other.clone()),
    }
}

/// `p ⇒ f`, where `p` is `None` for "always".
fn implied_by(p: Option<&Threshold>, f: &Formula) -> bool {
    match f {
        Formula::Const(b) => *b,
        Formula::Atom(a) => p.is_some_and(|p| atom_implies(p, a)),
        Formula::Opaque(..) => false,
        Formula::And(a, b) => implied_by(p, a) && implied_by(p, b),
        Formula::Or(a, b) => implied_by(p, a) || implied_by(p, b),
    }
}

/// `f ⇒ g`.
fn implies(f: &Formula, g: &Threshold) -> bool {
    match f {
        Formula::Const(b) => !*b,
        Formula::Atom(a) => atom_implies(a, g),
        Formula::Opaque(..) => false,
        Formula::And(a, b) => implies(a, g) || implies(b, g),
        Formula::Or(a, b) => implies(a, g) && implies(b, g),
    }
}

/// Simplify `f` assuming `p` holds.
fn assume(f: &Formula, p: Option<&Threshold>) -> Formula {
    match f {
        Formula::Atom(a) => match p {
            Some(p) if atom_implies(p, a) => Formula::Const(true),
            Some(p) if atom_implies(p, &a.negate()) => Formula::Const(false),
            _ => f.clone(),
        },
        Formula::And(a, b) => match (assume(a, p), assume(b, p)) {
            (Formula::Const(false), _) | (_, Formula::Const(false)) => Formula::Const(false),
            (Formula::Const(true), x) | (x, Formula::Const(true)) => x,
            (x, y) => Formula::And(Box::new(x), Box::new(y)),
        },
        Formula::Or(a, b) => match (assume(a, p), assume(b, p)) {
            (Formula::Const(true), _) | (_, Formula::Const(true)) => Formula::Const(true),
            (Formula::Const(false), x) | (x, Formula::Const(false)) => x,
            (x, y) => Formula::Or(Box::new(x), Box::new(y)),
        },
        other => other.clone(),
    }
}

fn reads_step(f: &Formula, step: &str) -> bool {
    match f {
        Formula::Opaque(e) => {
            let mut refs = vec![];
            e.step_refs(&mut refs);
            refs.iter().any(|r| r == step)
        }
        Formula::And(a, b) | Formula::Or(a, b) => reads_step(a, step) || reads_step(b, step),
        _ => false,
    }
}

#[derive(Debug, Clone)]
struct Rule {
    id: String,
    kind: &'static str,
    code: &'static str,
    error: bool,
    description: Option<String>,
    workflows: Vec<String>,
    exclude_workflows: Vec<String>,
    spec: RuleSpec,
    threshold: Option<Threshold>,
    max_ms: Option<u64>,
    max_n: Option<u64>,
}

#[derive(Debug, Clone)]
struct Exemption {
    spec: ExemptionSpec,
    index: usize,
}

/// A loaded policy.
#[derive(Debug, Clone)]
pub struct Policy {
    pub name: String,
    pub path: String,
    pub description: Option<String>,
    rules: Vec<Rule>,
    exemptions: Vec<Exemption>,
}

/// The result of checking a compilation against a policy.
#[derive(Debug, Default)]
pub struct PolicyReport {
    /// Violations and waiver problems, located in the `.orbit` sources.
    pub diagnostics: Vec<Diagnostic>,
    /// Warnings about the policy file itself (unused exemptions).
    pub messages: Vec<String>,
}

impl Policy {
    pub fn load(path: &Path) -> Result<Policy, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read policy {}: {e}", path.display()))?;
        Policy::parse(&text, &path.display().to_string())
    }

    pub fn parse(text: &str, path: &str) -> Result<Policy, String> {
        let err = |m: String| format!("policy {path}: {m}");
        let file: PolicyFile = toml::from_str(text).map_err(|e| err(e.to_string()))?;
        let header = file.policy.unwrap_or_default();
        let default_scope = header.workflows.clone().unwrap_or_else(|| vec!["*".into()]);
        let default_exclude = header.exclude_workflows.clone().unwrap_or_default();
        let mut rules = vec![];
        let mut ids = HashSet::new();
        for (i, spec) in file.rule.into_iter().enumerate() {
            let n = i + 1;
            let Some(&(kind, code, keys)) = RULES.iter().find(|(k, _, _)| *k == spec.kind) else {
                return Err(err(format!(
                    "rule {n}: unknown type `{}`; known types: {}",
                    spec.kind,
                    RULES.iter().map(|r| r.0).collect::<Vec<_>>().join(", ")
                )));
            };
            let id = spec.id.clone().unwrap_or_else(|| kind.to_string());
            if !ids.insert(id.clone()) {
                return Err(err(format!(
                    "rule {n}: duplicate id `{id}` (give each rule of the same type its own `id`)"
                )));
            }
            if id.is_empty() || id.contains(char::is_whitespace) {
                return Err(err(format!("rule {n}: `id` must be a non-empty word")));
            }
            for k in spec.present_keys() {
                if !keys.contains(&k) {
                    return Err(err(format!(
                        "rule `{id}`: `{k}` does not apply to `{kind}` (it takes: {})",
                        keys.join(", ")
                    )));
                }
            }
            let error = match spec.severity.as_deref() {
                None | Some("error") => true,
                Some("warning") => false,
                Some(other) => {
                    return Err(err(format!(
                        "rule `{id}`: severity must be `error` or `warning`, not `{other}`"
                    )))
                }
            };
            let needs_tasks = matches!(
                kind,
                "require_compensation"
                    | "require_retry"
                    | "require_timeout"
                    | "require_approval"
                    | "forbid_tasks"
                    | "allow_tasks"
            );
            if needs_tasks && spec.tasks.as_ref().is_none_or(|t| t.is_empty()) {
                return Err(err(format!("rule `{id}`: `{kind}` needs `tasks = [...]`")));
            }
            if kind == "require_approval" && spec.signals.as_ref().is_none_or(|s| s.is_empty()) {
                return Err(err(format!(
                    "rule `{id}`: `require_approval` needs `signals = [...]`"
                )));
            }
            let threshold = match &spec.when {
                Some(w) => Some(Threshold::parse(w).map_err(|e| err(format!("rule `{id}`: {e}")))?),
                None => None,
            };
            let (mut max_ms, mut max_n) = (None, None);
            if let Some(m) = &spec.max {
                match kind {
                    "require_timeout" | "require_signal_timeout" => {
                        let d = m.as_str().and_then(parse_duration).ok_or_else(|| {
                            err(format!(
                                "rule `{id}`: `max` must be a duration such as \"30s\", \"5m\" or \"7d\""
                            ))
                        })?;
                        max_ms = Some(d);
                    }
                    _ => {
                        let v = m.as_integer().filter(|v| *v >= 1).ok_or_else(|| {
                            err(format!("rule `{id}`: `max` must be a positive integer"))
                        })?;
                        max_n = Some(v as u64);
                    }
                }
            } else if matches!(kind, "max_attempts" | "max_fan_out") {
                return Err(err(format!("rule `{id}`: `{kind}` needs `max = <n>`")));
            }
            if kind == "require_retry" {
                if let (Some(lo), Some(hi)) = (spec.min_attempts, spec.max_attempts) {
                    if lo > hi {
                        return Err(err(format!(
                            "rule `{id}`: min_attempts ({lo}) is greater than max_attempts ({hi})"
                        )));
                    }
                }
            }
            if let Some(hs) = &spec.handlers {
                if let Some(h) = hs
                    .iter()
                    .find(|h| !orbit_ir::HANDLER_KINDS.contains(&h.as_str()))
                {
                    return Err(err(format!(
                        "rule `{id}`: unknown handler `{h}` (expected failure, cancel or timeout)"
                    )));
                }
            }
            if kind == "require_limits" {
                let keys = spec.keys.as_deref().unwrap_or_default();
                if keys.is_empty() {
                    return Err(err(format!(
                        "rule `{id}`: `require_limits` needs `keys = [...]`"
                    )));
                }
                if let Some(k) = keys.iter().find(|k| !WORKFLOW_LIMITS.contains(&k.as_str())) {
                    return Err(err(format!(
                        "rule `{id}`: unknown limit `{k}` (expected {})",
                        WORKFLOW_LIMITS.join(", ")
                    )));
                }
            }
            rules.push(Rule {
                id,
                kind,
                code,
                error,
                description: spec.description.clone(),
                workflows: spec
                    .workflows
                    .clone()
                    .unwrap_or_else(|| default_scope.clone()),
                exclude_workflows: spec
                    .exclude_workflows
                    .clone()
                    .unwrap_or_else(|| default_exclude.clone()),
                spec,
                threshold,
                max_ms,
                max_n,
            });
        }
        let mut exemptions = vec![];
        for (i, e) in file.exemption.into_iter().enumerate() {
            if !rules.iter().any(|r| r.id == e.rule) {
                return Err(err(format!(
                    "exemption {}: unknown rule `{}`",
                    i + 1,
                    e.rule
                )));
            }
            if e.reason.as_deref().is_none_or(|r| r.trim().is_empty()) {
                return Err(err(format!(
                    "exemption {} (rule `{}`, workflow `{}`): `reason` is required",
                    i + 1,
                    e.rule,
                    e.workflow
                )));
            }
            exemptions.push(Exemption {
                spec: e,
                index: i + 1,
            });
        }
        let name = header.name.unwrap_or_else(|| {
            Path::new(path)
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "policy".into())
        });
        Ok(Policy {
            name,
            path: path.to_string(),
            description: header.description,
            rules,
            exemptions,
        })
    }

    /// Rule ids, in file order.
    pub fn rule_ids(&self) -> Vec<&str> {
        self.rules.iter().map(|r| r.id.as_str()).collect()
    }

    /// Check every workflow of a successful compilation.
    pub fn check(&self, c: &Compilation) -> PolicyReport {
        let idx = SourceIndex::new(c);
        let mut report = PolicyReport::default();
        let mut used_exemptions: HashSet<usize> = HashSet::new();
        for wf in &c.workflows {
            let mut violations = vec![];
            for rule in &self.rules {
                if !glob_any(&rule.workflows, &wf.name)
                    || glob_any(&rule.exclude_workflows, &wf.name)
                {
                    continue;
                }
                check_rule(rule, wf, &mut violations);
            }
            self.report(wf, &idx, violations, &mut used_exemptions, &mut report);
        }
        for e in &self.exemptions {
            if !used_exemptions.contains(&e.index) {
                report.messages.push(format!(
                    "policy {}: exemption {} (rule `{}`, workflow `{}`{}) matched no violation",
                    self.path,
                    e.index,
                    e.spec.rule,
                    e.spec.workflow,
                    e.spec
                        .step
                        .as_ref()
                        .map(|s| format!(", step `{s}`"))
                        .unwrap_or_default()
                ));
            }
        }
        report
            .diagnostics
            .sort_by_key(|d| (d.span.file, d.span.start, d.code));
        report
    }

    fn report(
        &self,
        wf: &WorkflowIr,
        idx: &SourceIndex,
        violations: Vec<Violation>,
        used_exemptions: &mut HashSet<usize>,
        report: &mut PolicyReport,
    ) {
        let src = idx.workflows.get(&wf.name);
        let wf_span = src.map(|s| s.name.span).unwrap_or_default();
        // Inline waivers: `// policy-allow: <rule> <reason>` above a step or
        // the workflow.
        struct Waiver {
            rule: String,
            step: Option<(String, String)>,
            span: Span,
            used: bool,
        }
        let mut waivers: Vec<Waiver> = vec![];
        let mut parse_waivers = |comments: &[(String, Span)], step: Option<(String, String)>| {
            for (text, span) in comments {
                let Some(rest) = text.trim().strip_prefix("policy-allow:") else {
                    continue;
                };
                let rest = rest.trim();
                let (rule, reason) = rest.split_once(char::is_whitespace).unwrap_or((rest, ""));
                if rule.is_empty() || !self.rules.iter().any(|r| r.id == rule) {
                    report.diagnostics.push(
                        Diagnostic::error(
                            INVALID_WAIVER,
                            format!("waiver names unknown policy rule `{rule}`"),
                            *span,
                        )
                        .with_note(format!(
                            "rules in {}: {}",
                            self.path,
                            self.rule_ids().join(", ")
                        )),
                    );
                    continue;
                }
                if reason.trim().is_empty() {
                    report.diagnostics.push(
                        Diagnostic::error(
                            INVALID_WAIVER,
                            format!("waiver for policy rule `{rule}` has no reason"),
                            *span,
                        )
                        .with_help(format!(
                            "write `// policy-allow: {rule} <why this is acceptable>`"
                        )),
                    );
                    continue;
                }
                waivers.push(Waiver {
                    rule: rule.to_string(),
                    step: step.clone(),
                    span: *span,
                    used: false,
                });
            }
        };
        if let Some(s) = src {
            parse_waivers(&s.comments, None);
            for (key, comments) in &s.step_comments {
                parse_waivers(comments, Some(key.clone()));
            }
        }
        for v in violations {
            let step_key = v.step.as_ref().map(|(sc, st)| (sc.clone(), st.clone()));
            // Waivers above the step, then above the workflow.
            let waived = waivers
                .iter_mut()
                .find(|w| w.rule == v.rule.id && (w.step.is_none() || w.step == step_key));
            if let Some(w) = waived {
                w.used = true;
                continue;
            }
            let exempt = self.exemptions.iter().find(|e| {
                e.spec.rule == v.rule.id
                    && super::glob(&e.spec.workflow, &wf.name)
                    && match (&e.spec.step, &v.step) {
                        (None, _) => true,
                        (Some(s), Some((scope, step))) => {
                            *s == *step || *s == qualify_label(scope, step)
                        }
                        (Some(_), None) => false,
                    }
            });
            if let Some(e) = exempt {
                used_exemptions.insert(e.index);
                continue;
            }
            let span = v
                .step
                .as_ref()
                .and_then(|(sc, st)| idx.step(&wf.name, sc, st))
                .map(|i| i.span)
                .unwrap_or(wf_span);
            let message = format!("policy rule `{}`: {}", v.rule.id, v.message);
            let mut d = if v.rule.error {
                Diagnostic::error(v.rule.code, message, span)
            } else {
                Diagnostic::warning(v.rule.code, message, span)
            };
            for n in v.notes {
                d = d.with_note(n);
            }
            if let Some(desc) = &v.rule.description {
                d = d.with_note(format!("policy {} ({}): {desc}", self.name, self.path));
            } else {
                d = d.with_note(format!("required by policy {} ({})", self.name, self.path));
            }
            let waive = format!("// policy-allow: {} <reason>", v.rule.id);
            d = d.with_help(match v.help {
                Some(h) => format!("{h}, or waive it: `{waive}`"),
                None => format!("waive it with `{waive}` above the step or workflow"),
            });
            report.diagnostics.push(d);
        }
        for w in waivers.iter().filter(|w| !w.used) {
            report.diagnostics.push(
                Diagnostic::warning(
                    UNUSED_WAIVER,
                    format!("waiver for policy rule `{}` suppresses nothing", w.rule),
                    w.span,
                )
                .with_help("remove the comment"),
            );
        }
    }
}

/// [`Scope::qualify`] from a scope label.
fn qualify_label(label: &str, step: &str) -> String {
    if label.is_empty() {
        step.to_string()
    } else {
        format!("{label}: {step}")
    }
}

/// Records a violation: the step (or `None` for the workflow), message,
/// notes and help.
type Push<'a> = dyn FnMut(Option<(&Scope, &StepIr)>, String, Vec<String>, Option<String>) + 'a;

struct Violation<'r> {
    rule: &'r Rule,
    /// (scope label, step id), or `None` for the workflow.
    step: Option<(String, String)>,
    message: String,
    notes: Vec<String>,
    help: Option<String>,
}

fn task_of(s: &StepIr) -> Option<&str> {
    match &s.action {
        Action::Task(i) => Some(&i.task),
        _ => None,
    }
}

/// Transitive dependencies of a step within its scope.
fn ancestors<'a>(steps: &'a [StepIr], id: &str) -> Vec<&'a StepIr> {
    let by_id: HashMap<&str, &StepIr> = steps.iter().map(|s| (s.id.as_str(), s)).collect();
    let mut seen: HashSet<&str> = HashSet::new();
    let mut stack: Vec<&str> = by_id
        .get(id)
        .map(|s| s.dependencies.iter().map(String::as_str).collect())
        .unwrap_or_default();
    while let Some(n) = stack.pop() {
        if seen.insert(n) {
            if let Some(s) = by_id.get(n) {
                stack.extend(s.dependencies.iter().map(String::as_str));
            }
        }
    }
    // Execution order, for stable reports.
    steps
        .iter()
        .filter(|s| seen.contains(s.id.as_str()))
        .collect()
}

fn input_field_is_numeric(t: &Type, path: &[String]) -> bool {
    let mut t = t;
    for p in path {
        match t.unwrap_optional().field(p) {
            Some(f) => t = &f.ty,
            None => return false,
        }
    }
    t.unwrap_optional().is_numeric()
}

fn check_rule<'r>(rule: &'r Rule, wf: &WorkflowIr, out: &mut Vec<Violation<'r>>) {
    let spec = &rule.spec;
    let tasks = spec.tasks.clone().unwrap_or_else(|| vec!["*".into()]);
    let mut push = |step: Option<(&Scope, &StepIr)>,
                    message: String,
                    notes: Vec<String>,
                    help: Option<String>| {
        out.push(Violation {
            rule,
            step: step.map(|(sc, s)| (sc.label(), s.id.clone())),
            message,
            notes,
            help,
        })
    };
    let all = super::all_steps(wf);
    match rule.kind {
        "require_compensation" => {
            for (scope, s) in all.iter().filter(|(sc, _)| *sc == Scope::Main) {
                let Some(task) = task_of(s) else { continue };
                if !glob_any(&tasks, task) {
                    continue;
                }
                match &s.compensation {
                    None => push(
                        Some((scope, s)),
                        format!("step `{}` calls `{task}` but has no compensation", s.id),
                        vec![],
                        Some("add a `compensate { worker \"...\" with { ... } }` block".into()),
                    ),
                    Some(c) => {
                        if let Some(with) = &spec.compensate_with {
                            if !glob_any(with, &c.task) {
                                push(
                                    Some((scope, s)),
                                    format!(
                                        "step `{}` compensates `{task}` with `{}`, which is not one of {}",
                                        s.id,
                                        c.task,
                                        with.join(", ")
                                    ),
                                    vec![],
                                    None,
                                );
                            }
                        }
                    }
                }
            }
        }
        "require_retry" | "max_attempts" => {
            let min = if rule.kind == "require_retry" {
                spec.min_attempts.unwrap_or(2)
            } else {
                1
            };
            let max = if rule.kind == "require_retry" {
                spec.max_attempts.map(u64::from)
            } else {
                rule.max_n
            };
            for (scope, s) in &all {
                let (Some(task), Some(inv)) = (task_of(s), s.invocation()) else {
                    continue;
                };
                if !glob_any(&tasks, task) {
                    continue;
                }
                let n = inv.retry.max_attempts;
                let q = scope.qualify(&s.id);
                if n < min {
                    push(
                        Some((scope, s)),
                        format!(
                            "step `{q}` calls `{task}` with {n} attempt(s); at least {min} required"
                        ),
                        vec![],
                        Some(format!("add `retry {{ attempts: {min} }}`")),
                    );
                }
                if let Some(m) = max {
                    if u64::from(n) > m {
                        push(
                            Some((scope, s)),
                            format!(
                                "step `{q}` calls `{task}` with up to {n} attempts; at most {m} allowed"
                            ),
                            vec![],
                            Some(format!("lower `attempts` to {m}")),
                        );
                    }
                }
            }
        }
        "require_timeout" => {
            for (scope, s) in &all {
                let (Some(task), Some(inv)) = (task_of(s), s.invocation()) else {
                    continue;
                };
                if !glob_any(&tasks, task) {
                    continue;
                }
                let q = scope.qualify(&s.id);
                match inv.timeout_ms {
                    None => push(
                        Some((scope, s)),
                        format!("step `{q}` calls `{task}` without a `timeout`"),
                        vec![],
                        Some(format!(
                            "add `timeout {}` to the step",
                            rule.max_ms
                                .map(super::duration)
                                .unwrap_or_else(|| "30s".into())
                        )),
                    ),
                    Some(t) if rule.max_ms.is_some_and(|m| t > m) => push(
                        Some((scope, s)),
                        format!(
                            "step `{q}` has `timeout {}`; at most {} allowed",
                            super::duration(t),
                            super::duration(rule.max_ms.unwrap())
                        ),
                        vec![],
                        None,
                    ),
                    _ => {}
                }
                if spec.queue_timeout == Some(true) && inv.queue_timeout_ms.is_none() {
                    push(
                        Some((scope, s)),
                        format!("step `{q}` calls `{task}` without a `queue_timeout`"),
                        vec![],
                        Some("add `queue_timeout 5m` to the step".into()),
                    );
                }
            }
        }
        "require_approval" => check_approval(rule, wf, &tasks, &mut push),
        "require_handler" => {
            let wanted = spec
                .handlers
                .clone()
                .unwrap_or_else(|| vec!["failure".into()]);
            let missing: Vec<&String> = wanted
                .iter()
                .filter(|h| !wf.handlers.contains_key(*h))
                .collect();
            if !missing.is_empty() {
                push(
                    None,
                    format!(
                        "workflow `{}` has no {} handler",
                        wf.name,
                        missing
                            .iter()
                            .map(|h| format!("`on {h}`"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    vec![],
                    Some(format!("add `on {} {{ step ... }}`", missing[0])),
                );
            }
        }
        "forbid_tasks" | "allow_tasks" => {
            for (scope, s) in &all {
                let calls = task_of(s)
                    .map(|t| (t, false))
                    .into_iter()
                    .chain(s.compensation.as_ref().map(|c| (c.task.as_str(), true)));
                for (task, comp) in calls {
                    let matched = glob_any(&tasks, task);
                    let bad = if rule.kind == "forbid_tasks" {
                        matched
                    } else {
                        !matched
                    };
                    if !bad {
                        continue;
                    }
                    let who = if comp {
                        format!("the compensation of step `{}`", scope.qualify(&s.id))
                    } else {
                        format!("step `{}`", scope.qualify(&s.id))
                    };
                    let verdict = if rule.kind == "forbid_tasks" {
                        "a forbidden task"
                    } else {
                        "a task outside the allowed list"
                    };
                    push(
                        Some((scope, s)),
                        format!("{who} calls `{task}`, {verdict}"),
                        vec![format!("patterns: {}", tasks.join(", "))],
                        None,
                    );
                }
            }
        }
        "max_fan_out" => {
            let max = rule.max_n.unwrap_or(u64::MAX);
            let fan: Vec<&(Scope, &StepIr)> =
                all.iter().filter(|(_, s)| s.for_each.is_some()).collect();
            if fan.is_empty() {
                return;
            }
            match wf.limits.as_ref().and_then(|l| l.max_fan_out) {
                None => push(
                    None,
                    format!(
                        "workflow `{}` has `for` steps ({}) but no `limits {{ max_fan_out: ... }}`",
                        wf.name,
                        fan.iter()
                            .map(|(sc, s)| format!("`{}`", sc.qualify(&s.id)))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    vec![],
                    Some(format!("add `limits {{ max_fan_out: {max} }}`")),
                ),
                Some(n) if n > max => push(
                    None,
                    format!(
                        "workflow `{}` allows `max_fan_out: {n}`; at most {max} allowed",
                        wf.name
                    ),
                    vec![],
                    None,
                ),
                _ => {}
            }
            if spec.require_max_parallel == Some(true) {
                for (scope, s) in fan {
                    if s.for_each
                        .as_ref()
                        .is_some_and(|f| f.max_parallel.is_none())
                    {
                        push(
                            Some((scope, s)),
                            format!(
                                "`for` step `{}` has no `max_parallel`",
                                scope.qualify(&s.id)
                            ),
                            vec![],
                            Some("add `max_parallel <n>`".into()),
                        );
                    }
                }
            }
        }
        "require_limits" => {
            let keys = spec.keys.clone().unwrap_or_default();
            let l = wf.limits.clone().unwrap_or_default();
            let missing: Vec<&String> = keys
                .iter()
                .filter(|k| match k.as_str() {
                    "max_fan_out" => l.max_fan_out.is_none(),
                    "max_history_events" => l.max_history_events.is_none(),
                    _ => l.max_payload_bytes.is_none(),
                })
                .collect();
            if !missing.is_empty() {
                push(
                    None,
                    format!(
                        "workflow `{}` does not set {} in `limits`",
                        wf.name,
                        missing
                            .iter()
                            .map(|k| format!("`{k}`"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    vec![],
                    Some(format!(
                        "add `limits {{ {} }}`",
                        missing
                            .iter()
                            .map(|k| format!("{k}: ..."))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )),
                );
            }
        }
        "require_signal_timeout" => {
            let signals = spec.signals.clone().unwrap_or_else(|| vec!["*".into()]);
            let wf_ok = spec.allow_workflow_timeout == Some(true)
                && wf
                    .timeout_ms
                    .is_some_and(|t| rule.max_ms.is_none_or(|m| t <= m));
            for (scope, s) in &all {
                let Action::Await { signal, timeout_ms } = &s.action else {
                    continue;
                };
                if !glob_any(&signals, signal) {
                    continue;
                }
                let q = scope.qualify(&s.id);
                match timeout_ms {
                    None if !wf_ok => push(
                        Some((scope, s)),
                        format!("step `{q}` waits for signal `{signal}` without a step `timeout`"),
                        vec![],
                        Some(format!(
                            "add `timeout {}` to the step",
                            rule.max_ms
                                .map(super::duration)
                                .unwrap_or_else(|| "7d".into())
                        )),
                    ),
                    Some(t) if rule.max_ms.is_some_and(|m| *t > m) => push(
                        Some((scope, s)),
                        format!(
                            "step `{q}` waits up to {} for `{signal}`; at most {} allowed",
                            super::duration(*t),
                            super::duration(rule.max_ms.unwrap())
                        ),
                        vec![],
                        None,
                    ),
                    _ => {}
                }
            }
        }
        _ => {}
    }
}

/// `require_approval`: see `docs/src/tooling/policy.md#require_approval`.
///
/// A step `S` calling a matching task passes if either
///
/// 1. the rule has a `when` threshold `P` and `S`'s `when` condition implies
///    `not P` (`S` never runs when the threshold is reached), or
/// 2. some transitive dependency `A` of `S` (in the same scope) is an `await`
///    of a matching signal, `A` runs whenever `P` holds (it has no `when`,
///    or `P` implies its `when`), and, unless `require_condition = false`,
///    `S`'s `when` condition, simplified under `P`, still reads `A`'s output.
///
/// Conditions are analysed after substituting the fields of unconditional,
/// non-`for` `compute` steps of the same scope. Only comparisons of an
/// `input` field with a number literal are understood; anything else is
/// treated as unknown, which can only make the check fail, never pass.
fn check_approval(rule: &Rule, wf: &WorkflowIr, tasks: &[String], push: &mut Push) {
    let signals = rule.spec.signals.clone().unwrap_or_default();
    let p = rule.threshold.as_ref();
    let gate = rule.spec.require_condition.unwrap_or(true);
    for scope in super::scopes(wf) {
        let steps = super::scope_steps(wf, scope);
        let computes: Computes = steps
            .iter()
            .filter(|s| s.condition.is_none() && s.for_each.is_none())
            .filter_map(|s| match &s.action {
                Action::Compute { expr } => Some((s.id.as_str(), expr)),
                _ => None,
            })
            .collect();
        for s in steps {
            let Some(task) = task_of(s) else { continue };
            if !glob_any(tasks, task) {
                continue;
            }
            let q = scope.qualify(&s.id);
            if let Some(p) = p {
                if !input_field_is_numeric(&wf.input, &p.path) {
                    push(
                        Some((&scope, s)),
                        format!(
                            "step `{q}` calls `{task}`, but the workflow input has no numeric field `{}` to evaluate `{p}`",
                            p.path.join(".")
                        ),
                        vec!["the check is conservative: it cannot show that the threshold is never reached".into()],
                        None,
                    );
                    continue;
                }
            }
            let cond = s
                .condition
                .as_ref()
                .map(|c| formula(c, false, &computes, 0));
            // 1. The step cannot run when the threshold is reached.
            if let (Some(p), Some(f)) = (p, &cond) {
                if implies(f, &p.negate()) {
                    continue;
                }
            }
            // 2. An approval wait guards it.
            let awaits: Vec<&StepIr> = ancestors(steps, &s.id)
                .into_iter()
                .filter(|a| matches!(&a.action, Action::Await { signal, .. } if glob_any(&signals, signal)))
                .collect();
            let when = p.map(|p| format!(" when {p}")).unwrap_or_default();
            if awaits.is_empty() {
                push(
                    Some((&scope, s)),
                    format!(
                        "step `{q}` calls `{task}` without depending on an approval (`await` of {}){when}",
                        signals.iter().map(|x| format!("`{x}`")).collect::<Vec<_>>().join(", ")
                    ),
                    vec![],
                    Some(format!(
                        "add a step that awaits the approval signal and make `{}` depend on it (`after` or by reading its output)",
                        s.id
                    )),
                );
                continue;
            }
            let mut reasons = vec![];
            let mut ok = false;
            for a in &awaits {
                let a_runs = match &a.condition {
                    None => true,
                    Some(c) => implied_by(p, &formula(c, false, &computes, 0)),
                };
                if !a_runs {
                    reasons.push(format!(
                        "`{}` may be skipped{when}: its condition `{}` is not implied by the rule",
                        a.id,
                        super::pretty::expr(a.condition.as_ref().unwrap())
                    ));
                    continue;
                }
                if gate {
                    let reads = cond
                        .as_ref()
                        .map(|f| assume(f, p))
                        .is_some_and(|f| !matches!(f, Formula::Const(_)) && reads_step(&f, &a.id));
                    if !reads {
                        reasons.push(format!(
                            "the `when` condition of `{}` does not read `{}`{when}, so the decision does not gate the step",
                            s.id, a.id
                        ));
                        continue;
                    }
                }
                ok = true;
                break;
            }
            if !ok {
                push(
                    Some((&scope, s)),
                    format!("step `{q}` calls `{task}` without a guaranteed approval{when}"),
                    reasons,
                    Some(format!(
                        "make `{}` read the decision, e.g. `when coalesce({}.approved, false)`",
                        s.id, awaits[0].id
                    )),
                );
            }
        }
    }
}

/// The diagnostic code of every rule type, for documentation.
pub fn codes() -> BTreeMap<&'static str, &'static str> {
    RULES.iter().map(|(k, c, _)| (*k, *c)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile_str;

    const CONTRACTS: &str = r#"
task "payments.charge" { version "1.0.0" input { amount: Float } output { id: String } }
task "payments.refund" { version "1.0.0" input { id: String } output { ok: Bool } }
task "legacy.fax" { version "1.0.0" input { to: String } output { ok: Bool } }
task "mail.send" { version "1.0.0" input { to: String } output { ok: Bool } }
"#;

    fn run(policy: &str, wf: &str) -> Vec<String> {
        let c = compile_str("w.orbit", &format!("{CONTRACTS}\n{wf}"));
        assert!(!c.has_errors(), "{}", c.render_diagnostics(false));
        let p = Policy::parse(policy, "test.toml").unwrap();
        let r = p.check(&c);
        r.diagnostics
            .iter()
            .map(|d| {
                format!(
                    "{}[{}] {}",
                    if d.is_error() { "E" } else { "W" },
                    d.code,
                    d.message
                )
            })
            .chain(r.messages)
            .collect()
    }

    const CHARGE: &str = r#"
workflow Pay {
    version "1.0.0"
    input { amount: Float, to: String }
    step charge {
        worker "payments.charge"
        with { amount: input.amount }
    }
}
"#;

    const CHARGE_OK: &str = r#"
workflow Pay {
    version "1.0.0"
    input { amount: Float, to: String }
    step charge {
        worker "payments.charge"
        with { amount: input.amount }
        retry { attempts: 3 }
        timeout 30s
        compensate { worker "payments.refund" with { id: charge.id } }
    }
    on failure { step tell { worker "mail.send" with { to: input.to } } }
}
"#;

    #[test]
    fn require_compensation() {
        let p = "[[rule]]\ntype = \"require_compensation\"\ntasks = [\"payments.*\"]\n";
        assert_eq!(
            run(p, CHARGE),
            ["E[P0101] policy rule `require_compensation`: step `charge` calls `payments.charge` but has no compensation"]
        );
        assert!(run(p, CHARGE_OK).is_empty());
        let with = format!("{p}compensate_with = [\"payments.void\"]\n");
        assert_eq!(run(&with, CHARGE_OK).len(), 1);
    }

    #[test]
    fn retry_bounds_and_timeouts() {
        let p = "[[rule]]\ntype = \"require_retry\"\ntasks = [\"payments.*\"]\nmin_attempts = 2\nmax_attempts = 5\n";
        assert_eq!(
            run(p, CHARGE),
            ["E[P0102] policy rule `require_retry`: step `charge` calls `payments.charge` with 1 attempt(s); at least 2 required"]
        );
        assert!(run(p, CHARGE_OK).is_empty());
        let p =
            "[[rule]]\ntype = \"max_attempts\"\ntasks = [\"*\"]\nmax = 2\nseverity = \"warning\"\n";
        assert_eq!(
            run(p, CHARGE_OK),
            ["W[P0103] policy rule `max_attempts`: step `charge` calls `payments.charge` with up to 3 attempts; at most 2 allowed"]
        );
        assert!(run(p, CHARGE).is_empty());
        let p = "[[rule]]\ntype = \"require_timeout\"\ntasks = [\"payments.*\"]\nmax = \"10s\"\n";
        assert_eq!(
            run(p, CHARGE),
            ["E[P0104] policy rule `require_timeout`: step `charge` calls `payments.charge` without a `timeout`"]
        );
        assert_eq!(
            run(p, CHARGE_OK),
            ["E[P0104] policy rule `require_timeout`: step `charge` has `timeout 30s`; at most 10s allowed"]
        );
        let p = "[[rule]]\ntype = \"require_timeout\"\ntasks = [\"payments.*\"]\nqueue_timeout = true\n";
        assert_eq!(run(p, CHARGE_OK).len(), 1);
    }

    #[test]
    fn handlers_task_lists_and_scoping() {
        let p = "[[rule]]\ntype = \"require_handler\"\nhandlers = [\"failure\", \"cancel\"]\n";
        assert_eq!(
            run(p, CHARGE),
            ["E[P0106] policy rule `require_handler`: workflow `Pay` has no `on failure`, `on cancel` handler"]
        );
        assert_eq!(
            run(p, CHARGE_OK),
            ["E[P0106] policy rule `require_handler`: workflow `Pay` has no `on cancel` handler"]
        );
        let scoped = format!("{p}workflows = [\"Other*\"]\n");
        assert!(run(&scoped, CHARGE).is_empty());
        let excluded = format!("{p}exclude_workflows = [\"Pay\"]\n");
        assert!(run(&excluded, CHARGE).is_empty());
        let header = format!("[policy]\nworkflows = [\"Nope\"]\n{p}");
        assert!(run(&header, CHARGE).is_empty());

        let p = "[[rule]]\ntype = \"forbid_tasks\"\ntasks = [\"legacy.*\", \"payments.refund\"]\n";
        assert_eq!(
            run(p, CHARGE_OK),
            ["E[P0107] policy rule `forbid_tasks`: the compensation of step `charge` calls `payments.refund`, a forbidden task"]
        );
        let p = "[[rule]]\ntype = \"allow_tasks\"\ntasks = [\"payments.*\"]\n";
        assert_eq!(
            run(p, CHARGE_OK),
            ["E[P0108] policy rule `allow_tasks`: step `on failure: tell` calls `mail.send`, a task outside the allowed list"]
        );
    }

    const FAN: &str = r#"
workflow Fan {
    version "1.0.0"
    input { tos: List<String> }
    step mail { for t in input.tos worker "mail.send" with { to: t } }
}
"#;

    #[test]
    fn fan_out_limits_and_signal_timeouts() {
        let p = "[[rule]]\ntype = \"max_fan_out\"\nmax = 100\nrequire_max_parallel = true\n";
        assert_eq!(
            run(p, FAN),
            [
                "E[P0109] policy rule `max_fan_out`: workflow `Fan` has `for` steps (`mail`) but no `limits { max_fan_out: ... }`",
                "E[P0109] policy rule `max_fan_out`: `for` step `mail` has no `max_parallel`",
            ]
        );
        let limited = FAN.replace(
            "input { tos",
            "limits { max_fan_out: 500 }\n    input { tos",
        );
        assert_eq!(
            run(p, &limited)[0],
            "E[P0109] policy rule `max_fan_out`: workflow `Fan` allows `max_fan_out: 500`; at most 100 allowed"
        );
        let good = FAN
            .replace("input { tos", "limits { max_fan_out: 50 }\n    input { tos")
            .replace(
                "worker \"mail.send\"",
                "max_parallel 4 worker \"mail.send\"",
            );
        assert!(run(p, &good).is_empty());
        assert!(run(p, CHARGE).is_empty());

        let p = "[[rule]]\ntype = \"require_limits\"\nkeys = [\"max_history_events\"]\n";
        assert_eq!(
            run(p, FAN),
            ["E[P0110] policy rule `require_limits`: workflow `Fan` does not set `max_history_events` in `limits`"]
        );

        let wait = |t: &str| {
            format!(
                "workflow W {{ version \"1.0.0\" {t}\n signal go {{ ok: Bool }}\n step wait {{ await go }} }}"
            )
        };
        let p = "[[rule]]\ntype = \"require_signal_timeout\"\nmax = \"7d\"\n";
        assert_eq!(
            run(p, &wait("")),
            ["E[P0111] policy rule `require_signal_timeout`: step `wait` waits for signal `go` without a step `timeout`"]
        );
        assert_eq!(run(p, &wait("timeout 1d")).len(), 1);
        let allow = format!("{p}allow_workflow_timeout = true\n");
        assert!(run(&allow, &wait("timeout 1d")).is_empty());
        assert_eq!(run(&allow, &wait("timeout 30d")).len(), 1);
    }

    const APPROVAL: &str = r#"[[rule]]
type = "require_approval"
tasks = ["payments.charge"]
signals = ["review"]
when = "input.amount >= 10000"
"#;

    fn approval_wf(decision_when: &str, charge_when: &str) -> String {
        format!(
            r#"
workflow Pay {{
    version "1.0.0"
    input {{ amount: Float, to: String }}
    signal review {{ approved: Bool }}
    step assess {{ compute {{ big: input.amount >= 1000.0 }} }}
    step decision {{ await review {decision_when} timeout 1d }}
    step charge {{
        worker "payments.charge"
        {charge_when}
        with {{ amount: input.amount }}
        after decision
    }}
}}
"#
        )
    }

    #[test]
    fn require_approval_passes_when_guarded() {
        // The example in examples/approval.orbit: the wait runs for every
        // order of 1000 or more, and the charge reads the decision.
        let wf = approval_wf(
            "when assess.big",
            "when !assess.big || coalesce(decision.approved, false)",
        );
        assert!(run(APPROVAL, &wf).is_empty(), "{:?}", run(APPROVAL, &wf));
        // An unconditional wait and a gate.
        let wf = approval_wf("", "when decision.approved");
        assert!(run(APPROVAL, &wf).is_empty());
        // The step can only run below the threshold.
        let wf = approval_wf("", "when input.amount < 5000.0 && input.to != \"x\"");
        assert!(run(APPROVAL, &wf).is_empty());
    }

    #[test]
    fn require_approval_fails_when_not_guarded() {
        // The wait is skipped for orders between 10000 and 50000.
        let wf = approval_wf(
            "when input.amount >= 50000.0",
            "when coalesce(decision.approved, false)",
        );
        let r = run(APPROVAL, &wf);
        assert_eq!(
            r,
            ["E[P0105] policy rule `require_approval`: step `charge` calls `payments.charge` without a guaranteed approval when input.amount >= 10000"]
        );
        // The decision does not gate the charge.
        let wf = approval_wf("", "");
        assert_eq!(run(APPROVAL, &wf).len(), 1);
        // Reading the decision only below the threshold does not count.
        let wf = approval_wf(
            "",
            "when input.amount < 10000.0 || decision.approved || true",
        );
        assert_eq!(run(APPROVAL, &wf).len(), 1);
        // No wait at all.
        assert_eq!(
            run(APPROVAL, CHARGE),
            ["E[P0105] policy rule `require_approval`: step `charge` calls `payments.charge` without depending on an approval (`await` of `review`) when input.amount >= 10000"]
        );
        // The gate can be turned off.
        let lax = format!("{APPROVAL}require_condition = false\n");
        assert!(run(&lax, &approval_wf("", "")).is_empty());
        // Without a threshold the wait must be unconditional.
        let always = APPROVAL.replace("when = \"input.amount >= 10000\"\n", "");
        assert_eq!(
            run(
                &always,
                &approval_wf("when assess.big", "when coalesce(decision.approved, false)")
            )
            .len(),
            1
        );
        assert!(run(&always, &approval_wf("", "when decision.approved")).is_empty());
    }

    #[test]
    fn require_approval_missing_field_is_conservative() {
        let p = APPROVAL.replace("input.amount", "input.total");
        assert_eq!(run(&p, &approval_wf("", "when decision.approved")).len(), 1);
    }

    #[test]
    fn threshold_logic() {
        let t = |s: &str| Threshold::parse(s).unwrap();
        assert!(atom_implies(&t("input.a >= 10"), &t("input.a >= 5")));
        assert!(atom_implies(&t("input.a > 10"), &t("input.a >= 10")));
        assert!(!atom_implies(&t("input.a >= 10"), &t("input.a > 10")));
        assert!(atom_implies(&t("input.a < 3"), &t("input.a != 3")));
        assert!(atom_implies(&t("input.a == 3"), &t("input.a <= 3")));
        assert!(!atom_implies(&t("input.a >= 10"), &t("input.b >= 10")));
        assert!(Threshold::parse("amount >= 1").is_err());
        assert!(Threshold::parse("input.amount >= lots").is_err());
        assert_eq!(t("input.x.y<=-2").to_string(), "input.x.y <= -2");
    }

    #[test]
    fn waivers_and_exemptions() {
        let p =
            "[[rule]]\nid = \"comp\"\ntype = \"require_compensation\"\ntasks = [\"payments.*\"]\n";
        let waived = CHARGE.replace(
            "    step charge {",
            "    // policy-allow: comp the provider voids uncaptured charges itself\n    step charge {",
        );
        assert!(run(p, &waived).is_empty());
        let no_reason = CHARGE.replace(
            "    step charge {",
            "    // policy-allow: comp\n    step charge {",
        );
        let r = run(p, &no_reason);
        assert!(
            r.contains(&"E[P0001] waiver for policy rule `comp` has no reason".to_string()),
            "{r:?}"
        );
        let unknown = CHARGE.replace(
            "    step charge {",
            "    // policy-allow: nope because\n    step charge {",
        );
        assert!(run(p, &unknown)
            .contains(&"E[P0001] waiver names unknown policy rule `nope`".to_string()));
        let unused = CHARGE_OK.replace(
            "    step charge {",
            "    // policy-allow: comp because\n    step charge {",
        );
        assert_eq!(
            run(p, &unused),
            ["W[P0002] waiver for policy rule `comp` suppresses nothing"]
        );
        // A waiver above the workflow covers all its steps.
        let wf_waiver = CHARGE.replace(
            "workflow Pay",
            "// policy-allow: comp legacy flow\nworkflow Pay",
        );
        assert!(run(p, &wf_waiver).is_empty());

        let ex = format!("{p}[[exemption]]\nrule = \"comp\"\nworkflow = \"Pay\"\nstep = \"charge\"\nreason = \"ticket 12\"\n");
        assert!(run(&ex, CHARGE).is_empty());
        assert_eq!(
            run(&ex, CHARGE_OK),
            ["policy test.toml: exemption 1 (rule `comp`, workflow `Pay`, step `charge`) matched no violation"]
        );
        let no = format!("{p}[[exemption]]\nrule = \"comp\"\nworkflow = \"Pay\"\n");
        assert!(Policy::parse(&no, "p.toml")
            .unwrap_err()
            .contains("`reason` is required"));
    }

    #[test]
    fn policy_file_errors() {
        let e = |s: &str| Policy::parse(s, "p.toml").unwrap_err();
        assert!(e("[[rule]]\ntype = \"nope\"\n").contains("unknown type `nope`"));
        assert!(e("[[rule]]\ntype = \"require_handler\"\ntasks = [\"x\"]\n")
            .contains("`tasks` does not apply to `require_handler`"));
        assert!(e("[[rule]]\ntype = \"require_retry\"\n").contains("needs `tasks"));
        assert!(e("[[rule]]\ntype = \"max_attempts\"\n").contains("needs `max = <n>`"));
        assert!(
            e("[[rule]]\ntype = \"require_timeout\"\ntasks = [\"*\"]\nmax = 5\n")
                .contains("duration")
        );
        assert!(
            e("[[rule]]\ntype = \"require_handler\"\nseverity = \"fatal\"\n").contains("severity")
        );
        assert!(
            e("[[rule]]\ntype = \"require_handler\"\n[[rule]]\ntype = \"require_handler\"\n")
                .contains("duplicate id")
        );
        assert!(
            e("[[rule]]\ntype = \"require_limits\"\nkeys = [\"max_steps\"]\n")
                .contains("unknown limit")
        );
        assert!(e("[[rule]]\ntype = \"require_handler\"\nbogus = 1\n").contains("unknown field"));
        assert!(
            e("[[rule]]\ntype = \"require_approval\"\ntasks = [\"x\"]\n")
                .contains("needs `signals")
        );
    }
}
