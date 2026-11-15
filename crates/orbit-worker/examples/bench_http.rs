//! End-to-end benchmark: HTTP API, worker protocol and Rust SDK workers.
//!
//! Deploys a workflow with `--steps` sequential task steps, starts
//! `--executions` executions over HTTP from `--clients` threads, serves the
//! tasks with Rust SDK workers (`--workers` processes' worth of `Worker`s, each
//! with `--concurrency` handler threads), and waits until every execution has
//! completed. Reports throughput and the latency of whole executions.
//!
//! ```bash
//! # An in-process runtime on an in-memory store (no durability):
//! cargo run --release -p orbit-worker --example bench_http -- --executions 2000
//! # An in-process runtime on a SQLite file:
//! cargo run --release -p orbit-worker --example bench_http -- --store /tmp/bench.db
//! # A runtime you started yourself (`orbit server --token admin --worker-token worker`):
//! cargo run --release -p orbit-worker --example bench_http -- --server http://127.0.0.1:7233
//! ```

use orbit_runtime::engine::system_clock;
use orbit_runtime::server::{serve, ServerConfig};
use orbit_runtime::{Engine, EngineConfig, Store};
use orbit_worker::Worker;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn arg(name: &str, default: &str) -> String {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
        .unwrap_or_else(|| default.to_string())
}

fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .http_status_as_error(false)
        .build()
        .into()
}

fn call(
    agent: &ureq::Agent,
    url: &str,
    token: &str,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> Value {
    let full = format!("{url}{path}");
    let mut resp = match body {
        Some(b) if method == "POST" => agent
            .post(&full)
            .header("authorization", format!("Bearer {token}"))
            .send_json(b),
        _ => agent
            .get(&full)
            .header("authorization", format!("Bearer {token}"))
            .call(),
    }
    .expect("request");
    let status = resp.status().as_u16();
    let v: Value = resp.body_mut().read_json().unwrap_or(Value::Null);
    assert!(status < 300, "{method} {path}: {status} {v}");
    v
}

fn start_runtime(store: Store) -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let addr = format!("127.0.0.1:{port}").parse().unwrap();
    std::thread::spawn(move || {
        let engine = Engine::open(store, EngineConfig::default(), system_clock()).unwrap();
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(async move {
                let cfg = ServerConfig {
                    addr,
                    auth_token: Some("admin".into()),
                    worker_token: Some("worker".into()),
                    identities: Default::default(),
                    grpc_addr: None,
                    listener: Some(listener),
                    grpc_listener: None,
                    tls: None,
                    body_limit: 1 << 20,
                    tick_interval: Duration::from_millis(200),
                };
                serve(engine, cfg, std::future::pending()).await.unwrap();
            });
    });
    let url = format!("http://127.0.0.1:{port}");
    for _ in 0..200 {
        if ureq::get(format!("{url}/v1/health")).call().is_ok() {
            return url;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("runtime did not start");
}

#[derive(Deserialize)]
struct N {
    n: i64,
}

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let i = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[i]
}

fn main() {
    let executions: usize = arg("--executions", "2000").parse().expect("--executions");
    let steps: usize = arg("--steps", "3").parse().expect("--steps");
    let workers: usize = arg("--workers", "4").parse().expect("--workers");
    let concurrency: usize = arg("--concurrency", "8").parse().expect("--concurrency");
    let clients: usize = arg("--clients", "4").parse().expect("--clients");
    let store_arg = arg("--store", "memory");
    let server = arg("--server", "");
    let admin_token = arg("--token", "admin");
    let worker_token = arg("--worker-token", "worker");

    let url = if server.is_empty() {
        let store = if store_arg == "memory" {
            Store::in_memory()
        } else if store_arg.starts_with("postgres") {
            Store::connect(&store_arg)
        } else {
            let _ = std::fs::remove_file(&store_arg);
            Store::open(std::path::Path::new(&store_arg))
        }
        .expect("open store");
        start_runtime(store)
    } else {
        server.clone()
    };

    // A fresh workflow name per run, so repeated runs against one server work.
    let name = format!("Bench{}", std::process::id());
    let mut src = format!(
        "task \"bench.inc\" {{ version \"1.0.0\" input {{ n: Int }} output {{ n: Int }} }}\n\
         workflow {name} {{ version \"1.0.0\" input {{ n: Int }}\n"
    );
    for i in 0..steps {
        let from = if i == 0 {
            "input".to_string()
        } else {
            format!("s{}", i - 1)
        };
        src.push_str(&format!(
            "  step s{i} {{ worker \"bench.inc\" with {{ n: {from}.n }} }}\n"
        ));
    }
    src.push_str(&format!("  return {{ n: s{}.n }}\n}}\n", steps - 1));
    let c = orbit_compiler::compile_str("bench.orbit", &src);
    assert!(!c.has_errors(), "{}", c.render_diagnostics(false));
    let a = agent();
    call(
        &a,
        &url,
        &admin_token,
        "POST",
        "/v1/workflows",
        Some(serde_json::to_value(&c.workflows[0]).unwrap()),
    );

    for w in 0..workers {
        let mut worker = Worker::builder(&format!("bench-{w}"))
            .server(&url)
            .token(&worker_token)
            .concurrency(concurrency)
            .poll_wait(Duration::from_millis(500))
            .build();
        worker.task("bench.inc", "1.0.0", |i: N, _| Ok(json!({ "n": i.n + 1 })));
        std::thread::spawn(move || worker.run().unwrap());
    }
    std::thread::sleep(Duration::from_millis(300));

    let started = Instant::now();
    let next = Arc::new(AtomicUsize::new(0));
    let run = format!("{:x}", std::process::id());
    let threads: Vec<_> = (0..clients)
        .map(|_| {
            let (next, url, token, name, run) = (
                next.clone(),
                url.clone(),
                admin_token.clone(),
                name.clone(),
                run.clone(),
            );
            std::thread::spawn(move || {
                let a = agent();
                loop {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    if i >= executions {
                        break;
                    }
                    call(
                        &a,
                        &url,
                        &token,
                        "POST",
                        "/v1/executions",
                        Some(json!({
                            "workflow": name,
                            "execution_id": format!("bench-{run}-{i}"),
                            "input": { "n": i },
                        })),
                    );
                }
            })
        })
        .collect();
    for t in threads {
        t.join().unwrap();
    }
    let start_secs = started.elapsed().as_secs_f64();

    // Wait until no execution of this run is still running.
    loop {
        let running = call(
            &a,
            &url,
            &admin_token,
            "GET",
            &format!("/v1/executions?status=RUNNING&workflow={name}&limit=1"),
            None,
        );
        if running["executions"]
            .as_array()
            .is_none_or(|v| v.is_empty())
        {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let total = started.elapsed().as_secs_f64();

    let mut latencies = Vec::with_capacity(executions);
    for i in 0..executions {
        let s = call(
            &a,
            &url,
            &admin_token,
            "GET",
            &format!("/v1/executions/bench-{run}-{i}"),
            None,
        );
        assert_eq!(s["status"], "COMPLETED", "{s}");
        let created = s["created_at_ms"].as_u64().unwrap();
        let finished = s["finished_at_ms"].as_u64().unwrap();
        latencies.push(finished - created);
    }
    latencies.sort_unstable();

    println!(
        "runtime      {}",
        if server.is_empty() {
            format!("in-process, store {store_arg}")
        } else {
            server
        }
    );
    println!("executions   {executions} ({steps} task steps each)");
    println!("workers      {workers} x {concurrency} handler threads, {clients} client threads");
    println!("starts       {:.0}/s", executions as f64 / start_secs);
    println!("total        {total:.2}s");
    println!(
        "throughput   {:.0} executions/s, {:.0} steps/s",
        executions as f64 / total,
        (executions * steps) as f64 / total
    );
    println!(
        "latency      p50 {}ms, p90 {}ms, p99 {}ms (start to completion)",
        percentile(&latencies, 0.5),
        percentile(&latencies, 0.9),
        percentile(&latencies, 0.99)
    );
}
