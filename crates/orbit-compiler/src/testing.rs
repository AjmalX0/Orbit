//! Type checking and lowering of `test` declarations (diagnostics E07xx).
//!
//! Tests are checked against the compiled workflows: the workflow must exist,
//! the input and signal payloads are constants typed like an execution input
//! and a signal, every mock names a task the workflow (or one of its child
//! workflows) calls, and mock results are typed by the task's contract with
//! `input` bound to the task's input.

use crate::check::{Checker, Env};
use orbit_ir::testing::{
    Expectation, MockBehaviour, MockOutcome, TestCase, TestSignal, TEST_STEP_STATUSES,
    TEST_WORKFLOW_STATUSES,
};
use orbit_ir::WORKFLOW_CONTRACT_PREFIX;
use orbit_ir::{self as ir, FieldType, SourceLocation, TaskContract, Type, WorkflowIr};
use orbit_syntax::ast::{self, Declaration, ExpectKind, StepExpectation};
use orbit_syntax::diagnostic::{suggest, Diagnostic};
use orbit_syntax::{FileId, SourceMap, Span};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};

/// Check every `test` declaration and lower the valid ones.
///
/// `defined` holds the names of every workflow declared in the sources, so a
/// test of a workflow that failed to compile is not reported a second time.
pub(crate) fn lower_tests(
    programs: &[(FileId, ast::Program)],
    workflows: &[WorkflowIr],
    defined: &HashSet<&str>,
    sources: &SourceMap,
    diags: &mut Vec<Diagnostic>,
) -> Vec<TestCase> {
    let mut out = vec![];
    let mut seen: BTreeMap<&str, Span> = BTreeMap::new();
    for (_, program) in programs {
        for decl in &program.declarations {
            let Declaration::Test(t) = decl else { continue };
            if let Some(prev) = seen.get(t.name.as_str()) {
                diags.push(
                    Diagnostic::error(
                        "E0709",
                        format!("test `{}` is declared more than once", t.name),
                        t.name_span,
                    )
                    .with_label(*prev, "first declared here"),
                );
                continue;
            }
            seen.insert(&t.name, t.name_span);
            if let Some(tc) = lower_test(t, workflows, defined, sources, diags) {
                out.push(tc);
            }
        }
    }
    out
}

fn lower_test(
    t: &ast::TestDecl,
    workflows: &[WorkflowIr],
    defined: &HashSet<&str>,
    sources: &SourceMap,
    diags: &mut Vec<Diagnostic>,
) -> Option<TestCase> {
    let start_errors = diags.iter().filter(|d| d.is_error()).count();
    let Some(wf_name) = &t.workflow else {
        diags.push(
            Diagnostic::error(
                "E0701",
                format!("test `{}` does not name a workflow", t.name),
                t.name_span,
            )
            .with_help("add `workflow <Name>` to the test"),
        );
        return None;
    };
    let Some(wf) = workflows.iter().find(|w| w.name == wf_name.name) else {
        if !defined.contains(wf_name.name.as_str()) {
            let mut d = Diagnostic::error(
                "E0700",
                format!("unknown workflow `{}`", wf_name.name),
                wf_name.span,
            );
            if let Some(s) = suggest(&wf_name.name, defined.iter().copied()) {
                d = d.with_help(format!("did you mean `{s}`?"));
            } else {
                d = d.with_help("import the file that defines it, e.g. `import \"orders.orbit\"`");
            }
            diags.push(d);
        }
        return None;
    };
    if let Some((v, span)) = &t.version {
        if *v != wf.version {
            diags.push(
                Diagnostic::error(
                    "E0702",
                    format!(
                        "test expects `{}` version {v}, but the sources define version {}",
                        wf.name, wf.version
                    ),
                    *span,
                )
                .with_help("update the test's `version`, or remove it to test whatever version is compiled"),
            );
        }
    }

    // --- Input: a constant checked like an execution input.
    let empty = ast::Expr::Object(vec![], t.name_span);
    let input_expr = t.input.as_ref().unwrap_or(&empty);
    let before = diags.len();
    let mut input = constant(input_expr, &wf.input, "a test input", diags).unwrap_or(Value::Null);
    if t.input.is_none() {
        // Missing required fields are reported at the test name; say how to fix it.
        for d in &mut diags[before..] {
            if d.code == "E0403" {
                d.help = Some("add `input { ... }` with the workflow's required fields".into());
            }
        }
    }
    wf.input.apply_defaults(&mut input);

    // --- Mocks: every mocked task must be one the workflow can call.
    let used = used_contracts(wf, workflows);
    let mut mocks: BTreeMap<String, Vec<MockBehaviour>> = BTreeMap::new();
    let mut mock_spans: BTreeMap<&str, Span> = BTreeMap::new();
    for m in &t.mocks {
        if let Some(prev) = mock_spans.get(m.task.as_str()) {
            diags.push(
                Diagnostic::error(
                    "E0704",
                    format!("task `{}` is mocked more than once", m.task),
                    m.task_span,
                )
                .with_label(*prev, "first mocked here")
                .with_help("chain behaviours instead: `fails \"X\" times 1 then returns { ... }`"),
            );
            continue;
        }
        mock_spans.insert(&m.task, m.task_span);
        let Some(contract) = used.get(m.task.as_str()) else {
            let mut d = Diagnostic::error(
                "E0703",
                format!("`{}` does not call task `{}`", wf.name, m.task),
                m.task_span,
            );
            if let Some(s) = suggest(&m.task, used.keys().copied()) {
                d = d.with_help(format!("did you mean `{s}`?"));
            } else if used.is_empty() {
                d = d.with_note(format!("`{}` calls no worker tasks", wf.name));
            } else {
                d = d.with_note(format!(
                    "tasks `{}` can call: {}",
                    wf.name,
                    used.keys().copied().collect::<Vec<_>>().join(", ")
                ));
            }
            diags.push(d);
            continue;
        };
        let mut behaviours = vec![];
        for b in &m.behaviours {
            let times = match b.times {
                Some((n, span)) if n < 1 || n > u32::MAX as i64 => {
                    diags.push(Diagnostic::error(
                        "E0711",
                        format!("`times {n}` must be at least 1"),
                        span,
                    ));
                    None
                }
                Some((n, _)) => Some(n as u32),
                None => None,
            };
            let outcome = match &b.outcome {
                ast::MockOutcome::Returns(e) => {
                    let steps = BTreeMap::new();
                    let env = Env {
                        input: Some(&contract.input),
                        steps: &steps,
                        allowed: None,
                        context: "a mock result",
                        locals: &[],
                    };
                    let (_, output) = Checker { env: &env, diags }.check(e, Some(&contract.output));
                    MockOutcome::Returns { output }
                }
                ast::MockOutcome::Fails {
                    code,
                    message,
                    retryable,
                } => MockOutcome::Fails {
                    code: code.clone(),
                    message: message
                        .clone()
                        .unwrap_or_else(|| format!("mocked failure of `{}`", m.task)),
                    retryable: *retryable,
                },
                ast::MockOutcome::Hangs => MockOutcome::Hangs,
            };
            behaviours.push(MockBehaviour { outcome, times });
        }
        mocks.insert(m.task.clone(), behaviours);
    }

    // --- Signals: declared by the workflow, constant payloads of its type.
    let mut signals = vec![];
    for s in &t.signals {
        let Some(ty) = wf.signals.get(&s.name.name) else {
            let mut d = Diagnostic::error(
                "E0705",
                format!("`{}` declares no signal `{}`", wf.name, s.name.name),
                s.name.span,
            );
            if let Some(sug) = suggest(&s.name.name, wf.signals.keys().map(String::as_str)) {
                d = d.with_help(format!("did you mean `{sug}`?"));
            } else if wf.signals.is_empty() {
                d = d.with_note(format!("`{}` declares no signals", wf.name));
            }
            diags.push(d);
            continue;
        };
        let empty = ast::Expr::Object(vec![], s.span);
        let payload = constant(
            s.payload.as_ref().unwrap_or(&empty),
            ty,
            "a signal payload",
            diags,
        );
        if let Some(mut payload) = payload {
            ty.apply_defaults(&mut payload);
            signals.push(TestSignal {
                name: s.name.name.clone(),
                payload,
            });
        }
    }

    // --- Expectations.
    let step_names: Vec<&str> = wf.steps.iter().map(|s| s.id.as_str()).collect();
    let find_step = |name: &ast::Ident, diags: &mut Vec<Diagnostic>| -> Option<&ir::StepIr> {
        let found = match name
            .name
            .strip_prefix("on_event:")
            .and_then(|r| r.split_once(':'))
        {
            Some((signal, id)) => wf.event_step(signal, id),
            None => match ir::parse_handler_key(&name.name) {
                Some((kind, id)) => wf.handler_step(kind, id),
                None => wf.step(&name.name),
            },
        };
        if found.is_none() {
            let mut d = Diagnostic::error(
                "E0706",
                format!("`{}` has no step `{}`", wf.name, name.name),
                name.span,
            );
            if let Some(s) = suggest(&name.name, step_names.iter().copied()) {
                d = d.with_help(format!("did you mean `{s}`?"));
            } else {
                d = d.with_note(format!("steps: {}", step_names.join(", ")));
            }
            diags.push(d);
        }
        found
    };
    let status_name =
        |id: &ast::Ident, allowed: &[&str], what: &str, diags: &mut Vec<Diagnostic>| {
            if allowed.contains(&id.name.as_str()) {
                return Some(id.name.clone());
            }
            let mut d =
                Diagnostic::error("E0707", format!("`{}` is not a {what}", id.name), id.span);
            let upper = id.name.to_uppercase();
            if allowed.contains(&upper.as_str()) {
                d = d.with_help(format!("statuses are upper case: `{upper}`"));
            } else if let Some(s) = suggest(&upper, allowed.iter().copied()) {
                d = d.with_help(format!("did you mean `{s}`?"));
            } else {
                d = d.with_note(format!("expected one of: {}", allowed.join(", ")));
            }
            diags.push(d);
            None
        };
    let mut expectations = vec![];
    for e in &t.expectations {
        let lowered = match &e.kind {
            ExpectKind::Status(id) => {
                status_name(id, TEST_WORKFLOW_STATUSES, "workflow status", diags)
                    .map(|status| Expectation::Status { status })
            }
            ExpectKind::Output(expr) => {
                constant(expr, &partial(&wf.output), "an expected output", diags)
                    .map(|expected| Expectation::Output { expected })
            }
            ExpectKind::Error(code, _) => Some(Expectation::Error { code: code.clone() }),
            ExpectKind::Compensated(name) => match find_step(name, diags) {
                Some(step) if step.compensation.is_none() => {
                    diags.push(
                        Diagnostic::error(
                            "E0708",
                            format!("step `{}` has no `compensate` block", name.name),
                            name.span,
                        )
                        .with_note("only steps with a compensation can be compensated"),
                    );
                    None
                }
                Some(step) => Some(Expectation::Compensated {
                    step: step.id.clone(),
                }),
                None => None,
            },
            ExpectKind::Step(name, check) => {
                let step = find_step(name, diags);
                match (step, check) {
                    (None, StepExpectation::Output(expr)) => {
                        // Still type-check the value as a constant.
                        let _ = constant(expr, &Type::Json, "an expected output", diags);
                        None
                    }
                    (None, _) => None,
                    (Some(_), StepExpectation::Status(id)) => {
                        status_name(id, TEST_STEP_STATUSES, "step status", diags).map(|status| {
                            Expectation::StepStatus {
                                step: name.name.clone(),
                                status,
                            }
                        })
                    }
                    (Some(step), StepExpectation::Output(expr)) => constant(
                        expr,
                        &partial(step.output_type.unwrap_optional()),
                        "an expected output",
                        diags,
                    )
                    .map(|expected| Expectation::StepOutput {
                        step: name.name.clone(),
                        expected,
                    }),
                    (Some(_), StepExpectation::Attempts(n, span)) => {
                        if *n < 0 || *n > u32::MAX as i64 {
                            diags.push(Diagnostic::error(
                                "E0711",
                                format!("`attempts {n}` is not a valid number of attempts"),
                                *span,
                            ));
                            None
                        } else {
                            Some(Expectation::StepAttempts {
                                step: name.name.clone(),
                                attempts: *n as u32,
                            })
                        }
                    }
                }
            }
        };
        expectations.extend(lowered);
    }
    if t.expectations.is_empty() {
        diags.push(
            Diagnostic::warning(
                "W0701",
                format!("test `{}` checks nothing", t.name),
                t.name_span,
            )
            .with_help("add an expectation such as `expect status COMPLETED`"),
        );
    }

    let errors = diags.iter().filter(|d| d.is_error()).count();
    if errors > start_errors {
        return None;
    }
    let l = sources.location(t.name_span);
    Some(TestCase {
        name: t.name.clone(),
        workflow: wf.name.clone(),
        version: wf.version.clone(),
        input,
        mocks,
        signals,
        expectations,
        location: SourceLocation {
            file: l.file,
            line: l.line,
            column: l.column,
        },
    })
}

/// Worker tasks (and built-in tasks) a workflow can call, directly, in a
/// compensation, or through its child workflows.
fn used_contracts<'a>(
    wf: &'a WorkflowIr,
    workflows: &'a [WorkflowIr],
) -> BTreeMap<&'a str, &'a TaskContract> {
    let mut out = BTreeMap::new();
    let mut stack = vec![wf];
    let mut visited: HashSet<&str> = HashSet::new();
    while let Some(w) = stack.pop() {
        if !visited.insert(&w.name) {
            continue;
        }
        for (name, c) in &w.contracts {
            match name.strip_prefix(WORKFLOW_CONTRACT_PREFIX) {
                Some(child) => stack.extend(workflows.iter().find(|x| x.name == child)),
                None => {
                    out.insert(name.as_str(), c);
                }
            }
        }
    }
    out
}

/// The type an `expect output` value is checked against: every record field
/// becomes optional, so a test lists only the fields it cares about.
fn partial(ty: &Type) -> Type {
    match ty {
        Type::Record { name, fields } => Type::Record {
            name: name.clone(),
            fields: fields
                .iter()
                .map(|f| FieldType {
                    name: f.name.clone(),
                    ty: partial(&f.ty).optional(),
                    default: None,
                })
                .collect(),
        },
        Type::Optional { inner } => partial(inner).optional(),
        other => other.clone(),
    }
}

/// Check `e` against `ty` and evaluate it; `None` if it has errors.
fn constant(e: &ast::Expr, ty: &Type, context: &str, diags: &mut Vec<Diagnostic>) -> Option<Value> {
    let steps = BTreeMap::new();
    let env = Env {
        input: None,
        steps: &steps,
        allowed: None,
        context,
        locals: &[],
    };
    let before = diags.iter().filter(|d| d.is_error()).count();
    let (_, lowered) = Checker { env: &env, diags }.check(e, Some(ty));
    if diags.iter().filter(|d| d.is_error()).count() != before {
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
    match ir::eval(&lowered, &Empty(Value::Null)) {
        Ok(v) => Some(v),
        Err(err) => {
            diags.push(Diagnostic::error(
                "E0710",
                format!("cannot evaluate {context}: {err}"),
                e.span(),
            ));
            None
        }
    }
}
