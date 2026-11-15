//! `continue_as_new [when <cond>] with <input>`: parsing, type checking
//! (E0323, E0324, W0607, E0712, E0713), lowering and the IR version.

use orbit_compiler::{compile_str, Compilation};
use orbit_ir::testing::Expectation;
use orbit_ir::{WorkflowIr, IR_VERSION, IR_VERSION_2};

const CONTRACTS: &str = r#"
task "page" { version "1.0.0" input { cursor: Int } output { next: Int, done: Bool } }
"#;

fn compile(body: &str) -> Compilation {
    compile_str("can.orbit", &format!("{CONTRACTS}\n{body}"))
}

fn ok(body: &str) -> WorkflowIr {
    let c = compile(body);
    assert!(!c.has_errors(), "{}", c.render_diagnostics(false));
    c.workflows.into_iter().next().unwrap()
}

fn codes(body: &str) -> Vec<&'static str> {
    compile(body).diagnostics.iter().map(|d| d.code).collect()
}

const POLLER: &str = r#"
workflow Poller {
    version "1.0.0"
    input { cursor: Int = 0, pages: Int = 0 }
    step fetch { worker "page" with { cursor: input.cursor } }
    continue_as_new when !fetch.done with { cursor: fetch.next, pages: input.pages + 1 }
    return { pages: input.pages }
}
"#;

#[test]
fn continue_as_new_lowers_to_ir_v2() {
    let ir = ok(POLLER);
    assert_eq!(ir.ir_version, IR_VERSION_2);
    let can = ir.continue_as_new.as_ref().expect("continue_as_new");
    assert!(can.condition.is_some());
    ir.validate().unwrap();
    // Either order of `when` and `with`; `when` is optional.
    let ir2 = ok(r#"
workflow Poller {
    version "1.0.0"
    input { cursor: Int = 0, pages: Int = 0 }
    step fetch { worker "page" with { cursor: input.cursor } }
    continue_as_new with { cursor: fetch.next, pages: input.pages + 1 }
        when !fetch.done
    return { pages: input.pages }
}
"#);
    assert_eq!(ir2.definition_hash, ir.definition_hash);
    let always = ok(r#"
workflow Forever {
    version "1.0.0"
    input { cursor: Int = 0 }
    step fetch { worker "page" with { cursor: input.cursor } }
    continue_as_new with { cursor: fetch.next }
}
"#);
    assert!(always.continue_as_new.unwrap().condition.is_none());
}

#[test]
fn workflows_without_it_keep_ir_v1_and_their_hash() {
    let ir = ok(r#"
workflow Plain {
    version "1.0.0"
    input { cursor: Int }
    step fetch { worker "page" with { cursor: input.cursor } }
}
"#);
    assert_eq!(ir.ir_version, IR_VERSION);
    let json = serde_json::to_value(&ir).unwrap();
    assert!(json.get("continue_as_new").is_none());
}

#[test]
fn the_next_input_is_checked_against_the_workflow_input() {
    // A field of the wrong type.
    let c = compile(
        r#"
workflow W {
    version "1.0.0"
    input { cursor: Int }
    step fetch { worker "page" with { cursor: input.cursor } }
    continue_as_new with { cursor: "next" }
}
"#,
    );
    assert!(c.has_errors());
    let text = c.render_diagnostics(false);
    assert!(text.contains("cursor"), "{text}");
    // A missing required field.
    let c = compile(
        r#"
workflow W {
    version "1.0.0"
    input { cursor: Int, limit: Int }
    step fetch { worker "page" with { cursor: input.cursor } }
    continue_as_new with { cursor: fetch.next }
}
"#,
    );
    assert!(c.has_errors(), "{}", c.render_diagnostics(false));
    // Fields with defaults may be left out.
    ok(r#"
workflow W {
    version "1.0.0"
    input { cursor: Int, limit: Int = 10 }
    step fetch { worker "page" with { cursor: input.cursor } }
    continue_as_new with { cursor: fetch.next }
}
"#);
    // Unknown steps are reported like anywhere else.
    assert!(compile(
        r#"
workflow W {
    version "1.0.0"
    input { cursor: Int }
    step fetch { worker "page" with { cursor: input.cursor } }
    continue_as_new with { cursor: fetchh.next }
}
"#
    )
    .has_errors());
}

#[test]
fn diagnostics() {
    // E0323: no `with`.
    assert!(codes(
        r#"
workflow W {
    version "1.0.0"
    input { cursor: Int }
    step fetch { worker "page" with { cursor: input.cursor } }
    continue_as_new when fetch.done
}
"#
    )
    .contains(&"E0323"));
    // E0324: the condition is not Bool.
    assert!(codes(
        r#"
workflow W {
    version "1.0.0"
    input { cursor: Int }
    step fetch { worker "page" with { cursor: input.cursor } }
    continue_as_new when fetch.next with { cursor: fetch.next }
}
"#
    )
    .contains(&"E0324"));
    // E0106: declared twice.
    assert!(codes(
        r#"
workflow W {
    version "1.0.0"
    input { cursor: Int }
    step fetch { worker "page" with { cursor: input.cursor } }
    continue_as_new with { cursor: 1 }
    continue_as_new with { cursor: 2 }
}
"#
    )
    .contains(&"E0106"));
    // W0607: `return` can never be used.
    let c = compile(
        r#"
workflow W {
    version "1.0.0"
    input { cursor: Int }
    step fetch { worker "page" with { cursor: input.cursor } }
    continue_as_new with { cursor: fetch.next }
    return { done: true }
}
"#,
    );
    assert!(!c.has_errors());
    assert_eq!(
        c.diagnostics.iter().map(|d| d.code).collect::<Vec<_>>(),
        ["W0607"]
    );
}

#[test]
fn expect_runs() {
    let c = compile(&format!(
        r#"{POLLER}
test "pages" {{
    workflow Poller
    input {{}}
    mock "page" returns {{ next: input.cursor + 1, done: input.cursor >= 2 }}
    expect runs 3
    expect output {{ pages: 2 }}
}}
"#
    ));
    assert!(!c.has_errors(), "{}", c.render_diagnostics(false));
    assert!(c.tests[0]
        .expectations
        .contains(&Expectation::Runs { runs: 3 }));
    let bad = |expect: &str| {
        let c = compile(&format!(
            r#"{POLLER}
workflow Plain {{
    version "1.0.0"
    input {{ cursor: Int = 0 }}
    step fetch {{ worker "page" with {{ cursor: input.cursor }} }}
}}
test "t" {{
    workflow Plain
    input {{}}
    mock "page" returns {{ next: 1, done: true }}
    {expect}
}}
"#
        ));
        c.diagnostics.iter().map(|d| d.code).collect::<Vec<_>>()
    };
    assert_eq!(bad("expect runs 0"), ["E0712"]);
    assert_eq!(bad("expect runs 2"), ["E0713"]);
    assert!(bad("expect runs 1").is_empty());
}
