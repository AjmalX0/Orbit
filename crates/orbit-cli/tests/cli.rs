//! CLI smoke tests: scaffolding, checking, compiling and graphing.

use std::path::PathBuf;
use std::process::Command;

fn orbit() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_orbit"));
    c.env("NO_COLOR", "1");
    c
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("orbit-cli-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

#[test]
fn init_produces_a_project_that_compiles() {
    let dir = tmp("init");
    let out = orbit().arg("init").arg(&dir).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let check = orbit()
        .arg("check")
        .arg(dir.join("workflows/hello.orbit"))
        .output()
        .unwrap();
    assert!(
        check.status.success(),
        "{}",
        String::from_utf8_lossy(&check.stderr)
    );
    assert!(String::from_utf8_lossy(&check.stdout).contains("ok Hello@1.0.0 (2 steps)"));
    // The generated Python worker must at least be valid Python.
    if let Ok(py) = Command::new("python3")
        .arg("-m")
        .arg("py_compile")
        .arg(dir.join("workers/python/worker.py"))
        .output()
    {
        assert!(
            py.status.success(),
            "{}",
            String::from_utf8_lossy(&py.stderr)
        );
    }
    // Refuses to overwrite a non-empty directory.
    assert!(!orbit()
        .arg("init")
        .arg(&dir)
        .output()
        .unwrap()
        .status
        .success());
}

#[test]
fn compile_writes_ir_and_check_reports_json_diagnostics() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    let out_dir = tmp("build");
    let out = orbit()
        .arg("compile")
        .arg(root.join("order_processing.orbit"))
        .arg("-o")
        .arg(&out_dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let ir = out_dir.join("order_processing.orbit.json");
    let v = orbit().arg("validate").arg(&ir).output().unwrap();
    assert!(String::from_utf8_lossy(&v.stdout).contains("is valid orbit-ir/1"));

    let bad = tmp("bad.orbit");
    std::fs::write(
        &bad,
        "workflow W { version \"1.0.0\" step a { worker \"nope\" } }",
    )
    .unwrap();
    let out = orbit()
        .args(["check", "--format", "json"])
        .arg(&bad)
        .output()
        .unwrap();
    assert!(!out.status.success());
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let d = &json["diagnostics"][0];
    assert_eq!(d["code"], "E0300");
    assert_eq!(d["location"]["line"], 1);
    assert_eq!(d["location"]["column"], 46);
}

#[test]
fn graph_renders_mermaid() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples");
    let out = orbit()
        .arg("graph")
        .arg(root.join("order_processing.orbit"))
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.starts_with("flowchart TD"));
    assert!(text.contains("reserve_inventory --> process_payment"));
}
