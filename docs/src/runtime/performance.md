# Performance

This page gives measured throughput and latency for the runtime, explains
what bounds them, and shows how to reproduce the numbers on your own
hardware. The figures are from two benchmark programs in the repository.
They measure the runtime itself: the task handlers do no work, so a real
deployment is usually limited by its workers instead.

## What was measured

Each execution runs a workflow with three sequential task steps (each step's
input is the previous step's output), so one execution is three dispatches
and three results.

| Program | Measures |
|---------|----------|
| `bench_engine` (`crates/orbit-runtime/examples`) | The decider and the store only: one thread starts every execution, then plays the worker (poll, complete) until all have completed. No network. |
| `bench_http` (`crates/orbit-worker/examples`) | The whole path: executions started over the HTTP API by 4 client threads, and tasks served by 4 Rust SDK workers with 8 handler threads each, over the HTTP worker protocol. |

**Machine:** 4 vCPU (Intel Xeon, 2.1 GHz), 16 GB RAM, Linux, a container's
local disk. PostgreSQL 16 ran on the same machine with its default settings
(`synchronous_commit = on`). ORBIT 0.4.0, release build.

## Results

### End to end (`bench_http`, 2000 executions)

| Store | Executions/s | Steps/s | Latency p50 | p90 | p99 |
|-------|-------------:|--------:|------------:|----:|----:|
| In memory (no durability) | 1210 | 3629 | 11 ms | 15 ms | 18 ms |
| SQLite file | 244 | 731 | 52 ms | 71 ms | 101 ms |
| PostgreSQL | 111 | 332 | 113 ms | 151 ms | 206 ms |

Latency is from the execution's creation to its completion, as recorded in
its history (`created_at_ms` to `finished_at_ms`), with all 2000 executions
started as fast as the clients could.

### Engine only (`bench_engine`)

| Store | Executions | Executions/s | Steps/s |
|-------|-----------:|-------------:|--------:|
| In memory | 5000 | 4464 | 13392 |
| In memory, 10 steps each | 5000 | 1346 | 13464 |
| SQLite file | 5000 | 425 | 1276 |
| PostgreSQL | 2000 | 138 | 413 |

### Several servers (shared mode)

Throughput of [shared mode](cluster.md#shared-mode-several-active-servers)
with one, two and three servers, on the same machine, is in
[Running several servers: throughput](cluster.md#throughput).

## What bounds the numbers

- **Durable commits.** Every change to an execution is committed to the
  store before it takes effect: one transaction when a step is scheduled and
  dispatched, one when its result arrives. With SQLite that is a WAL write
  with `synchronous = FULL` (an fsync); with PostgreSQL it is one statement
  and a WAL flush. On a durable store, throughput is therefore set by how many
  commits per second the disk can sustain, not by the CPU. Faster disks, or a
  PostgreSQL server with battery-backed or fast NVMe storage, raise it
  directly.
- **One engine per server.** Commits for all executions on one server are
  serialized. With PostgreSQL, the round-trip time to the database is part of
  every commit, so run the database close to the server.
- **Steps, not executions.** Throughput in steps per second stays about the
  same as steps per execution grow (13392 against 13464 above); plan capacity
  in steps.
- **The number of running executions does not matter.** Workers are handed
  the oldest scheduled step from an index, so the cost of a poll does not grow
  with the number of executions in flight: the engine benchmark runs at the
  same rate with 1000 or 5000 executions running at once.

Grouping the commits of several executions into one transaction would raise
durable throughput further, and is planned work.

## Reproducing

```bash
cargo run --release -p orbit-runtime --example bench_engine -- --executions 5000
cargo run --release -p orbit-runtime --example bench_engine -- --store /tmp/bench.db
cargo run --release -p orbit-runtime --example bench_engine -- --store postgres://orbit:orbit@127.0.0.1/orbit_bench
```

```bash
cargo run --release -p orbit-worker --example bench_http -- --executions 2000
cargo run --release -p orbit-worker --example bench_http -- --store /tmp/bench.db
```

`bench_http` can also drive a runtime you started yourself, for example a
server with your production settings:

```bash
orbit server --token admin --worker-token worker --db /var/lib/orbit/bench.db
cargo run --release -p orbit-worker --example bench_http -- --server http://127.0.0.1:7233
```

| Option | Default | Meaning |
|--------|---------|---------|
| `--executions` | `2000` | Executions to run. |
| `--steps` | `3` | Sequential task steps per execution. |
| `--store` | `memory` | `memory`, a SQLite file path (deleted first), or a PostgreSQL URL (used as is; give it an empty database). |
| `--server` | none | `bench_http` only: use this runtime instead of starting one in-process. |
| `--token`, `--worker-token` | `admin`, `worker` | `bench_http` only: credentials for `--server`. |
| `--workers`, `--concurrency` | `4`, `8` | `bench_http` only: SDK workers, and handler threads per worker. |
| `--clients` | `4` | `bench_http` only: threads starting executions. |

Each run prints its parameters, throughput and latency. Against an
`orbit server` on a SQLite file, on the machine above:

```text
runtime      http://127.0.0.1:7391
executions   1000 (3 task steps each)
workers      4 x 8 handler threads, 4 client threads
starts       238/s
total        4.25s
throughput   236 executions/s, 707 steps/s
latency      p50 55ms, p90 72ms, p99 101ms (start to completion)
```

## See also

- [Storage](storage.md)
- [Running several servers](cluster.md)
- [Delivery guarantees](guarantees.md)
