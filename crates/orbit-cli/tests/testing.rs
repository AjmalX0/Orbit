//! `orbit test`, `orbit simulate` and `orbit replay` (offline).

use std::path::PathBuf;
use std::process::Command;

fn orbit() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_orbit"));
    c.env("NO_COLOR", "1");
    c
}

fn examples() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples")
}

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("orbit-testing-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn example_tests_pass() {
    let out = orbit()
        .arg("test")
        .arg(examples().join("approval.test.orbit"))
        .arg(examples().join("order_processing.test.orbit"))
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains("ok      large orders wait for approval"),
        "{text}"
    );
    assert!(
        text.contains("test result: ok. 10 passed, 0 failed, 0 filtered out"),
        "{text}"
    );
}

#[test]
fn failing_tests_exit_1_with_details_and_json() {
    let dir = tmp("fail");
    let file = dir.join("t.test.orbit");
    std::fs::write(
        &file,
        format!(
            "import {:?}\n\ntest \"wrong status\" {{\n    workflow OrderApproval\n    input {{ order_id: \"S1\", customer_id: \"c\", amount: 1.0 }}\n    mock \"payments.charge\" fails \"CARD_DECLINED\"\n    expect status COMPLETED\n}}\n\ntest \"unmocked\" {{\n    workflow OrderApproval\n    input {{ order_id: \"S1\", customer_id: \"c\", amount: 1.0 }}\n    expect status COMPLETED\n}}\n",
            examples().join("approval.orbit").display().to_string()
        ),
    )
    .unwrap();
    let out = orbit().arg("test").arg(&file).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(text.contains("FAILED  wrong status"), "{text}");
    assert!(
        text.contains("expected status COMPLETED, got FAILED (CARD_DECLINED"),
        "{text}"
    );
    assert!(text.contains("StepAttemptFailed"), "timeline: {text}");
    assert!(
        text.contains("task `payments.charge` (step `charge`) is not mocked"),
        "{text}"
    );
    assert!(text.contains("0 passed, 2 failed"), "{text}");

    let out = orbit()
        .args(["test", "--format", "json", "--filter", "unmocked"])
        .arg(&file)
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["failed"], 1);
    assert_eq!(v["filtered_out"], 1);
    assert_eq!(v["tests"][0]["status"], "failed");
    assert_eq!(v["tests"][0]["line"], 10);
}

#[test]
fn simulate_then_replay() {
    let dir = tmp("sim");
    let out = orbit()
        .arg("simulate")
        .arg(examples().join("approval.orbit"))
        .args([
            "--input",
            r#"{"order_id":"T1","customer_id":"c-1","amount":2500}"#,
            "--signal",
            r#"review={"approved":false,"reviewer":"dana"}"#,
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["status"], "COMPLETED");
    assert_eq!(v["output"]["status"], "rejected");
    let history = dir.join("history.json");
    std::fs::write(
        &history,
        serde_json::to_string(
            &serde_json::json!({ "execution_id": "sim", "events": v["history"] }),
        )
        .unwrap(),
    )
    .unwrap();

    let out = orbit()
        .arg("replay")
        .arg("--history")
        .arg(&history)
        .arg("--definition")
        .arg(examples().join("approval.orbit"))
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{text}{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains("every recorded decision is reproduced"),
        "{text}"
    );

    let out = orbit()
        .args(["replay", "--at", "6", "--history"])
        .arg(&history)
        .arg("--definition")
        .arg(examples().join("approval.orbit"))
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("state after #6 SignalDelivered"), "{text}");
    assert!(
        text.contains(r#"decision: {"approved":false,"reviewer":"dana"}"#),
        "{text}"
    );

    // A definition whose decisions differ is reported as a divergence.
    let changed = dir.join("approval.orbit");
    std::fs::copy(
        examples().join("contracts.orbit"),
        dir.join("contracts.orbit"),
    )
    .unwrap();
    std::fs::write(
        &changed,
        std::fs::read_to_string(examples().join("approval.orbit"))
            .unwrap()
            .replace("input.amount >= 1000.0", "input.amount >= 5000.0"),
    )
    .unwrap();
    let out = orbit()
        .arg("replay")
        .arg("--history")
        .arg(&history)
        .arg("--definition")
        .arg(&changed)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(text.contains("divergence at #3"), "{text}");
}

#[test]
fn simulate_generates_samples_and_applies_failures() {
    let out = orbit()
        .arg("simulate")
        .arg(examples().join("approval.orbit"))
        .args([
            "--input",
            r#"{"order_id":"T1","customer_id":"c-1","amount":10}"#,
            "--fail",
            "notifications.send=MAILBOX_FULL",
        ])
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(out.status.code(), Some(1), "{text}");
    assert!(text.contains("FAILED sim  OrderApproval@1.0.0"), "{text}");
    assert!(text.contains("compensation: completed"), "{text}");
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("generated sample results for: payments.charge, payments.refund"),
        "{err}"
    );
}
