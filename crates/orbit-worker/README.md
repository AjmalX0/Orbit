# orbit-worker (Rust SDK)

Worker SDK for the [ORBIT](../../README.md) runtime, with typed handlers via serde.

```rust
use orbit_worker::{TaskError, Worker};
use serde::{Deserialize, Serialize};

#[derive(Deserialize)] struct Input { order_id: String }
#[derive(Serialize)] struct Output { reservation_id: String }

let mut worker = Worker::builder("inventory-rust").concurrency(8).build();
worker.task("inventory.reserve", "1.0.0", |input: Input, ctx| {
    if ctx.is_cancelled() { return Err(TaskError::permanent("CANCELLED", "stopped")); }
    Ok(Output { reservation_id: format!("res-{}", input.order_id) })
});
worker.run()?; // call worker.clone().stop() from another thread to drain and exit
```

* Input that does not deserialize fails the attempt with a permanent
  `INVALID_INPUT` error. Panics are caught and reported as retryable `PANIC`.
* Configuration comes from the builder or from `ORBIT_SERVER`,
  `ORBIT_WORKER_TOKEN` / `ORBIT_TOKEN` and `ORBIT_CA_CERT`.
* Example worker: `cargo run -p orbit-worker --example text_worker`.
