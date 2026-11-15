# Worker protocol

This page specifies version 1 of the ORBIT worker protocol: the messages a
worker exchanges with the runtime to register, receive tasks, report results,
and stay alive. Use it to write a worker in a language without an official
SDK, or to debug one. The protocol has two bindings with the same semantics:
HTTP/JSON, which every runtime serves on its API port and which the official
SDKs use, and gRPC, which the runtime serves when started with
`--grpc-addr`. The canonical message definitions are in
[`protocol/orbit/worker/v1/worker.proto`](https://github.com/AjmalX0/Orbit/blob/main/protocol/orbit/worker/v1/worker.proto).
For the concepts behind the protocol, read [How workers work](../sdks/overview.md).

## Overview

```text
worker                                         runtime
  │ POST /v1/workers/register  ───────────────▶ │
  │ ◀─────────────── worker_session_id, heartbeat_interval_ms, session_timeout_ms
  │
  │ POST /v1/workers/poll  ───────────────────▶ │  long poll, up to wait_ms
  │ ◀──────────────── 200 task assignment   or   204 No Content
  │ POST /v1/tasks/complete  or  /v1/tasks/fail ▶ │
  │ ◀──────────────────────────────── accepted, reason
  │
  │ POST /v1/workers/heartbeat  (every heartbeat_interval_ms)
  │ ◀──────────────────────────────── accepted, cancel[]
  │ POST /v1/workers/unregister  (on shutdown)
```

A worker is a client only. It needs no inbound port, which lets it run behind
NAT and gives natural backpressure: a worker asks for a task only when it has
capacity.

## HTTP conventions

| Aspect | Rule |
|---|---|
| Method | Every worker endpoint is `POST`. |
| Request body | JSON, with `content-type: application/json`. A request without that header is answered `415 Unsupported Media Type`. |
| Field names | snake_case, following the proto3 JSON mapping of `worker.proto`. Unknown fields are ignored. |
| Response header | Every response that passes authentication carries `x-orbit-api-version: orbit-api/1`. |
| Body size | Requests larger than the runtime's `--max-body` (4 MiB by default) are answered `413 Payload Too Large`. |
| TLS | When the runtime runs with `--tls-cert` and `--tls-key`, use `https://`. |

### Errors

Errors produced by the runtime have this body:

```json
{"error": {"code": "UNKNOWN_SESSION", "message": "unknown or expired worker session; register again"}}
```

Branch on `code`, not on `message`. A body that is not valid JSON is answered
`400` and a body that lacks a required field `422`; both have a plain-text
body that describes the problem, not the JSON error object.

| Status | Codes | Meaning |
|---|---|---|
| `400` | `INVALID_REQUEST`, `RESERVED_TASK` | The request is well-formed JSON but not valid. |
| `401` | `UNAUTHENTICATED` | Credentials are required and none were valid. |
| `403` | `FORBIDDEN` | The credential may not perform this call. |
| `404` | `UNKNOWN_SESSION` | The session does not exist, for example after a runtime restart. Register again. |
| `5xx` | various | A runtime or storage failure. Retry with backoff. |

## Authentication

When the runtime runs without `--token` and without a workers file (local
development on a loopback address), no credentials are needed. Otherwise,
send a bearer token on every request:

```text
authorization: Bearer <token>
```

| Credential | Configured with | May call |
|---|---|---|
| Admin token | `--token` / `ORBIT_TOKEN` | Every endpoint, including the worker endpoints. |
| Shared worker token | `--worker-token` / `ORBIT_WORKER_TOKEN` | `/v1/workers/*` and `/v1/tasks/*` only. |
| Per-worker token | `token_sha256` in `--workers-file` | `/v1/workers/*` and `/v1/tasks/*`, limited to its own identity. |
| Client certificate | `client_cert` in `--workers-file`, with `--tls-client-ca` | Same as a per-worker token. |

A worker credential that calls any other endpoint gets `403 FORBIDDEN`
("worker credentials may only call /v1/workers/* and /v1/tasks/*"). A
per-worker identity is further limited, and each violation is a
`403 FORBIDDEN`:

| Rule | Example message |
|---|---|
| Register only under its own `id`. | ``this credential belongs to worker `ruby-1`, not `ruby-2` `` |
| Announce its configured `runtime`, when one is set. | ``worker `ruby-1` must announce runtime `ruby` `` |
| Announce only tasks matching its `tasks` patterns. | ``worker `ruby-1` may not serve: payments.charge (allowed: greeting.*)`` |
| Use only sessions it opened. | ``session belongs to worker `other`, not `ruby-1` `` |
| Report results only for tokens issued to it. | ``task token was issued to worker `other`, not `ruby-1` `` |

Under mutual TLS, present a client certificate signed by the CA given to
`--tls-client-ca`. The runtime matches its DNS SANs and subject common name
against `client_cert` entries. A valid certificate that matches no entry is
refused with `403 FORBIDDEN`. When a request carries both a valid token and a
certificate, the token decides. See [Security](../runtime/security.md).

## Endpoints

### Register a worker

`POST /v1/workers/register`

Opens a session. Call it at startup and whenever another call returns
`404 UNKNOWN_SESSION`. Every call opens a new session; a process normally
holds one. Several processes can register the same `worker_id`.

| Request field | Type | Required | Meaning |
|---|---|---|---|
| `worker_id` | string | yes | Stable identity, for example `payments-7f9c`. Must not be blank. |
| `runtime` | string | yes | Your language, matched case-insensitively against `using <language>` in steps. |
| `contracts` | array | yes, non-empty | The tasks you serve: objects with `task_name` and `contract_version`. |
| `sdk` | string | no | Informational, for example `orbit-ruby/0.1.0`. Shown by `GET /v1/workers`. |

| Response field | Type | Meaning |
|---|---|---|
| `accepted` | bool | Always `true` on success. |
| `worker_session_id` | string | Send it with every session call. |
| `heartbeat_interval_ms` | integer | How often to heartbeat: one sixth of the session timeout, at least 500. |
| `session_timeout_ms` | integer | Silence longer than this makes the runtime consider the worker lost. Set by `orbit server --session-timeout` (default 30 seconds). |
| `warnings` | array of strings | Problems that do not prevent registration, such as a contract version no registered workflow uses. Log them. |

| Status | Code | When |
|---|---|---|
| `200` | | Registered. |
| `400` | `INVALID_REQUEST` | `worker_id` is blank, or `contracts` is empty. |
| `400` | `RESERVED_TASK` | A task name starts with `orbit.` (a built-in task) or names a child workflow. |
| `403` | `FORBIDDEN` | A per-worker identity broke one of its rules. |
| `422` | | `worker_id`, `runtime` or `contracts` is missing. |

### Poll for a task

`POST /v1/workers/poll`

Waits for a task and returns at most one. Run one poll loop per task you want
to execute in parallel.

| Request field | Type | Required | Meaning |
|---|---|---|---|
| `worker_session_id` | string | yes | From registration. |
| `wait_ms` | integer | no | How long to wait for a task. Default `20000`, maximum `60000` (larger values are capped). |

The runtime answers as soon as a queued task matches the session, or with
`204 No Content` and an empty body when `wait_ms` passes first. Set your HTTP
client's read timeout above `wait_ms`; the SDKs use `wait_ms` plus 15
seconds.

A task matches when the session announced its exact `task_name` and
`contract_version`, and the step either has no `using`, says `using any`, or
names the session's runtime. Among matching tasks, the oldest scheduled one is
dispatched. Each dispatch issues a new `token`.

| Status | Code | When |
|---|---|---|
| `200` | | A task assignment, described below. |
| `204` | | No task arrived within `wait_ms`. Poll again. |
| `403` | `FORBIDDEN` | The session belongs to another per-worker identity. |
| `404` | `UNKNOWN_SESSION` | Register again, then poll with the new session. |

#### The task assignment

| Field | Type | Meaning |
|---|---|---|
| `token` | string | Identifies this delivery. Echo it in the result. |
| `execution_id` | string | The workflow execution. |
| `workflow` | string | Workflow name. |
| `workflow_version` | string | Workflow version. |
| `step_id` | string | The unit being executed: `<step>`, `<step>[<i>]` for a fan-out element, or `compensate:<unit>` for a compensation. |
| `task_name` | string | The task to run. |
| `contract_version` | string | The task's contract version. |
| `input` | any JSON value | The task input, already validated against the contract's input schema. |
| `idempotency_key` | string | Stable across retries and redeliveries of the step. Defaults to `<execution_id>/<step>`. |
| `attempt` | integer | 1-based attempt number. A redelivery keeps the number. |
| `redelivery` | bool | `true` when the outcome of an earlier delivery of this attempt is unknown. |
| `deadline_ms` | integer, optional | Absolute deadline of this attempt in Unix epoch milliseconds. Present only when the step has a `timeout`. |
| `timeout_ms` | integer, optional | The step's `timeout` in milliseconds. Present only when set. |

### Send a heartbeat

`POST /v1/workers/heartbeat`

Keeps the session alive and delivers cancellations. Send it every
`heartbeat_interval_ms`, from a thread or task that is independent of your
handlers.

| Request field | Type | Required | Meaning |
|---|---|---|---|
| `worker_session_id` | string | yes | From registration. |

| Response field | Type | Meaning |
|---|---|---|
| `accepted` | bool | Always `true` on success. |
| `cancel` | array of strings | Tokens of attempts this worker holds that it should stop. Each token is listed once. |

| Status | Code | When |
|---|---|---|
| `200` | | Heartbeat recorded. |
| `403` | `FORBIDDEN` | The session belongs to another per-worker identity. |
| `404` | `UNKNOWN_SESSION` | Register again. |

### Complete a task

`POST /v1/tasks/complete`

Reports a successful attempt.

| Request field | Type | Required | Meaning |
|---|---|---|---|
| `token` | string | yes, non-empty | The token from the task assignment. |
| `output` | JSON object | no | The task output. A missing or `null` output counts as `{}`. |

The runtime applies schema defaults to the output and validates it against
the contract's output schema. A mismatch fails the attempt with
`OUTPUT_CONTRACT_VIOLATION`, which is never retried, and the response has
`accepted: false`.

| Response field | Type | Meaning |
|---|---|---|
| `accepted` | bool | Whether the result changed the step. |
| `reason` | string, optional | Why the result was not accepted. Present only when `accepted` is `false`. |

| Status | Code | When |
|---|---|---|
| `200` | | The result was processed; check `accepted`. |
| `400` | `INVALID_REQUEST` | `token` is empty. |
| `403` | `FORBIDDEN` | The token was issued to another per-worker identity. |
| `422` | | `token` is missing. |

### Fail a task

`POST /v1/tasks/fail`

Reports a failed attempt.

| Request field | Type | Required | Default | Meaning |
|---|---|---|---|---|
| `token` | string | yes | | The token from the task assignment. |
| `error_code` | string | yes, non-empty | | Machine-readable code, for example `CARD_DECLINED`. |
| `error_message` | string | no | `""` | Human-readable message. |
| `retryable` | bool | no | `true` | `false` fails the step regardless of its retry policy. |
| `details` | any JSON value | no | none | Stored with the error in the execution history. |

The response has the same shape as for `complete`. The step's retry policy
then decides what happens: the runtime retries when the error is retryable,
its code is not in the step's `non_retryable` list (exact, case-sensitive
match), and attempts remain. See [Retries and timeouts](../language/retries-timeouts.md).

| Status | Code | When |
|---|---|---|
| `200` | | The result was processed; check `accepted`. |
| `400` | `INVALID_REQUEST` | `error_code` is empty. |
| `403` | `FORBIDDEN` | The token was issued to another per-worker identity. |
| `422` | | `token` or `error_code` is missing. |

### Unregister

`POST /v1/workers/unregister`

Closes the session. Attempts the session still holds are marked uncertain and
redelivered to other workers right away, instead of after the session
timeout. Finish or abandon your in-flight tasks before you unregister.

| Request field | Type | Required | Meaning |
|---|---|---|---|
| `worker_session_id` | string | yes | From registration. |

The response is `{"accepted": true}`. An unknown session is answered
`404 UNKNOWN_SESSION` with the message "unknown worker session".

### List workers

`GET /v1/workers` returns every worker that has registered, with its runtime,
SDK, contracts, last contact, whether it is online, and its number of live
sessions and in-flight tasks. It is an administrative endpoint: worker
credentials cannot call it. `orbit workers list` prints the same data.

## Result acceptance

The response to `complete` and `fail` is final. Do not retry a result that the
runtime answered, whether `accepted` is `true` or `false`.

| `accepted` | `reason` | Meaning |
|---|---|---|
| `true` | | The result was recorded. |
| `false` | `output violates the task contract` | The output failed validation. The attempt failed with `OUTPUT_CONTRACT_VIOLATION`. |
| `false` | `stale attempt` | A failure for an attempt that is no longer the one in flight. It was recorded and ignored. |
| `false` | `duplicate result; step already resolved` | The step already has an outcome. The result was recorded as `StaleResultIgnored`. |
| `false` | `unknown token (execution finished or token invalid)` | The execution is finished, or the token was never issued. |

The runtime reconciles results. A success for *any* token issued for a step
that is not yet resolved is accepted, because the work happened: a success
that arrives after its attempt timed out, or after the attempt was
redelivered to another worker, still completes the step. Failures count only
for the attempt currently in flight.

## Sessions, leases and liveness

There is no explicit lease to renew per task. A dispatched attempt belongs to
the session that polled it, and the session stays alive as long as the runtime
hears from it: every poll, heartbeat and result counts as contact.

- If a session is silent for longer than `session_timeout_ms`, the runtime
  marks its in-flight attempts uncertain and redelivers each to another
  worker, with the same `attempt` number, the same `idempotency_key`, and
  `redelivery: true`. A redelivery does not use up a retry.
- If the runtime restarts, every session is forgotten. In-flight attempts
  keep their tokens. The runtime waits one session timeout for workers to
  re-register and report results, then redelivers the attempts nobody
  reported.
- A step's `timeout` limits one attempt from dispatch to result. When
  `deadline_ms` passes, the runtime fails the attempt with `TIMEOUT`
  (retryable) and applies the retry policy, independently of the session.

Heartbeat from a separate thread so that a long-running handler does not let
the session expire.

## Cancellation

Cancellation is cooperative. When an attempt times out, or its workflow is
cancelled, times out or fails, the runtime lists the attempt's token in the
`cancel` array of the next heartbeat response to the session that holds it.
Stop the work if you can. The runtime may still record a result you send for
that token. For example, while a cancelled workflow waits for its in-flight
attempts, a late success is accepted and then compensated if the step has a
`compensate` block. Do not rely on either outcome; make the handler
idempotent.

## Rules for implementers

1. **Register on session loss.** On `404 UNKNOWN_SESSION` from `poll`,
   `heartbeat` or `unregister`, register again and retry the call with the new
   session. Tokens stay valid across sessions.
2. **Make result delivery survive outages.** Retry `complete` and `fail` on
   connection errors and 5xx responses, with backoff, until the runtime
   answers. A result from before a runtime restart is still accepted.
3. **Treat answers as final.** Do not retry a result the runtime answered,
   and do not retry 4xx responses.
4. **Make side effects idempotent.** Use `idempotency_key`, which is stable
   across retries and redeliveries.
5. **Heartbeat independently.** Send heartbeats every
   `heartbeat_interval_ms` from a thread that your handlers cannot block.
6. **Honour cancellations.** Stop work for tokens listed in `cancel`.
7. **Announce contracts exactly.** Use the `task_name` and
   `contract_version` from the `.orbit` files, and log registration
   `warnings`.
8. **Return objects.** Send a JSON object as `output`; the official SDKs fail
   an attempt with `INVALID_RESULT` (not retryable) when a handler returns
   anything else.
9. **Unregister on shutdown,** after in-flight tasks finish.

## gRPC binding

Start the runtime with `--grpc-addr` (or `ORBIT_GRPC_ADDR`), for example
`orbit server --grpc-addr 127.0.0.1:7234`, to also serve
`orbit.worker.v1.WorkerService` over HTTP/2. Generate a client from
`worker.proto` with your language's gRPC tooling. The server does not offer
gRPC reflection.

```proto
service WorkerService {
  rpc RegisterWorker(RegisterWorkerRequest) returns (RegisterWorkerResponse);
  rpc Heartbeat(HeartbeatRequest) returns (HeartbeatResponse);
  rpc PollTask(PollTaskRequest) returns (ExecuteTaskRequest);
  rpc CompleteTask(CompleteTaskRequest) returns (TaskResultAck);
  rpc FailTask(FailTaskRequest) returns (TaskResultAck);
  rpc UnregisterWorker(UnregisterWorkerRequest) returns (UnregisterWorkerResponse);
}
```

| RPC | HTTP equivalent | Request message | Response message |
|---|---|---|---|
| `RegisterWorker` | `POST /v1/workers/register` | `RegisterWorkerRequest` | `RegisterWorkerResponse` |
| `PollTask` | `POST /v1/workers/poll` | `PollTaskRequest` | `ExecuteTaskRequest` |
| `Heartbeat` | `POST /v1/workers/heartbeat` | `HeartbeatRequest` | `HeartbeatResponse` |
| `CompleteTask` | `POST /v1/tasks/complete` | `CompleteTaskRequest` | `TaskResultAck` |
| `FailTask` | `POST /v1/tasks/fail` | `FailTaskRequest` | `TaskResultAck` |
| `UnregisterWorker` | `POST /v1/workers/unregister` | `UnregisterWorkerRequest` | `UnregisterWorkerResponse` |

The messages have the same fields as the JSON bodies above. `input`,
`output` and `details` are `google.protobuf.Value`, and `deadline_ms` and
`timeout_ms` are `optional uint64`.

The gRPC binding shares the engine, credentials, per-worker identities and
authorization rules with HTTP. Its differences:

- **Credentials.** Send `authorization: Bearer <token>` as call metadata. With
  `--tls-cert`, `--tls-key` and `--tls-client-ca`, the gRPC port uses the same
  TLS and mutual TLS settings as the HTTP port.
- **No task.** When no task arrives within `wait_ms`, `PollTask` returns an
  `ExecuteTaskRequest` with an empty `token`. Treat an empty token as "poll
  again".
- **Defaults.** A `wait_ms` of `0` means the default of 20 seconds. Because
  proto3 booleans default to `false`, `FailTask` treats an unset `retryable`
  as `false`, unlike HTTP, where an omitted `retryable` is `true`. Always set
  it.
- **Numbers.** `google.protobuf.Value` carries every number as a double. The
  runtime converts whole numbers within ±2^53 back to integers, so `Int`
  fields round-trip.
- **Acknowledgements.** `TaskResultAck.reason` is an empty string when the
  result was accepted.
- **Errors.** Errors are gRPC statuses. The status message starts with the
  ORBIT error code, for example status `NOT_FOUND` with the message
  `UNKNOWN_SESSION: unknown or expired worker session; register again`. The
  code is also in the `orbit-error-code` response metadata, except for
  missing credentials (`UNAUTHENTICATED`) and an empty `token` or
  `error_code` (`INVALID_ARGUMENT`), where only the message prefix carries
  it.

| HTTP status | gRPC status |
|---|---|
| `400` | `INVALID_ARGUMENT` |
| `401` | `UNAUTHENTICATED` |
| `403` | `PERMISSION_DENIED` |
| `404` | `NOT_FOUND` |
| `409` | `FAILED_PRECONDITION` |
| other | `INTERNAL` |

A complete, tested gRPC client covering every RPC over plaintext and mutual
TLS is in
[`crates/orbit-runtime/tests/grpc.rs`](https://github.com/AjmalX0/Orbit/blob/main/crates/orbit-runtime/tests/grpc.rs).

## End-to-end example with curl

This transcript acts as a worker using nothing but `curl`. It was recorded
against a local runtime; ids are shortened, and long responses are trimmed with `…`. The workflow routes its step to
`using ruby`, a language without an official SDK:

```orbit
task "greeting.compose" {
    version "1.0.0"
    input { name: String }
    output { message: String }
}

workflow Greet {
    version "1.0.0"
    input { name: String }
    output { message: String }

    step compose {
        worker "greeting.compose"
        using ruby
        with { name: input.name }
        timeout 30s
        retry { attempts: 3, backoff: fixed, initial_delay: 1s, non_retryable: ["INVALID_NAME"] }
    }

    return { message: compose.message }
}
```

Start the runtime with an admin token and a worker token, and deploy the
workflow:

```console
$ orbit server --db /tmp/orbit-demo/orbit.db --token admin-secret --worker-token worker-secret &
$ ORBIT_TOKEN=admin-secret orbit deploy greet.orbit
deployed Greet@1.0.0 (sha256:0bfbb03e7820)
$ B=http://127.0.0.1:7233
$ J='content-type: application/json'
$ W='authorization: Bearer worker-secret'
$ A='authorization: Bearer admin-secret'
```

Register. The worker token can only call worker endpoints:

```console
$ curl -s $B/v1/workers/register -H "$J" -H "$W" -d '{
    "worker_id": "ruby-1", "runtime": "ruby", "sdk": "curl",
    "contracts": [{"task_name": "greeting.compose", "contract_version": "1.0.0"}]}'
{"accepted":true,"worker_session_id":"fb2bf1a7-…","heartbeat_interval_ms":5000,"session_timeout_ms":30000,"warnings":[]}
$ SESSION=fb2bf1a7-…

$ curl -si $B/v1/executions -H "$W"
HTTP/1.1 403 Forbidden
content-type: application/json

{"error":{"code":"FORBIDDEN","message":"worker credentials may only call /v1/workers/* and /v1/tasks/*"}}
```

Start an execution with the admin token, then poll for its task:

```console
$ curl -s $B/v1/executions -H "$J" -H "$A" \
    -d '{"workflow": "Greet", "input": {"name": "Ada"}, "execution_id": "greet-ada"}'
{"execution_id":"greet-ada","created":true}

$ curl -s $B/v1/workers/poll -H "$J" -H "$W" -d "{\"worker_session_id\": \"$SESSION\", \"wait_ms\": 20000}"
{"token":"5e9dc58e-…","execution_id":"greet-ada","workflow":"Greet","workflow_version":"1.0.0","step_id":"compose","task_name":"greeting.compose","contract_version":"1.0.0","input":{"name":"Ada"},"idempotency_key":"greet-ada/compose","attempt":1,"redelivery":false,"deadline_ms":1794736057501,"timeout_ms":30000}

$ curl -s $B/v1/workers/heartbeat -H "$J" -H "$W" -d "{\"worker_session_id\": \"$SESSION\"}"
{"accepted":true,"cancel":[]}
```

Fail the first attempt with a retryable error. The policy allows three
attempts, so after the one-second backoff the next poll returns attempt 2,
with a new token and the same idempotency key:

```console
$ curl -s $B/v1/tasks/fail -H "$J" -H "$W" -d '{
    "token": "5e9dc58e-…", "error_code": "GREETER_BUSY",
    "error_message": "try again later", "retryable": true, "details": {"queue_depth": 42}}'
{"accepted":true}

$ curl -s $B/v1/workers/poll -H "$J" -H "$W" -d "{\"worker_session_id\": \"$SESSION\", \"wait_ms\": 20000}"
{"token":"4d02a6e3-…","execution_id":"greet-ada","workflow":"Greet","workflow_version":"1.0.0","step_id":"compose","task_name":"greeting.compose","contract_version":"1.0.0","input":{"name":"Ada"},"idempotency_key":"greet-ada/compose","attempt":2,"redelivery":false,"deadline_ms":1794736058601,"timeout_ms":30000}
```

A failure reported now for the first token is stale:

```console
$ curl -s $B/v1/tasks/fail -H "$J" -H "$W" -d '{"token": "5e9dc58e-…", "error_code": "X"}'
{"accepted":false,"reason":"stale attempt"}
```

Complete attempt 2 and read the result:

```console
$ curl -s $B/v1/tasks/complete -H "$J" -H "$W" -d '{"token": "4d02a6e3-…", "output": {"message": "Hello, Ada"}}'
{"accepted":true}

$ curl -s "$B/v1/executions/greet-ada/wait?timeout_ms=5000" -H "$A"
{"id":"greet-ada","workflow":"Greet","version":"1.0.0",…,"status":"COMPLETED",…,"output":{"message":"Hello, Ada"},…}
```

With nothing queued, a poll returns `204` when `wait_ms` passes:

```console
$ curl -si $B/v1/workers/poll -H "$J" -H "$W" -d "{\"worker_session_id\": \"$SESSION\", \"wait_ms\": 1000}"
HTTP/1.1 204 No Content
x-orbit-api-version: orbit-api/1
```

A non-retryable code from the step's `non_retryable` list fails the step on
the first attempt, even though `retryable` defaults to `true`:

```console
$ curl -s $B/v1/executions -H "$J" -H "$A" -d '{"workflow": "Greet", "input": {"name": ""}, "execution_id": "greet-blank"}'
{"execution_id":"greet-blank","created":true}
$ # poll as above, then:
$ curl -s $B/v1/tasks/fail -H "$J" -H "$W" -d '{"token": "…", "error_code": "INVALID_NAME", "error_message": "name is blank"}'
{"accepted":true}
```

The execution ends `FAILED` after one attempt, with the error
``INVALID_NAME: step `compose` failed: name is blank``.

Output that violates the contract is refused and fails the step:

```console
$ curl -s $B/v1/tasks/complete -H "$J" -H "$W" -d '{"token": "…", "output": {"msg": 1}}'
{"accepted":false,"reason":"output violates the task contract"}
```

The execution then fails with `OUTPUT_CONTRACT_VIOLATION`:
``worker output for `greeting.compose` does not match its contract at $.message: missing required field `message` of type String``.

When the workflow is cancelled while the worker holds a task, the next
heartbeat lists the token:

```console
$ curl -s -X POST $B/v1/executions/greet-cancel/cancel -H "$J" -H "$A" -d '{"reason": "customer withdrew"}'
{"id":"greet-cancel",…,"status":"RUNNING",…}
$ curl -s $B/v1/workers/heartbeat -H "$J" -H "$W" -d "{\"worker_session_id\": \"$SESSION\"}"
{"accepted":true,"cancel":["5f748944-…"]}
```

When a worker unregisters while it holds a task, the task is redelivered with
the same attempt number and `redelivery: true`. A success for the original
token is still accepted, because the step is not yet resolved:

```console
$ curl -s $B/v1/workers/unregister -H "$J" -H "$W" -d "{\"worker_session_id\": \"$SESSION\"}"
{"accepted":true}
$ # register again as SESSION2, then poll:
{"token":"f26ec7a5-…","execution_id":"greet-redeliver",…,"idempotency_key":"greet-redeliver/compose","attempt":1,"redelivery":true,…}
$ curl -s $B/v1/tasks/complete -H "$J" -H "$W" -d '{"token": "d0917fff-…", "output": {"message": "Hello, Di"}}'
{"accepted":true}
```

Calls with an unregistered session return `404`:

```console
$ curl -si $B/v1/workers/heartbeat -H "$J" -H "$W" -d "{\"worker_session_id\": \"$SESSION\"}"
HTTP/1.1 404 Not Found
content-type: application/json
x-orbit-api-version: orbit-api/1

{"error":{"code":"UNKNOWN_SESSION","message":"unknown or expired worker session; register again"}}
```

## A worker loop in pseudocode

```text
session, hb = register()
spawn every hb.heartbeat_interval_ms:
    r = heartbeat(session)            # on 404: session = register()
    for token in r.cancel: signal_cancel(token)

repeat N times in parallel:
    loop until shutting down:
        task = poll(session, wait_ms=20000)   # on 404: session = register(); retry
        if no task (204): continue
        try:
            output = handler(task.input, task)
            send_with_retry(complete, token=task.token, output=output)
        except TaskError e:
            send_with_retry(fail, token=task.token, error_code=e.code,
                            error_message=e.message, retryable=e.retryable, details=e.details)
        except any other error e:
            send_with_retry(fail, token=task.token, error_code="UNHANDLED_EXCEPTION",
                            error_message=str(e), retryable=true)

on shutdown: stop polling, wait for handlers, unregister(session)

send_with_retry: retry on connection errors and 5xx with exponential backoff;
                 stop on any other answer.
```

## See also

- [How workers work](../sdks/overview.md)
- [HTTP API](http-api.md)
- [Security](../runtime/security.md)
- [Delivery guarantees](../runtime/guarantees.md)
- [Retries and timeouts](../language/retries-timeouts.md)
- [Idempotency](../language/idempotency.md)
- [Environment variables](environment.md)
