//! `orbit graph`, `orbit docs`, `orbit schema`, `orbit diff` and `--policy`.

use std::path::PathBuf;
use std::process::{Command, Output};

fn orbit() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_orbit"));
    c.env("NO_COLOR", "1");
    c
}

fn examples() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples")
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("orbit-tooling-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn run(args: &[&str]) -> Output {
    orbit().args(args).current_dir(examples()).output().unwrap()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).to_string()
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).to_string()
}

#[test]
fn graph_formats() {
    let m = run(&["graph", "checkout.orbit"]);
    assert!(m.status.success(), "{}", stderr(&m));
    let text = stdout(&m);
    assert!(text.starts_with("flowchart TD\n    %% Checkout@1.0.0\n"));
    assert!(text.contains("reserve -. compensate .-> reserve__undo"));
    assert!(text.contains("subgraph on_failure[\"on failure\"]"));
    // Deterministic.
    assert_eq!(text, stdout(&run(&["graph", "checkout.orbit"])));

    let d = stdout(&run(&["graph", "checkout.orbit", "--format", "dot"]));
    assert!(d.starts_with("digraph \"Checkout\" {"), "{d}");
    let s = stdout(&run(&["graph", "checkout.orbit", "--format", "svg"]));
    assert!(s.starts_with("<svg xmlns=\"http://www.w3.org/2000/svg\""));
    let q = stdout(&run(&["graph", "approval.orbit", "--sequence"]));
    assert!(q.starts_with("sequenceDiagram\n"), "{q}");
    assert!(
        q.contains("Sender->>W: decision: signal review (timeout 2d)"),
        "{q}"
    );

    let bad = run(&["graph", "checkout.orbit", "--sequence", "--format", "dot"]);
    assert_eq!(bad.status.code(), Some(1));
    assert!(stderr(&bad).contains("--sequence"));

    let dir = tmp("graph");
    let out = dir.join("sub/checkout.mmd");
    let w = run(&["graph", "checkout.orbit", "-o", out.to_str().unwrap()]);
    assert!(w.status.success(), "{}", stderr(&w));
    assert_eq!(std::fs::read_to_string(&out).unwrap(), text);
}

#[test]
fn docs_markdown_and_html() {
    let dir = tmp("docs");
    let md = dir.join("md");
    let o = run(&[
        "docs",
        "checkout.orbit",
        "approval.orbit",
        "--output",
        md.to_str().unwrap(),
    ]);
    assert!(o.status.success(), "{}", stderr(&o));
    for f in [
        "index.md",
        "Checkout.md",
        "OrderApproval.md",
        "contracts.md",
    ] {
        assert!(md.join(f).is_file(), "{f}");
    }
    let page = std::fs::read_to_string(md.join("Checkout.md")).unwrap();
    assert!(page.contains("## Outcome handlers"));
    assert!(page.contains("```mermaid\nflowchart TD"));

    let html = dir.join("html");
    let o = run(&[
        "docs",
        "approval.orbit",
        "--output",
        html.to_str().unwrap(),
        "--format",
        "html",
        "--json-schema",
    ]);
    assert!(o.status.success(), "{}", stderr(&o));
    let page = std::fs::read_to_string(html.join("OrderApproval.html")).unwrap();
    assert!(page.starts_with("<!doctype html>"));
    assert!(page.contains("<svg"));
    assert!(!page.contains("<script") && !page.contains("<link"));
    assert!(html
        .join("schemas/workflow.OrderApproval.signal.review.schema.json")
        .is_file());
}

#[test]
fn schemas_are_valid_draft_2020_12() {
    let dir = tmp("schema");
    let o = run(&[
        "schema",
        "checkout.orbit",
        "approval.orbit",
        "fulfilment.orbit",
        "--output",
        dir.to_str().unwrap(),
    ]);
    assert!(o.status.success(), "{}", stderr(&o));
    let one = stdout(&run(&[
        "schema",
        "approval.orbit",
        "--workflow",
        "OrderApproval",
    ]));
    let all: serde_json::Value = serde_json::from_str(&one).unwrap();
    assert_eq!(
        all["workflow.OrderApproval.input"]["$schema"],
        "https://json-schema.org/draft/2020-12/schema"
    );
    assert!(all.get("task.payments.charge.input").is_some());

    // Validate with the reference Python implementation when it is installed.
    let script = r#"
import json, pathlib, sys
try:
    import jsonschema
except ImportError:
    print("SKIP"); sys.exit(0)
d = pathlib.Path(sys.argv[1])
n = 0
for p in sorted(d.glob("*.schema.json")):
    jsonschema.Draft202012Validator.check_schema(json.loads(p.read_text())); n += 1
inp = json.loads((d / "workflow.Checkout.input.schema.json").read_text())
v = jsonschema.Draft202012Validator(inp)
good = {"order_id": "A1", "customer_id": "c", "amount": 10, "items": [{"sku": "x", "quantity": 2}]}
assert not list(v.iter_errors(good)), list(v.iter_errors(good))
assert list(v.iter_errors({**good, "extra": 1})), "unknown field accepted"
assert list(v.iter_errors({**good, "items": [{"sku": "x", "quantity": 1.5}]})), "float accepted as Int"
del good["amount"]
assert list(v.iter_errors(good)), "missing field accepted"
sig = json.loads((d / "workflow.OrderApproval.signal.review.schema.json").read_text())
v = jsonschema.Draft202012Validator(sig)
assert not list(v.iter_errors({"approved": True, "reviewer": "r", "comment": None, "x": 1}))
assert list(v.iter_errors({"approved": "yes", "reviewer": "r"}))
print("OK", n)
"#;
    if let Ok(py) = Command::new("python3")
        .arg("-c")
        .arg(script)
        .arg(&dir)
        .output()
    {
        let out = stdout(&py);
        assert!(py.status.success(), "{out}{}", stderr(&py));
        assert!(out.starts_with("OK") || out.starts_with("SKIP"), "{out}");
    }
}

#[test]
fn diff_reports_changes_and_exit_codes() {
    let dir = tmp("diff");
    std::fs::copy(
        examples().join("contracts.orbit"),
        dir.join("contracts.orbit"),
    )
    .unwrap();
    let src = std::fs::read_to_string(examples().join("checkout.orbit")).unwrap();
    let v2 = src
        .replace("version \"1.0.0\"", "version \"1.1.0\"")
        .replace("after reserve", "after reserve\n        timeout 30s");
    let new = dir.join("checkout.orbit");
    std::fs::write(&new, v2).unwrap();
    let old = examples().join("checkout.orbit");
    let (o, n) = (old.to_str().unwrap(), new.to_str().unwrap());

    let same = run(&["diff", o, o, "--exit-code"]);
    assert_eq!(same.status.code(), Some(0));
    assert!(stdout(&same).contains("summary: no changes; compatibility: compatible"));

    let d = run(&["diff", o, n]);
    assert_eq!(
        d.status.code(),
        Some(0),
        "without --exit-code changes exit 0"
    );
    let text = stdout(&d);
    assert!(text.contains("  + timeout: charge: timeout 30s"), "{text}");
    assert_eq!(run(&["diff", o, n, "--exit-code"]).status.code(), Some(1));

    let md = stdout(&run(&["diff", o, n, "--format", "markdown"]));
    assert!(
        md.contains("| added | timeout | `charge` |  | `timeout 30s` |"),
        "{md}"
    );
    let j: serde_json::Value =
        serde_json::from_str(&stdout(&run(&["diff", o, n, "--format", "json"]))).unwrap();
    assert_eq!(j["workflows"][0]["status"], "changed");
    assert_eq!(j["compatibility"]["compatible"], true);

    let missing = run(&["diff", o, n, "--workflow", "Nope"]);
    assert_eq!(missing.status.code(), Some(2));
    assert!(stderr(&missing).contains("no workflow `Nope`"));
    assert_eq!(run(&["diff", o, "nope.orbit"]).status.code(), Some(2));
}

#[test]
fn policy_check_compile_and_deploy() {
    let ok = run(&[
        "check",
        "--policy",
        "policies/fintech.toml",
        "approval.orbit",
    ]);
    assert!(ok.status.success(), "{}", stderr(&ok));
    assert!(stderr(&ok).contains("warning[P0106]"));
    assert!(stdout(&ok).contains("ok OrderApproval@1.0.0"));

    let bad = run(&[
        "check",
        "--policy",
        "policies/fintech.toml",
        "checkout.orbit",
    ]);
    assert_eq!(bad.status.code(), Some(1));
    let err = stderr(&bad);
    assert!(
        err.contains("error[P0101]: policy rule `payments-compensated`"),
        "{err}"
    );
    assert!(err.contains("error[P0105]"), "{err}");
    assert!(
        err.contains("error: policy check failed: 2 error(s), 1 warning(s)"),
        "{err}"
    );

    let strict = run(&[
        "check",
        "--strict",
        "--policy",
        "policies/fintech.toml",
        "approval.orbit",
    ]);
    assert_eq!(strict.status.code(), Some(1));

    let json = run(&[
        "check",
        "--format",
        "json",
        "--policy",
        "policies/fintech.toml",
        "checkout.orbit",
    ]);
    assert_eq!(json.status.code(), Some(1));
    let v: serde_json::Value = serde_json::from_str(&stdout(&json)).unwrap();
    let codes: Vec<&str> = v["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["code"].as_str().unwrap())
        .collect();
    assert_eq!(codes, ["P0101", "P0105", "P0104"]);

    let dir = tmp("policy");
    let c = run(&[
        "compile",
        "--policy",
        "policies/fintech.toml",
        "checkout.orbit",
        "-o",
        dir.to_str().unwrap(),
    ]);
    assert_eq!(c.status.code(), Some(1));
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);

    // Deploy checks the policy before contacting the server.
    let d = run(&[
        "deploy",
        "--policy",
        "policies/fintech.toml",
        "checkout.orbit",
        "--server",
        "http://127.0.0.1:9",
    ]);
    assert_eq!(d.status.code(), Some(1));
    assert!(stderr(&d).contains("policy check failed"), "{}", stderr(&d));

    let broken = dir.join("broken.toml");
    std::fs::write(&broken, "[[rule]]\ntype = \"require_everything\"\n").unwrap();
    let b = run(&[
        "check",
        "--policy",
        broken.to_str().unwrap(),
        "approval.orbit",
    ]);
    assert_eq!(b.status.code(), Some(1));
    assert!(stderr(&b).contains("unknown type `require_everything`"));
}

#[test]
fn doc_comments_are_kept_by_fmt() {
    let dir = tmp("fmt");
    let f = dir.join("doc.orbit");
    std::fs::write(
        &f,
        "/// Says hello.\nworkflow Hello {\n    version \"1.0.0\"\n    /// The only step.\n    step pause { sleep 1s }\n}\n",
    )
    .unwrap();
    let o = orbit().arg("fmt").arg("--check").arg(&f).output().unwrap();
    assert!(o.status.success(), "{}", stderr(&o));
}
