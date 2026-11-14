//! Rust part of examples/polyglot.orbit: text.fingerprint.
//!
//! Run: cargo run -p orbit-worker --example text_worker

use orbit_worker::Worker;
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize)]
struct Input {
    text: String,
}

/// 64-bit FNV-1a, a stable non-cryptographic fingerprint.
fn fnv1a(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf29ce484222325, |h, b| {
        (h ^ *b as u64).wrapping_mul(0x100000001b3)
    })
}

fn main() {
    let mut worker = Worker::builder("text-rust").build();
    worker.task("text.fingerprint", "1.0.0", |input: Input, _ctx| {
        Ok(json!({ "hash": format!("{:016x}", fnv1a(input.text.as_bytes())), "bytes": input.text.len() }))
    });
    // `worker.clone().stop()` from a signal handler (e.g. the `ctrlc` crate) drains
    // in-flight tasks; a hard kill is also safe because delivery is at-least-once.
    worker.run().expect("worker failed");
}
