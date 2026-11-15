//! OpenTelemetry export against a stub OTLP/HTTP collector.

use orbit_runtime::engine::{ContractAnnouncement, RegisterWorker};
use orbit_runtime::model::TaskError;
use orbit_runtime::telemetry::{root_span_id, trace_id, TelemetryConfig};
use orbit_runtime::{Engine, EngineConfig, Store};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const SRC: &str = r#"
task "a" { version "1.0.0" input { n: Int } output { n: Int } }
workflow Child { version "1.0.0" input { n: Int } output { n: Int }
    step work { worker "a" with { n: input.n } retry { attempts: 2, initial_delay: 1ms } }
    return { n: work.n } }
workflow Parent { version "1.0.0" input { n: Int }
    step first { worker "a" with { n: input.n } }
    step kid { workflow "Child" after first with { n: first.n } }
    return { n: kid.n } }
"#;

/// A minimal HTTP server that records POSTed JSON bodies on /v1/traces.
fn collector() -> (String, Arc<Mutex<Vec<Value>>>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let bodies = Arc::new(Mutex::new(vec![]));
    let sink = bodies.clone();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            reader.read_line(&mut request_line).unwrap();
            let mut length = 0;
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                if line == "\r\n" || line.is_empty() {
                    break;
                }
                if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = v.trim().parse().unwrap();
                }
            }
            let mut body = vec![0; length];
            reader.read_exact(&mut body).unwrap();
            assert!(
                request_line.starts_with("POST /v1/traces"),
                "{request_line}"
            );
            sink.lock()
                .unwrap()
                .push(serde_json::from_slice(&body).unwrap());
            let mut s = stream;
            let _ = s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\n{}");
        }
    });
    (url, bodies)
}

fn spans(bodies: &[Value]) -> Vec<Value> {
    bodies
        .iter()
        .flat_map(|b| {
            b["resourceSpans"][0]["scopeSpans"][0]["spans"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .collect()
}

fn attr<'a>(span: &'a Value, key: &str) -> Option<&'a Value> {
    span["attributes"]
        .as_array()?
        .iter()
        .find(|a| a["key"] == key)
        .map(|a| &a["value"])
}

#[test]
fn executions_export_nested_traces() {
    let (url, bodies) = collector();
    let c = orbit_compiler::compile_str("t.orbit", SRC);
    assert!(!c.has_errors(), "{}", c.render_diagnostics(false));
    let mut e = Engine::open(
        Store::in_memory().unwrap(),
        EngineConfig::default(),
        orbit_runtime::engine::system_clock(),
    )
    .unwrap();
    e.enable_telemetry(TelemetryConfig {
        endpoint: url,
        headers: vec![],
        service_name: "orbit-test".into(),
    });
    for wf in &c.workflows {
        e.register_definition(wf.clone()).unwrap();
    }
    let id = e
        .start("Parent", None, json!({ "n": 2 }), Some("trace-me".into()))
        .unwrap()
        .execution_id;
    let w = e
        .register_worker(RegisterWorker {
            worker_id: "otel-worker".into(),
            runtime: "rust".into(),
            sdk: None,
            contracts: vec![ContractAnnouncement {
                task_name: "a".into(),
                contract_version: "1.0.0".into(),
            }],
        })
        .unwrap()
        .worker_session_id;
    let t = e.poll(&w).unwrap().unwrap();
    e.complete(&t.token, json!({ "n": 2 })).unwrap();
    e.tick().unwrap(); // start the child
    let t = e.poll(&w).unwrap().unwrap();
    e.fail(&t.token, TaskError::new("FLAKY", "first try", true))
        .unwrap();
    std::thread::sleep(Duration::from_millis(5));
    e.tick().unwrap(); // retry
    let t = e.poll(&w).unwrap().unwrap();
    e.complete(&t.token, json!({ "n": 4 })).unwrap();
    e.tick().unwrap(); // child result reaches the parent
    assert_eq!(e.execution(&id).unwrap().output, Some(json!({ "n": 4 })));

    let deadline = Instant::now() + Duration::from_secs(10);
    let all = loop {
        let all = spans(&bodies.lock().unwrap());
        if all
            .iter()
            .filter(|s| s["name"].as_str().unwrap_or("").starts_with("workflow "))
            .count()
            == 2
            || Instant::now() > deadline
        {
            break all;
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    assert_eq!(
        bodies.lock().unwrap()[0]["resourceSpans"][0]["resource"]["attributes"][0]["value"]
            ["stringValue"],
        "orbit-test"
    );

    let trace = trace_id("trace-me");
    assert!(
        all.iter().all(|s| s["traceId"] == trace),
        "the child joins the parent's trace"
    );
    let by_name = |n: &str| {
        all.iter()
            .filter(|s| s["name"] == n)
            .cloned()
            .collect::<Vec<_>>()
    };
    let parent_root = &by_name("workflow Parent")[0];
    let child_root = &by_name("workflow Child")[0];
    assert_eq!(parent_root["spanId"], root_span_id("trace-me"));
    assert!(parent_root.get("parentSpanId").is_none());
    assert_eq!(parent_root["status"]["code"], 1);

    // The child's root span hangs under the parent's `kid` step span.
    let kid = &by_name("step kid")[0];
    assert_eq!(kid["parentSpanId"], parent_root["spanId"]);
    assert_eq!(child_root["parentSpanId"], kid["spanId"]);
    assert_eq!(
        attr(child_root, "orbit.execution_id").unwrap()["stringValue"],
        "trace-me.kid"
    );

    // The retried child step: one failed attempt span, one successful one.
    let work = by_name("step work");
    assert_eq!(work.len(), 2);
    assert!(work
        .iter()
        .all(|s| s["parentSpanId"] == child_root["spanId"]));
    let failed = work
        .iter()
        .find(|s| s["status"]["code"] == 2)
        .expect("failed attempt");
    assert!(failed["status"]["message"]
        .as_str()
        .unwrap()
        .starts_with("FLAKY"));
    assert_eq!(attr(failed, "orbit.attempt").unwrap()["intValue"], "1");
    assert_eq!(
        attr(failed, "orbit.worker_id").unwrap()["stringValue"],
        "otel-worker"
    );
    assert_ne!(work[0]["spanId"], work[1]["spanId"]);
    let ok = work.iter().find(|s| s["status"]["code"] == 1).unwrap();
    assert_eq!(attr(ok, "orbit.attempt").unwrap()["intValue"], "2");
    // Times are well-formed and ordered.
    for s in &all {
        let start: u128 = s["startTimeUnixNano"].as_str().unwrap().parse().unwrap();
        let end: u128 = s["endTimeUnixNano"].as_str().unwrap().parse().unwrap();
        assert!(start > 0 && end >= start, "{s}");
    }
}
