# ORBIT worker protocol v1

The messages are defined in [`orbit/worker/v1/worker.proto`](orbit/worker/v1/worker.proto).
ORBIT v0.1 implements the **HTTP/JSON binding**: each RPC is a `POST` with a
JSON body that follows the proto3 JSON mapping. Any language with an HTTP
client can implement a worker. The official SDKs (`sdks/python`,
`sdks/typescript`, `sdks/go`, `crates/orbit-worker`) are a few hundred lines each.

When the runtime has a worker credential (`--worker-token`), send it as
`Authorization: Bearer <token>`. It is accepted only on the endpoints below.

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

## Example

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
