//! `orbit compat` rules: workflow versions, input/output evolution and task
//! contract evolution.

use orbit_compiler::compat::{compare, Finding, Report, Severity, Snapshot, Subject};
use orbit_compiler::compile_str;
use orbit_ir::WorkflowIr;

fn snapshot(src: &str) -> Snapshot {
    let c = compile_str("test.orbit", src);
    assert!(!c.has_errors(), "{}", c.render_diagnostics(false));
    Snapshot::from_compilation(&c)
}

fn diff(old: &str, new: &str) -> Report {
    compare(&snapshot(old), &snapshot(new))
}

/// A workflow with the given version, input fields and `return` body.
fn wf(version: &str, input: &str, ret: &str) -> String {
    format!(
        "workflow W {{\n version \"{version}\"\n input {{ {input} }}\n step pause {{ sleep 1s }}\n return {ret}\n}}\n"
    )
}

fn with(r: &Report, sev: Severity) -> Vec<&Finding> {
    r.findings.iter().filter(|f| f.severity == sev).collect()
}

fn at<'a>(r: &'a Report, path: &str) -> &'a Finding {
    r.findings
        .iter()
        .find(|f| f.path.as_deref() == Some(path))
        .unwrap_or_else(|| panic!("no finding at {path}:\n{}", r.render(false)))
}

#[test]
fn unchanged_workflow_is_ok() {
    let src = wf("1.0.0", "a: String", "{ a: input.a }");
    let r = diff(&src, &src);
    assert_eq!(r.errors(), 0);
    assert_eq!(r.findings.len(), 1);
    assert_eq!(r.findings[0].severity, Severity::Ok);
    assert_eq!(r.findings[0].message, "unchanged");
}

#[test]
fn content_change_without_version_change_is_an_error() {
    let old = wf("1.0.0", "a: String", "{ a: input.a }");
    let new = wf("1.0.0", "a: String, b: Int?", "{ a: input.a }");
    let r = diff(&old, &new);
    let e = with(&r, Severity::Error);
    assert_eq!(e.len(), 1, "{}", r.render(false));
    assert!(e[0]
        .message
        .contains("content changed without a version change"));
    assert!(e[0].message.contains("DEFINITION_CONFLICT"));
    // An additive change suggests a minor bump.
    assert_eq!(
        e[0].hint.as_deref(),
        Some("bump the version (1.0.0 → 1.1.0)")
    );

    // A change that only touches steps suggests a patch bump, a breaking one a major bump.
    let steps = old.replace("sleep 1s", "sleep 2s");
    let r = diff(&old, &steps);
    assert_eq!(
        with(&r, Severity::Error)[0].hint.as_deref(),
        Some("bump the version (1.0.0 → 1.0.1)")
    );
    let breaking = wf("1.0.0", "a: Int", "{ a: input.a }");
    let r = diff(&old, &breaking);
    assert_eq!(
        with(&r, Severity::Error)[0].hint.as_deref(),
        Some("bump the version (1.0.0 → 2.0.0)")
    );
}

#[test]
fn breaking_input_change_needs_a_major_bump() {
    let old = wf("1.4.0", "amount: Float", "{ ok: true }");
    let minor = wf("1.5.0", "amount: Int", "{ ok: true }");
    let r = diff(&old, &minor);
    assert_eq!(r.errors(), 1, "{}", r.render(false));
    let f = at(&r, "input.amount");
    assert_eq!(f.severity, Severity::Error);
    assert_eq!(f.old_type.as_deref(), Some("Float"));
    assert_eq!(f.new_type.as_deref(), Some("Int"));
    assert!(f.message.contains("breaking for callers"));
    assert_eq!(
        f.hint.as_deref(),
        Some("bump the major version (1.4.0 → 2.0.0)")
    );
    assert!(r.render(false).starts_with(
        "error   workflow W 1.4.0 → 1.5.0: input.amount: Float → Int: incompatible type"
    ));

    let major = wf("2.0.0", "amount: Int", "{ ok: true }");
    let r = diff(&old, &major);
    assert_eq!(r.errors(), 0, "{}", r.render(false));
    assert_eq!(at(&r, "input.amount").severity, Severity::Info);
}

#[test]
fn every_breaking_input_change_is_detected() {
    let old = wf(
        "1.0.0",
        "a: String, b: Int = 1, c: String?, d: String",
        "{}",
    );
    let new = wf("1.1.0", "a: Int, b: Int, c: String, e: String", "{}");
    let r = diff(&old, &new);
    for path in ["input.a", "input.b", "input.c", "input.d", "input.e"] {
        assert_eq!(at(&r, path).severity, Severity::Error, "{path}");
    }
    assert!(at(&r, "input.b").message.contains("default removed"));
    assert!(at(&r, "input.d").message.contains("field removed"));
    assert!(at(&r, "input.e")
        .message
        .contains("required field added without a default"));
}

#[test]
fn compatible_additive_change_is_ok() {
    let old = wf("1.0.0", "a: Int, b: String", "{ x: input.b }");
    let new = wf(
        "1.1.0",
        "a: Float, b: String?, c: String?, d: Int = 3",
        "{ x: \"s\", y: input.d }",
    );
    let r = diff(&old, &new);
    assert_eq!(r.errors(), 0, "{}", r.render(false));
    assert_eq!(r.warnings(), 0, "{}", r.render(false));
    for path in ["input.a", "input.b", "input.c", "input.d", "output.y"] {
        assert_eq!(at(&r, path).severity, Severity::Ok, "{path}");
    }
    assert!(at(&r, "input.a").message.starts_with("widened"));
    assert!(at(&r, "input.b").message.starts_with("now optional"));
}

#[test]
fn output_narrowing_is_breaking() {
    let old = wf("1.0.0", "a: String", "{ x: input.a, y: 1, z: 2.5 }");
    let new = wf(
        "1.1.0",
        "a: String",
        "{ x: if true then input.a else null, z: 1 }",
    );
    let r = diff(&old, &new);
    let x = at(&r, "output.x");
    assert_eq!(x.severity, Severity::Error, "{}", r.render(false));
    assert_eq!(x.old_type.as_deref(), Some("String"));
    assert_eq!(x.new_type.as_deref(), Some("String?"));
    assert!(x.message.contains("consumers of results"));
    assert_eq!(at(&r, "output.y").severity, Severity::Error);
    assert!(at(&r, "output.y").message.contains("field removed"));
    // Float → Int in an output still satisfies consumers.
    assert_eq!(at(&r, "output.z").severity, Severity::Ok);
}

#[test]
fn nested_records_lists_and_json() {
    let old = wf(
        "1.0.0",
        "customer: { email: String }, tags: List<String>, meta: JSON, extra: String",
        "{}",
    );
    let new = wf(
        "1.1.0",
        "customer: { email: String, phone: String }, tags: List<Int>, meta: { k: String }, extra: JSON",
        "{}",
    );
    let r = diff(&old, &new);
    assert_eq!(at(&r, "input.customer.phone").severity, Severity::Error);
    assert_eq!(at(&r, "input.tags[]").severity, Severity::Error);
    assert_eq!(at(&r, "input.meta").severity, Severity::Error);
    assert_eq!(at(&r, "input.extra").severity, Severity::Ok);
}

#[test]
fn zero_major_versions_treat_minor_as_major() {
    let old = wf("0.3.1", "a: String", "{}");
    let patch = wf("0.3.2", "a: Int", "{}");
    let r = diff(&old, &patch);
    let f = at(&r, "input.a");
    assert_eq!(f.severity, Severity::Error);
    assert_eq!(
        f.hint.as_deref(),
        Some("bump the minor version (0.3.1 → 0.4.0); in 0.x versions the minor version is the major version")
    );
    let minor = wf("0.4.0", "a: Int", "{}");
    assert_eq!(diff(&old, &minor).errors(), 0);
    let pre = wf("1.0.0-rc.1", "a: Int", "{}");
    assert_eq!(diff(&old, &pre).errors(), 0);
}

#[test]
fn version_without_semver_cannot_be_checked() {
    let old = wf("v1", "a: String", "{}");
    let new = wf("v2", "a: Int", "{}");
    let c = compile_str("t.orbit", &old);
    let o = Snapshot::from_compilation(&c);
    let n = Snapshot::from_compilation(&compile_str("t.orbit", &new));
    let r = compare(&o, &n);
    assert_eq!(r.errors(), 0);
    assert_eq!(at(&r, "input.a").severity, Severity::Warning);
}

#[test]
fn removed_and_added_workflows() {
    let old = wf("1.0.0", "a: String", "{}");
    let new = old.replace("workflow W", "workflow V");
    let r = diff(&old, &new);
    assert_eq!(r.errors(), 0);
    let removed = with(&r, Severity::Warning);
    assert_eq!(removed.len(), 1);
    assert_eq!(removed[0].name, "W");
    assert!(removed[0]
        .message
        .contains("running executions keep their definition"));
    let added = with(&r, Severity::Info);
    assert_eq!(added[0].name, "V");
    assert_eq!(added[0].message, "workflow added");
}

#[test]
fn version_bump_without_content_change_is_info() {
    let old = wf("1.0.0", "a: String", "{}");
    let new = wf("1.0.1", "a: String", "{}");
    let r = diff(&old, &new);
    assert_eq!(r.errors(), 0);
    assert_eq!(r.findings[0].severity, Severity::Info);
}

const CALLER: &str = r#"
workflow Pay {
    version "VERSION"
    step charge { worker "payments.charge" with { order_id: "o" } }
}
"#;

fn contract(version: &str, input: &str, output: &str, wf_version: &str) -> String {
    format!(
        "task \"payments.charge\" {{ version \"{version}\" input {{ {input} }} output {{ {output} }} }}\n{}",
        CALLER.replace("VERSION", wf_version)
    )
}

fn contract_findings(r: &Report) -> Vec<&Finding> {
    r.findings
        .iter()
        .filter(|f| f.subject == Subject::Contract)
        .collect()
}

#[test]
fn contract_shape_change_under_the_same_version_is_an_error() {
    let old = contract("1.0.0", "order_id: String", "id: String", "1.0.0");
    let new = contract("1.0.0", "order_id: String", "id: Int", "1.1.0");
    let r = diff(&old, &new);
    let c = contract_findings(&r);
    assert_eq!(c.len(), 1, "{}", r.render(false));
    assert_eq!(c[0].severity, Severity::Error);
    assert_eq!(c[0].path.as_deref(), Some("output.id"));
    assert!(c[0].message.contains("payments.charge@1.0.0"));
    assert_eq!(
        c[0].hint.as_deref(),
        Some("give the changed contract a new version (1.0.0 → 2.0.0)")
    );

    // An optional input field is safe for old and new workers alike.
    let new = contract("1.0.0", "order_id: String, n: Int?", "id: String", "1.1.0");
    let r = diff(&old, &new);
    assert_eq!(r.errors(), 0, "{}", r.render(false));
    // Widening an input to JSON is safe for new workers but not for old ones.
    let widened = contract("1.0.0", "order_id: JSON", "id: String", "1.1.0");
    assert_eq!(diff(&old, &widened).errors(), 1);
}

#[test]
fn compatible_contract_change_can_keep_its_version() {
    let old = contract("1.0.0", "order_id: String", "id: String", "1.0.0");
    let new = contract(
        "1.0.0",
        "order_id: String",
        "id: String, receipt: String?",
        "1.1.0",
    );
    let r = diff(&old, &new);
    assert_eq!(r.errors(), 0, "{}", r.render(false));
    let c = contract_findings(&r);
    assert_eq!(c[0].severity, Severity::Ok);
    assert_eq!(c[0].path.as_deref(), Some("output.receipt"));
}

#[test]
fn new_contract_versions_are_classified() {
    let old = contract("1.0.0", "order_id: String", "id: String", "1.0.0");
    // An old worker does not return `receipt`, so the change is breaking.
    let minor = contract(
        "1.1.0",
        "order_id: String",
        "id: String, receipt: String",
        "1.1.0",
    );
    let r = diff(&old, &minor);
    let f = at(&r, "output.receipt");
    assert_eq!(f.severity, Severity::Warning, "{}", r.render(false));
    assert_eq!(f.old_version.as_deref(), Some("1.0.0"));
    assert_eq!(f.new_version.as_deref(), Some("1.1.0"));
    assert_eq!(
        f.hint.as_deref(),
        Some("bump the major version (1.0.0 → 2.0.0)")
    );
    // The old version is reported as no longer used.
    assert!(r.findings.iter().any(|f| f.subject == Subject::Contract
        && f.severity == Severity::Info
        && f.new_version.is_none()));

    let major = contract(
        "2.0.0",
        "order_id: String",
        "id: String, receipt: String",
        "1.1.0",
    );
    assert_eq!(
        at(&diff(&old, &major), "output.receipt").severity,
        Severity::Info
    );

    // An old worker ignores an input field it does not know: compatible.
    let compatible = contract("1.1.0", "order_id: String, n: Int?", "id: String", "1.1.0");
    let r = diff(&old, &compatible);
    assert_eq!(r.errors() + r.warnings(), 0, "{}", r.render(false));
}

#[test]
fn compiled_ir_json_is_accepted() {
    let to_ir = |src: &str| -> WorkflowIr {
        let c = compile_str("t.orbit", src);
        let text = serde_json::to_string(&c.workflows[0]).unwrap();
        WorkflowIr::from_json(&text).unwrap()
    };
    let old = contract("1.0.0", "order_id: String", "id: String", "1.0.0");
    let new = contract("1.0.0", "order_id: String", "id: Int", "1.0.0");
    let r = compare(
        &Snapshot::from_workflows(vec![to_ir(&old)]),
        &Snapshot::from_workflows(vec![to_ir(&new)]),
    );
    // The workflow changed under the same version, and so did its contract.
    assert_eq!(r.errors(), 2, "{}", r.render(false));
    assert!(r.findings[0]
        .message
        .contains("content changed without a version change"));

    let mut stripped = to_ir(&old);
    stripped.definition_hash.clear();
    let r = compare(
        &Snapshot::from_workflows(vec![to_ir(&old)]),
        &Snapshot::from_workflows(vec![stripped]),
    );
    assert_eq!(r.errors(), 0, "{}", r.render(false));
}

#[test]
fn json_report_format() {
    let old = wf("1.4.0", "amount: Float", "{}");
    let new = wf("1.5.0", "amount: Int", "{}");
    let v = diff(&old, &new).to_json();
    assert_eq!(v["compatible"], false);
    assert_eq!(v["errors"], 1);
    let f = &v["findings"][0];
    assert_eq!(f["severity"], "error");
    assert_eq!(f["kind"], "workflow");
    assert_eq!(f["name"], "W");
    assert_eq!(f["old_version"], "1.4.0");
    assert_eq!(f["new_version"], "1.5.0");
    assert_eq!(f["path"], "input.amount");
    assert_eq!(f["old_type"], "Float");
    assert_eq!(f["new_type"], "Int");
    assert_eq!(f["hint"], "bump the major version (1.4.0 → 2.0.0)");
}
