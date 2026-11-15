//! Engine benchmark: the decider and the store, with no network.
//!
//! Starts `--executions` executions of a workflow with `--steps` sequential
//! task steps, then plays the worker in the same thread (poll, complete) until
//! every execution has completed. Reports executions and steps per second.
//!
//! ```bash
//! cargo run --release -p orbit-runtime --example bench_engine -- --executions 5000
//! cargo run --release -p orbit-runtime --example bench_engine -- --store /tmp/bench.db
//! cargo run --release -p orbit-runtime --example bench_engine -- --store postgres://orbit:orbit@127.0.0.1/postgres
//! ```
//!
//! `--store` is `memory` (default), a SQLite file path, or a PostgreSQL URL.
//! A PostgreSQL database is used as is; give the benchmark one it may write to.

use orbit_runtime::engine::{system_clock, ContractAnnouncement, RegisterWorker};
use orbit_runtime::model::WorkflowStatus;
use orbit_runtime::{Engine, EngineConfig, Store};
use serde_json::json;
use std::time::Instant;

fn arg(name: &str, default: &str) -> String {
    let args: Vec<String> = std::env::args().collect();
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1).cloned())
        .unwrap_or_else(|| default.to_string())
}

fn main() {
    let executions: usize = arg("--executions", "2000").parse().expect("--executions");
    let steps: usize = arg("--steps", "3").parse().expect("--steps");
    let store_arg = arg("--store", "memory");

    let mut src = String::from(
        "task \"bench.inc\" { version \"1.0.0\" input { n: Int } output { n: Int } }\n\
         workflow Bench { version \"1.0.0\" input { n: Int }\n",
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
    let ir = c.workflows.into_iter().next().unwrap();

    let store = if store_arg == "memory" {
        Store::in_memory()
    } else if store_arg.starts_with("postgres://") || store_arg.starts_with("postgresql://") {
        Store::connect(&store_arg)
    } else {
        let _ = std::fs::remove_file(&store_arg);
        Store::open(std::path::Path::new(&store_arg))
    }
    .expect("open store");
    let mut engine = Engine::open(store, EngineConfig::default(), system_clock()).unwrap();
    engine.register_definition(ir).unwrap();
    let session = engine
        .register_worker(RegisterWorker {
            worker_id: "bench".into(),
            runtime: "rust".into(),
            sdk: None,
            contracts: vec![ContractAnnouncement {
                task_name: "bench.inc".into(),
                contract_version: "1.0.0".into(),
            }],
        })
        .unwrap()
        .worker_session_id;

    let started = Instant::now();
    let run = format!("{:x}", std::process::id());
    let mut ids = Vec::with_capacity(executions);
    for i in 0..executions {
        let id = format!("bench-{run}-{i}");
        engine
            .start("Bench", None, json!({ "n": i }), Some(id.clone()))
            .unwrap();
        ids.push(id);
    }
    let start_secs = started.elapsed().as_secs_f64();
    let mut done_steps = 0usize;
    while let Some(t) = engine.poll(&session).unwrap() {
        let n = t.input["n"].as_i64().unwrap();
        engine.complete(&t.token, json!({ "n": n + 1 })).unwrap();
        done_steps += 1;
    }
    let total = started.elapsed().as_secs_f64();
    let completed = ids
        .iter()
        .filter(|id| engine.execution(id).unwrap().status == WorkflowStatus::Completed)
        .count();
    assert_eq!(completed, executions, "every execution should complete");

    println!("store        {store_arg}");
    println!("executions   {executions} ({steps} task steps each, {done_steps} steps)");
    println!("starts       {:.0}/s", executions as f64 / start_secs);
    println!("total        {total:.2}s");
    println!(
        "throughput   {:.0} executions/s, {:.0} steps/s",
        executions as f64 / total,
        done_steps as f64 / total
    );
}
