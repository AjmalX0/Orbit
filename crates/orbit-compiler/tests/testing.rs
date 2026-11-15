//! `test` declarations: parsing, type checking (E07xx) and lowering.

use orbit_compiler::{compile_str, Compilation};
use orbit_ir::testing::{Expectation, MockOutcome};
use serde_json::json;

const SRC: &str = r#"
task "charge" { version "1.0.0" input { amount: Float } output { id: String, ok: Bool } }
task "refund" { version "1.0.0" input { id: String } output { ok: Bool } }
task "ship" { version "1.0.0" input { n: Int } output { tracking: String } }

workflow Shipping {
    version "1.0.0"
    input { n: Int }
    step book { worker "ship" with { n: input.n } }
    return { tracking: book.tracking }
}

workflow Order {
    version "1.0.0"
    input { amount: Float, note: String = "none" }
    output { id: String, tracking: String }
    signal go { ok: Bool, by: String? }
    step wait { await go timeout 1h }
    step charge {
        worker "charge"
        after wait
        with { amount: input.amount }
        compensate { worker "refund" with { id: charge.id } }
    }
    step ship { workflow "Shipping" with { n: 1 } }
    return { id: charge.id, tracking: ship.tracking }
}
"#;

fn compile(tests: &str) -> Compilation {
    compile_str("t.orbit", &format!("{SRC}\n{tests}"))
}

fn codes(tests: &str) -> Vec<&'static str> {
    compile(tests).diagnostics.iter().map(|d| d.code).collect()
}

#[test]
fn a_valid_test_is_lowered() {
    let c = compile(
        r#"
test "orders ship" {
    workflow Order
    version "1.0.0"
    input { amount: 10.0 }
    mock "charge" fails "BUSY" "try later" retryable times 2 then returns { id: "tx-" + string(input.amount), ok: true }
    mock "ship" returns { tracking: "T" }
    mock "refund" hangs
    signal go { ok: true }
    expect status COMPLETED
    expect output { id: "tx-10.0" }
    expect step charge SUCCEEDED
    expect step charge attempts 3
    expect step wait output { ok: true }
    expect error "NONE"
    expect compensated charge
}
"#,
    );
    assert!(!c.has_errors(), "{}", c.render_diagnostics(false));
    // Tests never become workflows.
    assert_eq!(c.workflows.len(), 2);
    let t = &c.tests[0];
    assert_eq!(t.name, "orders ship");
    assert_eq!(t.workflow, "Order");
    assert_eq!(t.input, json!({ "amount": 10.0, "note": "none" }));
    assert_eq!(t.signals[0].payload, json!({ "ok": true }));
    let charge = &t.mocks["charge"];
    assert_eq!(charge.len(), 2);
    assert_eq!(charge[0].times, Some(2));
    assert!(
        matches!(&charge[0].outcome, MockOutcome::Fails { code, retryable: true, .. } if code == "BUSY")
    );
    assert!(matches!(t.mocks["refund"][0].outcome, MockOutcome::Hangs));
    // Mocks of a child workflow's tasks are allowed.
    assert!(t.mocks.contains_key("ship"));
    assert_eq!(t.expectations.len(), 7);
    assert!(matches!(
        &t.expectations[3],
        Expectation::StepAttempts { attempts: 3, .. }
    ));
    assert_eq!(t.location.line, 30);
}

#[test]
fn test_diagnostics() {
    assert_eq!(
        codes(r#"test "x" { expect status COMPLETED }"#),
        vec!["E0701"]
    );
    assert_eq!(
        codes(r#"test "x" { workflow Ordr expect status COMPLETED }"#),
        vec!["E0700"]
    );
    assert_eq!(
        codes(
            r#"test "x" { workflow Order version "2.0.0" input { amount: 1.0 } expect status COMPLETED }"#
        ),
        vec!["E0702"]
    );
    let base =
        |body: &str| format!("test \"x\" {{ workflow Order input {{ amount: 1.0 }} {body} }}");
    assert_eq!(
        codes(&base(r#"mock "nope" returns {} expect status FAILED"#)),
        vec!["E0703"]
    );
    assert_eq!(
        codes(&base(
            r#"mock "ship" returns { tracking: "a" } mock "ship" hangs expect status FAILED"#
        )),
        vec!["E0704"]
    );
    assert_eq!(
        codes(&base("signal stop {} expect status FAILED")),
        vec!["E0705"]
    );
    assert_eq!(codes(&base("expect step chrge SUCCEEDED")), vec!["E0706"]);
    assert_eq!(codes(&base("expect status DONE")), vec!["E0707"]);
    assert_eq!(codes(&base("expect step charge succeeded")), vec!["E0707"]);
    assert_eq!(codes(&base("expect compensated wait")), vec!["E0708"]);
    assert_eq!(
        codes(
            r#"test "x" { workflow Order input { amount: 1.0 } expect status FAILED } test "x" { workflow Order input { amount: 1.0 } expect status FAILED }"#
        ),
        vec!["E0709"]
    );
    assert_eq!(
        codes(&base(
            r#"mock "charge" fails "X" times 0 then hangs expect status FAILED"#
        )),
        vec!["E0711"]
    );
    assert_eq!(codes(&base("")), vec!["W0701"]);
}

#[test]
fn test_values_are_type_checked() {
    // Missing required input, wrong signal payload type, mock output of the
    // wrong type, `input` in a mock typed by the contract input.
    assert_eq!(
        codes(r#"test "x" { workflow Order expect status FAILED }"#),
        vec!["E0403"]
    );
    let base = |body: &str| {
        format!(
            "test \"x\" {{ workflow Order input {{ amount: 1.0 }} {body} expect status FAILED }}"
        )
    };
    assert_eq!(codes(&base("signal go { ok: \"yes\" }")), vec!["E0400"]);
    assert_eq!(codes(&base("signal go {}")), vec!["E0403"]);
    assert_eq!(
        codes(&base(
            r#"mock "charge" returns { id: input.amount, ok: true }"#
        )),
        vec!["E0400"]
    );
    assert_eq!(
        codes(&base(
            r#"mock "charge" returns { id: input.nope, ok: true }"#
        )),
        vec!["E0402"]
    );
    assert_eq!(
        codes(&base(r#"mock "charge" returns { id: "a" }"#)),
        vec!["E0403"]
    );
    // Expected outputs may list a subset of fields, but of the right types.
    assert_eq!(codes(&base("expect output { id: 1 }")), vec!["E0400"]);
    assert_eq!(codes(&base("expect output { idd: \"a\" }")), vec!["E0404"]);
    // Test values are constants.
    assert_eq!(
        codes(r#"test "x" { workflow Order input { amount: input.amount } expect status FAILED }"#),
        vec!["E0401"]
    );
}

#[test]
fn test_syntax_errors_are_located() {
    let c = compile(
        r#"test "x" { workflow Order mock "charge" fails "X" times 2 expect status FAILED }"#,
    );
    let d = c.diagnostics.iter().find(|d| d.code == "E0100").unwrap();
    assert!(d.message.contains("expected `then`"), "{}", d.message);
    let c = compile(r#"test "x" { workflow Order expect stauts FAILED }"#);
    let d = c.diagnostics.iter().find(|d| d.code == "E0100").unwrap();
    assert_eq!(d.help.as_deref(), Some("did you mean `status`?"));
    assert!(codes(r#"test "x" { workflw Order }"#).contains(&"E0102"));
}
