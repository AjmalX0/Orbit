//! Milestone 001 acceptance tests: the compiler accepts valid workflows,
//! rejects invalid ones with located diagnostics, and emits deterministic IR.

use orbit_compiler::{compile_files, compile_str, Compilation};
use orbit_ir::{Action, Type, WorkflowIr};
use std::path::PathBuf;

const CONTRACTS: &str = r#"
schema Charge { order_id: String, amount: Float, currency: String = "USD" }
task "orders.validate" { version "1.0.0" input { order_id: String } output { valid: Bool, reason: String? } }
task "risk.analyze" { version "2.1.0" input { amount: Float } output { score: Float } }
task "payments.charge" { version "1.0.0" input Charge output { transaction_id: String } }
task "payments.refund" { version "1.0.0" input { transaction_id: String } output { ok: Bool } }
"#;

fn compile(body: &str) -> Compilation {
    compile_str("test.orbit", &format!("{CONTRACTS}\n{body}"))
}

fn ok(body: &str) -> WorkflowIr {
    let c = compile(body);
    assert!(
        !c.has_errors(),
        "unexpected errors:\n{}",
        c.render_diagnostics(false)
    );
    c.workflows.into_iter().next().expect("one workflow")
}

fn codes(body: &str) -> Vec<&'static str> {
    compile(body)
        .diagnostics
        .iter()
        .filter(|d| d.is_error())
        .map(|d| d.code)
        .collect()
}

fn errors(body: &str) -> String {
    let c = compile(body);
    assert!(c.has_errors(), "expected an error");
    c.render_diagnostics(false)
}

#[test]
fn compiles_reference_example_from_disk() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    let c = compile_files(&[root.join("order_processing.orbit")]);
    assert!(!c.has_errors(), "{}", c.render_diagnostics(false));
    let wf = &c.workflows[0];
    assert_eq!(wf.name, "OrderProcessing");
    assert_eq!(wf.steps.len(), 5);
    // `process_payment` is conditional, so its output is optional.
    let pay = wf.step("process_payment").unwrap();
    assert!(pay.output_type.is_optional());
    assert!(pay.compensation.is_some());
    // Implicit dependency from `when analyze_risk.score < 0.8`.
    assert_eq!(pay.dependencies, vec!["analyze_risk", "reserve_inventory"]);
}

#[test]
fn every_example_compiles() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|e| e == "orbit") {
            let c = compile_files(std::slice::from_ref(&path));
            assert!(
                !c.has_errors(),
                "{}:\n{}",
                path.display(),
                c.render_diagnostics(false)
            );
        }
    }
}

#[test]
fn deterministic_ir() {
    let src = r#"workflow W {
        version "1.0.0"
        input { id: String, amount: Float }
        step b { worker "risk.analyze" with { amount: input.amount } }
        step a { worker "orders.validate" with { order_id: input.id } }
        step c { worker "payments.charge" after a, b with { order_id: input.id, amount: b.score } }
        return { tx: c.transaction_id }
    }"#;
    let one = serde_json::to_string(&ok(src)).unwrap();
    let two = serde_json::to_string(&ok(src)).unwrap();
    assert_eq!(one, two);
    let wf = ok(src);
    assert!(wf.definition_hash.starts_with("sha256:"));
    assert_eq!(wf.definition_hash, wf.compute_hash());
    // Reformatting the source changes spans but not the definition hash.
    let reformatted = ok(&src.replace("        ", "  ").replace("\n", "\n\n"));
    assert_eq!(reformatted.definition_hash, wf.definition_hash);
    let ids: Vec<_> = wf.steps.iter().map(|s| s.id.as_str()).collect();
    assert_eq!(ids, vec!["b", "a", "c"]);
    // Default from the schema is carried in the IR.
    let charge = &wf.contracts["payments.charge"];
    assert_eq!(
        charge.input.field("currency").unwrap().default,
        Some(serde_json::json!("USD"))
    );
}

#[test]
fn unknown_worker_is_reported_with_suggestion() {
    let msg = errors(
        r#"workflow W { version "1.0.0" step a { worker "orders.validat" with { order_id: "x" } } }"#,
    );
    assert!(
        msg.contains("error[E0300]: unknown worker task `orders.validat`"),
        "{msg}"
    );
    assert!(msg.contains("did you mean `orders.validate`?"), "{msg}");
    assert!(
        msg.contains("test.orbit:"),
        "diagnostics carry locations: {msg}"
    );
}

#[test]
fn unknown_dependency() {
    assert_eq!(
        codes(
            r#"workflow W { version "1.0.0" step a { worker "risk.analyze" after nope with { amount: 1 } } }"#
        ),
        vec!["E0303"]
    );
}

#[test]
fn circular_dependencies_are_rejected() {
    let src = r#"workflow W {
        version "1.0.0"
        step a { worker "risk.analyze" after c with { amount: 1.0 } }
        step b { worker "risk.analyze" with { amount: a.score } }
        step c { worker "risk.analyze" with { amount: b.score } }
    }"#;
    let msg = errors(src);
    assert!(msg.contains("E0304"), "{msg}");
    assert!(msg.contains("cycle: a -> c -> b -> a"), "{msg}");
}

#[test]
fn self_dependency() {
    assert_eq!(
        codes(
            r#"workflow W { version "1.0.0" step a { worker "risk.analyze" with { amount: a.score } } }"#
        ),
        vec!["E0305"]
    );
}

#[test]
fn invalid_types_are_rejected() {
    // Wrong field type.
    let msg = errors(
        r#"workflow W { version "1.0.0" step a { worker "risk.analyze" with { amount: "ten" } } }"#,
    );
    assert!(
        msg.contains("E0400") && msg.contains("expected `Float`, found `String`"),
        "{msg}"
    );
    // Missing required field.
    assert_eq!(
        codes(
            r#"workflow W { version "1.0.0" step a { worker "payments.charge" with { order_id: "x" } } }"#
        ),
        vec!["E0403"]
    );
    // Unknown field with suggestion.
    let msg = errors(
        r#"workflow W { version "1.0.0" step a { worker "risk.analyze" with { amout: 1.0 } } }"#,
    );
    assert!(
        msg.contains("E0404") && msg.contains("did you mean `amount`?"),
        "{msg}"
    );
    // Unknown type.
    let msg = errors(r#"workflow W { version "1.0.0" input { x: Strng } }"#);
    assert!(
        msg.contains("E0201") && msg.contains("did you mean `String`?"),
        "{msg}"
    );
    // Bad field reference on a step output.
    let msg = errors(
        r#"workflow W { version "1.0.0" step a { worker "risk.analyze" with { amount: 1.0 } }
           step b { worker "risk.analyze" with { amount: a.scor } } }"#,
    );
    assert!(
        msg.contains("E0402") && msg.contains("did you mean `score`?"),
        "{msg}"
    );
}

#[test]
fn optional_values_need_handling() {
    // `a` is conditional, so `a.score` is `Float?` and cannot feed a `Float` input.
    let src = r#"workflow W { version "1.0.0" input { go: Bool }
        step a { worker "risk.analyze" when input.go with { amount: 1.0 } }
        step b { worker "risk.analyze" with { amount: a.score } } }"#;
    let msg = errors(src);
    assert!(msg.contains("E0405") && msg.contains("coalesce"), "{msg}");
    // ... and coalesce fixes it.
    ok(&src.replace("amount: a.score", "amount: coalesce(a.score, 0.0)"));
}

#[test]
fn int_widens_to_float_and_defaults_apply() {
    let wf = ok(
        r#"workflow W { version "1.0.0" step a { worker "payments.charge" with { order_id: "x", amount: 10 } } }"#,
    );
    assert_eq!(wf.steps[0].id, "a");
}

#[test]
fn conditions_must_be_bool() {
    assert_eq!(
        codes(
            r#"workflow W { version "1.0.0" step a { worker "risk.analyze" when 1 + 1 with { amount: 1.0 } } }"#
        ),
        vec!["E0408"]
    );
}

#[test]
fn operators_are_type_checked() {
    let msg =
        errors(r#"workflow W { version "1.0.0" input { n: Int } return { x: "a" + input.n } }"#);
    assert!(
        msg.contains("E0406") && msg.contains("string(value)"),
        "{msg}"
    );
}

#[test]
fn return_must_match_output() {
    assert_eq!(
        codes(r#"workflow W { version "1.0.0" output { total: Int } return { total: "x" } }"#),
        vec!["E0400"]
    );
    assert_eq!(
        codes(r#"workflow W { version "1.0.0" output { total: Int } }"#),
        vec!["E0501"]
    );
}

#[test]
fn compensation_scope() {
    let ok_src = r#"workflow W { version "1.0.0"
        step pay { worker "payments.charge" with { order_id: "o", amount: 1.0 }
            compensate { worker "payments.refund" with { transaction_id: pay.transaction_id } } } }"#;
    let wf = ok(ok_src);
    let comp = wf.steps[0].compensation.as_ref().unwrap();
    assert_eq!(comp.task, "payments.refund");
    // A compensation cannot read a step that is not an ancestor.
    let bad = r#"workflow W { version "1.0.0"
        step pay { worker "payments.charge" with { order_id: "o", amount: 1.0 }
            compensate { worker "payments.refund" with { transaction_id: later.transaction_id } } }
        step later { worker "payments.charge" after pay with { order_id: "o", amount: 1.0 } } }"#;
    assert_eq!(codes(bad), vec!["E0308"]);
}

#[test]
fn retry_policy_validation() {
    let wf = ok(
        r#"workflow W { version "1.0.0" step a { worker "risk.analyze" with { amount: 1.0 }
        retry { attempts: 4, backoff: exponential, initial_delay: 200ms, max_delay: 1s, non_retryable: ["BAD"] } } }"#,
    );
    let Action::Task(inv) = &wf.steps[0].action else {
        panic!()
    };
    assert_eq!(inv.retry.max_attempts, 4);
    assert_eq!(inv.retry.delay_before(2), 200);
    assert_eq!(inv.retry.delay_before(3), 400);
    assert_eq!(inv.retry.delay_before(5), 1000);
    let msg = errors(
        r#"workflow W { version "1.0.0" step a { worker "risk.analyze" with { amount: 1.0 } retry { atempts: 3 } } }"#,
    );
    assert!(msg.contains("did you mean `attempts`?"), "{msg}");
    assert_eq!(
        codes(
            r#"workflow W { version "1.0.0" step a { worker "risk.analyze" with { amount: 1.0 } retry { attempts: 0 } } }"#
        ),
        vec!["E0410"]
    );
}

#[test]
fn sleep_steps() {
    let wf = ok(
        r#"workflow W { version "1.0.0" step wait { sleep 2s } step a { worker "risk.analyze" after wait with { amount: 1.0 } } }"#,
    );
    assert_eq!(wf.steps[0].action, Action::Sleep { duration_ms: 2000 });
    assert_eq!(
        codes(r#"workflow W { version "1.0.0" step wait { sleep 2s retry { attempts: 2 } } }"#),
        vec!["E0307"]
    );
}

#[test]
fn missing_version_and_duplicates() {
    assert_eq!(codes(r#"workflow W { }"#), vec!["E0500"]);
    assert_eq!(
        codes(r#"workflow W { version "1.0.0" step a { sleep 1s } step a { sleep 1s } }"#),
        vec!["E0302"]
    );
    assert_eq!(
        codes(r#"workflow W { version "1.0.0" step input { sleep 1s } }"#),
        vec!["E0306"]
    );
}

#[test]
fn recursive_schemas_are_rejected() {
    assert!(codes("schema Node { next: Node? }").contains(&"E0202"));
}

#[test]
fn ir_roundtrip_and_validation() {
    let wf = ok(r#"workflow W { version "1.0.0" input { amount: Float }
        step a { worker "risk.analyze" with { amount: input.amount } }
        return { score: a.score } }"#);
    let text = serde_json::to_string_pretty(&wf).unwrap();
    let back = WorkflowIr::from_json(&text).unwrap();
    assert_eq!(back, wf);
    // Tampering is detected.
    let tampered = text.replace("risk.analyze", "risk.analyse");
    assert!(WorkflowIr::from_json(&tampered).is_err());
    // Unsupported IR versions are rejected explicitly.
    let future = text.replace("orbit-ir/1", "orbit-ir/9");
    let err = WorkflowIr::from_json(&future).unwrap_err();
    assert!(err.0.contains("unsupported IR version"), "{err}");
    assert!(matches!(wf.output, Type::Record { .. }));
}

#[test]
fn http_adapter_is_in_prelude() {
    ok(
        r#"workflow W { version "1.0.0" step get { worker "orbit.http.request" with { url: "https://example.com" } }
        return { status: get.status } }"#,
    );
    assert_eq!(codes(r#"task "orbit.mine" { }"#), vec!["E0200"]);
}

#[test]
fn many_errors_in_one_run() {
    let c = compile(
        r#"workflow W { version "1.0.0"
        step a { worker "nope" }
        step b { worker "risk.analyze" with { amount: true } }
        step c { worker "risk.analyze" after zzz with { amount: 1.0 } } }"#,
    );
    let codes: Vec<_> = c.diagnostics.iter().map(|d| d.code).collect();
    assert_eq!(codes, vec!["E0300", "E0400", "E0303"]);
    assert!(c.workflows.is_empty());
}

#[test]
fn for_each_steps_type_check() {
    let wf = ok(
        r#"workflow W { version "1.0.0" input { amounts: List<Float> }
        step scores { for a in input.amounts  max_parallel 3  worker "risk.analyze" with { amount: a } }
        return { first: scores[0].score, all: scores } }"#,
    );
    let s = wf.step("scores").unwrap();
    let fe = s.for_each.as_ref().unwrap();
    assert_eq!((fe.variable.as_str(), fe.max_parallel), ("a", Some(3)));
    assert!(matches!(&s.output_type, Type::List { .. }));
    // Element type flows into the loop variable.
    let msg = errors(
        r#"workflow W { version "1.0.0" input { names: List<String> }
        step s { for n in input.names worker "risk.analyze" with { amount: n } } }"#,
    );
    assert!(
        msg.contains("E0400") && msg.contains("expected `Float`, found `String`"),
        "{msg}"
    );
    // Only lists can be iterated.
    assert_eq!(
        codes(
            r#"workflow W { version "1.0.0" input { x: Int } step s { for v in input.x worker "risk.analyze" with { amount: 1.0 } } }"#
        ),
        vec!["E0414"]
    );
    // Loop variables may not shadow steps or built-ins.
    assert_eq!(
        codes(
            r#"workflow W { version "1.0.0" input { xs: List<Float> } step input2 { sleep 1s } step s { for input in input.xs worker "risk.analyze" with { amount: 1.0 } } }"#
        ),
        vec!["E0413"]
    );
    // max_parallel needs a for.
    assert_eq!(
        codes(
            r#"workflow W { version "1.0.0" step s { worker "risk.analyze" with { amount: 1.0 } max_parallel 2 } }"#
        ),
        vec!["E0307"]
    );
    // A list comes from a previous step: implicit dependency.
    let wf = ok(r#"workflow W { version "1.0.0"
        step a { worker "orders.validate" with { order_id: "x" } }
        step b { for r in [a.valid, true] worker "risk.analyze" with { amount: 1.0 } } }"#);
    assert_eq!(wf.step("b").unwrap().dependencies, vec!["a"]);
}

#[test]
fn child_workflows_are_typed_and_ordered() {
    let src = r#"
        workflow Parent { version "1.0.0" input { id: String }
            step c { workflow "Child" with { amount: 2.5 } timeout 1h }
            return { score: c.score } }
        workflow Child { version "2.0.0" input { amount: Float } output { score: Float }
            step r { worker "risk.analyze" with { amount: input.amount } }
            return { score: r.score } }"#;
    let c = compile(src);
    assert!(!c.has_errors(), "{}", c.render_diagnostics(false));
    let names: Vec<_> = c.workflows.iter().map(|w| w.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["Child", "Parent"],
        "children are emitted before their callers"
    );
    let parent = &c.workflows[1];
    let contract = &parent.contracts["workflow:Child"];
    assert_eq!(contract.version, "2.0.0");
    assert_eq!(
        contract.workflow_hash.as_deref(),
        Some(c.workflows[0].definition_hash.as_str())
    );
    assert!(matches!(
        parent.step("c").unwrap().action,
        Action::Workflow(_)
    ));
    // Input is checked against the child's input type.
    let msg = errors(&src.replace("amount: 2.5", "amount: \"x\""));
    assert!(msg.contains("E0400"), "{msg}");
    // Unknown child with suggestion; retries are not allowed on child steps.
    let msg = errors(&src.replace("workflow \"Child\"", "workflow \"Chlid\""));
    assert!(
        msg.contains("E0312") && msg.contains("did you mean `Child`?"),
        "{msg}"
    );
    assert!(codes(&src.replace("timeout 1h", "retry { attempts: 2 }")).contains(&"E0307"));
}

#[test]
fn workflow_cycles_are_rejected() {
    let msg = errors(
        r#"workflow A { version "1.0.0" step b { workflow "B" } }
           workflow B { version "1.0.0" step a { workflow "A" } }"#,
    );
    assert!(
        msg.contains("E0311") && msg.contains("cycle: A -> B -> A"),
        "{msg}"
    );
    assert!(errors(r#"workflow A { version "1.0.0" step me { workflow "A" } }"#).contains("E0311"));
}

#[test]
fn formatting_never_changes_meaning() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_none_or(|e| e != "orbit") {
            continue;
        }
        let src = std::fs::read_to_string(&path).unwrap();
        // Make the source deliberately messy, then format it.
        let messy: String = src
            .lines()
            .map(|l| format!("  {}   \n\n", l.trim()))
            .collect();
        let formatted = orbit_syntax::format::format_source(&messy).unwrap();
        assert_eq!(
            orbit_syntax::format::format_source(&formatted).unwrap(),
            formatted,
            "idempotent: {}",
            path.display()
        );
        let hashes = |text: &str| {
            let mut files = std::collections::HashMap::new();
            files.insert(path.clone(), text.to_string());
            for e in std::fs::read_dir(&dir).unwrap() {
                let p = e.unwrap().path();
                files
                    .entry(p.clone())
                    .or_insert_with(|| std::fs::read_to_string(&p).unwrap_or_default());
            }
            let c = orbit_compiler::compile_with(
                std::slice::from_ref(&path),
                &orbit_compiler::MemLoader(files),
            );
            assert!(
                !c.has_errors(),
                "{}: {}",
                path.display(),
                c.render_diagnostics(false)
            );
            c.workflows
                .iter()
                .map(|w| w.definition_hash.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(
            hashes(&src),
            hashes(&formatted),
            "formatting changed the meaning of {}",
            path.display()
        );
    }
}

#[test]
fn loop_variable_is_not_in_scope_in_when() {
    let msg = errors(
        r#"workflow W { version "1.0.0" input { xs: List<Float> }
        step s { for x in input.xs when x > 1.0 worker "risk.analyze" with { amount: x } } }"#,
    );
    assert!(
        msg.contains("E0401") && msg.contains("loop variable `x` is not in scope in `when`"),
        "{msg}"
    );
    // A condition that does not use the loop variable is fine.
    ok(
        r#"workflow W { version "1.0.0" input { xs: List<Float>, go: Bool }
        step s { for x in input.xs when input.go worker "risk.analyze" with { amount: x } } }"#,
    );
}
