//! The Rust SDK against a real in-process ORBIT runtime.

use orbit_runtime::engine::system_clock;
use orbit_runtime::server::{serve, ServerConfig};
use orbit_runtime::{Engine, EngineConfig, Store};
use orbit_worker::{TaskError, Worker};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::time::Duration;

const SRC: &str = r#"
task "math.double" { version "1.0.0" input { n: Int } output { n: Int } }
task "math.check" { version "1.0.0" input { n: Int } output { ok: Bool } }
task "math.explode" { version "1.0.0" input { } output { } }
workflow Math {
    version "1.0.0"
    input { n: Int, explode: Bool = false }
    step double { worker "math.double" using rust with { n: input.n } }
    step check  { worker "math.check" with { n: double.n } retry { attempts: 1 } }
    step boom   { worker "math.explode" when input.explode after check retry { attempts: 2, initial_delay: 100ms } }
    return { doubled: double.n, ok: check.ok }
}
"#;

fn start_runtime() -> String {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let addr = format!("127.0.0.1:{port}").parse().unwrap();
    std::thread::spawn(move || {
        let engine = Engine::open(
            Store::in_memory().unwrap(),
            EngineConfig::default(),
            system_clock(),
        )
        .unwrap();
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(async move {
                let cfg = ServerConfig {
                    addr,
                    auth_token: Some("admin".into()),
                    worker_token: Some("worker".into()),
                    identities: Default::default(),
                    grpc_addr: None,
                    tls: None,
                    body_limit: 1 << 20,
                    tick_interval: Duration::from_millis(20),
                };
                serve(engine, cfg, std::future::pending()).await.unwrap();
            });
    });
    let url = format!("http://127.0.0.1:{port}");
    for _ in 0..100 {
        if ureq::get(format!("{url}/v1/health")).call().is_ok() {
            return url;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("runtime did not start");
}

fn admin(url: &str, method: &str, path: &str, body: Option<Value>) -> Value {
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let full = format!("{url}{path}");
    let mut resp = match (method, body) {
        ("POST", Some(b)) => agent
            .post(&full)
            .header("authorization", "Bearer admin")
            .send_json(b)
            .unwrap(),
        _ => agent
            .get(&full)
            .header("authorization", "Bearer admin")
            .call()
            .unwrap(),
    };
    resp.body_mut().read_json().unwrap()
}

#[derive(Deserialize)]
struct N {
    n: i64,
}

#[derive(Serialize)]
struct Ok_ {
    ok: bool,
}

#[test]
fn rust_worker_executes_workflow() {
    let url = start_runtime();
    let c = orbit_compiler::compile_str("math.orbit", SRC);
    assert!(!c.has_errors(), "{}", c.render_diagnostics(false));
    admin(
        &url,
        "POST",
        "/v1/workflows",
        Some(serde_json::to_value(&c.workflows[0]).unwrap()),
    );

    let mut worker = Worker::builder("rust-test")
        .server(&url)
        .token("worker")
        .poll_wait(Duration::from_millis(500))
        .build();
    worker
        .task("math.double", "1.0.0", |i: N, ctx| {
            assert!(!ctx.idempotency_key().is_empty());
            Ok(json!({ "n": i.n * 2 }))
        })
        .task("math.check", "1.0.0", |i: N, _| {
            Ok(Ok_ { ok: i.n % 2 == 0 })
        })
        .task(
            "math.explode",
            "1.0.0",
            |_: Value, ctx| -> Result<Value, TaskError> {
                if ctx.attempt() == 1 {
                    panic!("first attempt panics");
                }
                Err(TaskError::permanent(
                    "EXPLODED",
                    "second attempt fails permanently",
                ))
            },
        );
    let runner = worker.clone();
    let handle = std::thread::spawn(move || runner.run().unwrap());

    // Happy path.
    let id = admin(
        &url,
        "POST",
        "/v1/executions",
        Some(json!({ "workflow": "Math", "input": { "n": 21 } })),
    )["execution_id"]
        .as_str()
        .unwrap()
        .to_string();
    let s = admin(
        &url,
        "GET",
        &format!("/v1/executions/{id}/wait?timeout_ms=10000"),
        None,
    );
    assert_eq!(s["status"], "COMPLETED", "{s:#}");
    assert_eq!(s["output"], json!({ "doubled": 42, "ok": true }));
    assert_eq!(s["units"]["double"]["worker_id"], "rust-test");

    // A panic is a retryable failure; a permanent TaskError ends the workflow.
    let id = admin(
        &url,
        "POST",
        "/v1/executions",
        Some(json!({ "workflow": "Math", "input": { "n": 1, "explode": true } })),
    )["execution_id"]
        .as_str()
        .unwrap()
        .to_string();
    let s = admin(
        &url,
        "GET",
        &format!("/v1/executions/{id}/wait?timeout_ms=10000"),
        None,
    );
    assert_eq!(s["status"], "FAILED", "{s:#}");
    assert_eq!(s["error"]["code"], "EXPLODED");
    assert_eq!(s["units"]["boom"]["attempt"], 2);
    let history = admin(&url, "GET", &format!("/v1/executions/{id}/history"), None);
    let panic_event = history["events"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["error"]["code"] == "PANIC");
    assert!(panic_event.is_some(), "the panicking attempt is recorded");

    // The worker credential cannot use administrative endpoints.
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into();
    let r = agent
        .get(format!("{url}/v1/executions"))
        .header("authorization", "Bearer worker")
        .call()
        .unwrap();
    assert_eq!(r.status().as_u16(), 403);

    worker.stop();
    handle.join().unwrap();
}
