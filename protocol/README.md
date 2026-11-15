# ORBIT worker protocol v1

The messages are defined in [`orbit/worker/v1/worker.proto`](orbit/worker/v1/worker.proto).
There are two bindings of the same service, with the same semantics:

- **HTTP/JSON** (always on): each RPC is a `POST` with a JSON body following
  the proto3 JSON mapping. Any language with an HTTP client can implement a
  worker. The official SDKs (Python, TypeScript, Go, Rust, Java, C#) use this
  binding, and each is a few hundred lines.
- **gRPC** (`orbit server --grpc-addr 127.0.0.1:7234`): the
  `orbit.worker.v1.WorkerService` service. Generate a client from the
  `.proto` with your language's gRPC tooling.

Credentials work the same on both bindings: `Authorization: Bearer <token>`
(HTTP header or gRPC metadata) with the shared worker token or a per-worker
token, or a client certificate under mutual TLS. See
[docs/runtime.md](../docs/runtime.md#security).

## Lifecycle

```text
worker                                   runtime
  │ POST /v1/workers/register ──────────▶ │  {worker_id, runtime, contracts[]}
  │ ◀──────────── {worker_session_id, heartbeat_interval_ms, session_timeout_ms}
  │
  │ POST /v1/workers/poll ──────────────▶ │  {worker_session_id, wait_ms}
  │ ◀───────── 200 ExecuteTaskRequest  |  204 (nothing within wait_ms)
  │   ...run business logic...
  │ POST /v1/tasks/complete ────────────▶ │  {token, output}
  │   or POST /v1/tasks/fail ───────────▶ │  {token, error_code, error_message, retryable}
  │ ◀──────────────────── {accepted, reason?}
  │
  │ POST /v1/workers/heartbeat (every heartbeat_interval_ms)
  │ ◀──────────────────── {accepted, cancel: [token...]}
  │ POST /v1/workers/unregister (on shutdown)
```

## Rules for implementers

1. **Session loss.** If any session call returns `404 UNKNOWN_SESSION` (for
   example after a runtime restart), register again and continue. Tokens stay
   valid across sessions.
2. **Result delivery must survive outages.** Retry `complete`/`fail` on
   connection errors and 5xx until the runtime answers. The runtime accepts a
   result for any token of an unresolved step (reconciliation), so a result
   from before a crash is not lost.
3. **Treat `accepted: false` as final.** Duplicate or stale results are
   recorded and ignored. Do not retry them.
4. **Idempotency.** Use `idempotency_key` for external side effects. It is
   stable across retries (`attempt` increases) and redeliveries (`redelivery:
   true`, same `attempt`).
5. **Cancellation is cooperative.** When a token appears in a heartbeat's
   `cancel` list, stop the work if you can. Any result you send for that token
   is then ignored.
6. **Liveness.** Polls and heartbeats both count as contact. If a session is
   silent for longer than `session_timeout_ms`, its in-flight tasks are
   redelivered elsewhere.
7. **Contracts.** Announce `task_name` and `contract_version` exactly as
   declared in `.orbit` files. Output is validated against the declared output
   schema, and a mismatch fails the step with `OUTPUT_CONTRACT_VIOLATION`
   (not retried).

## gRPC specifics

- `PollTask` returns an `ExecuteTaskRequest` with an **empty `token`** when no
  task arrived within `wait_ms`. HTTP answers `204` in the same case.
- `google.protobuf.Value` carries numbers as doubles. The runtime converts
  whole numbers within ±2^53 back to integers, so `Int` fields round-trip.
- Errors map to gRPC status codes: `INVALID_ARGUMENT` (400),
  `UNAUTHENTICATED` (401), `PERMISSION_DENIED` (403), `NOT_FOUND` (404, e.g.
  `UNKNOWN_SESSION`), `FAILED_PRECONDITION` (409) and `INTERNAL`. The ORBIT
  error code is also in the `orbit-error-code` response metadata, and as the
  `CODE:` prefix of the status message.

A complete, tested gRPC client (register, poll, complete, fail, heartbeat,
unregister, over plaintext and mutual TLS) is in
[`crates/orbit-runtime/tests/grpc.rs`](../crates/orbit-runtime/tests/grpc.rs).

## Example (HTTP/JSON)

A worker built from nothing but `curl`, serving the `Hello` workflow created by
`orbit init`. `runtime` must match the step's `using python`:

```bash
curl -s localhost:7233/v1/workers/register -H 'content-type: application/json' -d '{
  "worker_id": "shell-1", "runtime": "python",
  "contracts": [{"task_name": "greeting.compose", "contract_version": "1.0.0"}]}'
# {"accepted":true,"worker_session_id":"6f1c...","heartbeat_interval_ms":5000,...}

curl -s localhost:7233/v1/workers/poll -H 'content-type: application/json' \
  -d '{"worker_session_id": "6f1c...", "wait_ms": 20000}'
# {"token":"a9e2...","execution_id":"...","step_id":"compose","task_name":"greeting.compose",
#  "input":{"name":"Ada"},"idempotency_key":".../compose","attempt":1,"redelivery":false,...}

curl -s localhost:7233/v1/tasks/complete -H 'content-type: application/json' \
  -d '{"token": "a9e2...", "output": {"message": "Hello, Ada"}}'
# {"accepted":true}
```
