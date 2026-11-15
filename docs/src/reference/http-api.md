# HTTP API

The runtime serves one HTTP API on `--addr` (default `127.0.0.1:7233`). It contains the administrative endpoints, used by the `orbit` CLI and your applications to register workflows and manage executions, and the HTTP/JSON binding of the worker protocol, used by the SDKs. This page lists every route with its authentication requirement, request and response bodies, status codes and error codes. The examples are real responses from `orbit server` with [`examples/order_processing.orbit`](https://github.com/AjmalX0/Orbit/blob/main/examples/order_processing.orbit) deployed; long values are shortened with `...`.

## Conventions

- Request and response bodies are JSON (`content-type: application/json`). Requests with a body must send that content type.
- Timestamps are milliseconds since the Unix epoch (`*_ms` fields).
- Every response that passes authentication carries the header `x-orbit-api-version: orbit-api/1`.
- The API is versioned by path (`/v1`). Fields may be added to responses within `orbit-api/1`; clients should ignore fields they do not know.

## Authentication

When the server runs without `--token` and without `--workers-file`, every endpoint is open. This is only allowed on a loopback address unless you pass `--insecure`.

When a credential is configured, every endpoint except `GET /v1/health` and `GET /v1/ready` requires one of:

| Caller | How it authenticates | Can call |
|---|---|---|
| Admin | `Authorization: Bearer <--token>` | Every endpoint. |
| Shared worker | `Authorization: Bearer <--worker-token>` | `/v1/workers/*` and `/v1/tasks/*` only. No identity restrictions. |
| Per-worker identity | `Authorization: Bearer <token>` whose SHA-256 is in a `[[worker]]` entry of the workers file, or a client certificate (mutual TLS) matching its `client_cert` | `/v1/workers/*` and `/v1/tasks/*` only, restricted to its own worker id, task patterns, sessions and task tokens. |
| Client identity | `Authorization: Bearer <token>` whose SHA-256 is in a `[[client]]` entry of the workers file, or a client certificate matching its `client_cert` | The execution API (`/v1/executions...`, and `GET /v1/workflows...`), for the workflows and actions the entry grants. |

`GET /v1/workers` (the worker list) is an administrative endpoint: the `/v1/workers/*` prefix covers only the paths below it. A request with a valid client certificate that matches no worker or client in the workers file is refused with `403` on every path, including `/v1/health`. See [Security](../runtime/security.md) for the full model.

A client identity is checked per route:

| Route | Needs the action |
|---|---|
| `POST /v1/executions` | `start` for the workflow in the body |
| `POST /v1/executions/{id}/signals/{name}` | `signal` |
| `GET /v1/executions/{id}/queries/{name}` | `query` |
| `GET /v1/executions/{id}`, `/history`, `/wait`, `GET /v1/workflows/{name}...` | `read` |
| `POST /v1/executions/{id}/cancel` | `cancel` |
| `GET /v1/executions`, `GET /v1/workflows` | none: the list contains only workflows the client may `read` |
| `POST /v1/workflows`, `GET /v1/workers`, `/v1/workers/*`, `/v1/tasks/*`, `GET /v1/metrics` | refused |

For routes with an execution id, the workflow is the execution's workflow; an id that does not exist is `404 NOT_FOUND`. See [Security: client identities](../runtime/security.md#client-identities).

```console
$ curl -s https://orbit.internal:7233/v1/executions
{"error":{"code":"UNAUTHENTICATED","message":"missing or invalid credentials"}}
$ curl -s -H "authorization: Bearer $ORBIT_WORKER_TOKEN" https://orbit.internal:7233/v1/executions
{"error":{"code":"FORBIDDEN","message":"worker credentials may only call /v1/workers/* and /v1/tasks/*"}}
$ curl -s -H "authorization: Bearer $WAREHOUSE_TOKEN" https://orbit.internal:7233/v1/metrics
{"error":{"code":"FORBIDDEN","message":"client `warehouse-app` may only use the execution API (start, signal, query, read, cancel)"}}
$ curl -s -X POST -H "authorization: Bearer $WAREHOUSE_TOKEN" https://orbit.internal:7233/v1/executions/labels-1/cancel
{"error":{"code":"FORBIDDEN","message":"client `warehouse-app` may not cancel executions of `PrintLabels`"}}
```

## Errors

Errors produced by ORBIT have this body:

```json
{ "error": { "code": "WORKFLOW_NOT_FOUND", "message": "workflow Nope is not registered" } }
```

`code` is stable and meant for programs; `message` is for people and may change. The HTTP status and the code always agree as listed below.

| Code | Status | Returned by | Meaning |
|---|---|---|---|
| `INVALID_REQUEST` | 400 | worker register, task complete/fail | A required field is missing or empty: `worker_id is required`, `a worker must announce at least one task contract`, `token is required`, `error_code is required`. |
| `INVALID_IR` | 400 | `POST /v1/workflows` | The body is not valid IR in a supported version (`orbit-ir/1` or `orbit-ir/2`): an unsupported `ir_version`, a missing field, a failed validation, a missing `definition_hash`, or a `definition_hash` that does not match the content. |
| `CHILD_NOT_REGISTERED` | 400 | `POST /v1/workflows` | The definition starts a child workflow whose exact definition is not registered. Register the child first; `orbit deploy` does this. |
| `INVALID_INPUT` | 400 | `POST /v1/executions` | The workflow input does not match the workflow's `input` type. The message names the path, for example `$.order_id`. |
| `INVALID_EXECUTION_ID` | 400 | `POST /v1/executions` | `execution_id` is empty, longer than 200 characters, or contains characters other than letters, digits, `-`, `_`, `.` and `:`. |
| `RESERVED_TASK` | 400 | `POST /v1/workers/register` | A worker announced a built-in task (`orbit.*`) or a child-workflow contract (`workflow:*`). |
| `UNAUTHENTICATED` | 401 | any authenticated endpoint | No credential, or a credential that matches nothing. |
| `FORBIDDEN` | 403 | any authenticated endpoint | The credential is valid but not allowed: a worker credential on an administrative endpoint, a per-worker identity acting for another worker, announcing a task outside its patterns or the wrong runtime, using another worker's session or token, a client identity outside the execution API or its granted workflows and actions, or an unmatched client certificate. |
| `NOT_FOUND` | 404 | execution endpoints | The execution does not exist. |
| `UNKNOWN_SIGNAL` | 404 | `POST /v1/executions/{id}/signals/{name}` | The execution's workflow does not declare this signal. The message lists the declared signals. |
| `UNKNOWN_QUERY` | 404 | `GET /v1/executions/{id}/queries/{name}` | The execution's workflow does not declare this query. The message lists the declared queries. |
| `WORKFLOW_NOT_FOUND` | 404 | workflow endpoints, `POST /v1/executions` | No such workflow, or no such version. |
| `UNKNOWN_SESSION` | 404 | heartbeat, poll, unregister | The worker session does not exist or has expired. Register again. |
| `DEFINITION_CONFLICT` | 409 | `POST /v1/workflows` | A different definition is already registered under this name and version. Definitions are immutable; bump the version. |
| `EXECUTION_EXISTS` | 409 | `POST /v1/executions` | An execution with this `execution_id` exists for a different workflow definition. |
| `EXECUTION_FINISHED` | 409 | `POST /v1/executions/{id}/cancel`, `POST /v1/executions/{id}/signals/{name}` | The execution is already terminal. |
| `PAYLOAD_TOO_LARGE` | 413 | `POST /v1/executions`, `POST /v1/executions/{id}/signals/{name}`, `POST /v1/tasks/complete` | The execution input, signal payload or task result is larger than `--max-payload-bytes`, as compact JSON. Nothing is recorded. See [Resource limits](../runtime/server.md#resource-limits). |
| `SIGNAL_PAYLOAD_INVALID` | 422 | `POST /v1/executions/{id}/signals/{name}` | The payload does not match the signal's declaration. The message names the path, for example `$.approved`. Nothing is recorded. |
| `QUERY_FAILED` | 422 | `GET /v1/executions/{id}/queries/{name}` | Evaluating the query failed at runtime, for example with a division by zero. |
| `RESOURCE_EXHAUSTED` | 429 | `POST /v1/executions` | `--max-active-executions` executions are already running. Nothing is started; retry later with the same `execution_id`. |
| `STORAGE_ERROR` | 500 | any endpoint that reads or writes the database | The database operation failed. The message contains the backend error. Nothing was changed. On a cluster leader that was just deposed, the message starts with `fenced:`. |
| `NOT_LEADER` | 503 | every route except `GET` routes, on a cluster standby | This server is a standby (`--cluster`). The body's `error` object adds `leader_node` and `leader_addr` (either may be `null`), the `x-orbit-leader` header carries the address, and `retry-after: 1` is set. Send the request to the leader, or retry. See [Running several servers](../runtime/cluster.md#standby-behavior). |

Some errors are produced by the HTTP layer before a request reaches ORBIT. They have a plain-text body (`content-type: text/plain`), not the JSON envelope:

| Status | When | Example body |
|---|---|---|
| 400 | The body is not valid JSON, or a query parameter has the wrong type. | `Failed to parse the request body as JSON: expected ident at line 1 column 2` |
| 404 | Unknown path. The body is empty. | |
| 405 | Known path, wrong method. The `allow` header lists the method. | |
| 413 | The body is larger than `--max-body`. (A value larger than `--max-payload-bytes` gets the JSON `PAYLOAD_TOO_LARGE` instead.) | `Failed to buffer the request body: length limit exceeded` |
| 415 | A body was sent without `content-type: application/json`. | |
| 422 | Valid JSON with a missing or mistyped field. | ``Failed to deserialize the JSON body into the target type: missing field `workflow` at line 1 column 12`` |

A client should check the status first, and parse the JSON error envelope only when the body is JSON.

## Routes

| Method | Path | Auth | Purpose |
|---|---|---|---|
| `GET` | [`/v1/health`](#get-v1health) | none | Liveness, versions and cluster role. |
| `GET` | [`/v1/ready`](#get-v1ready) | none | Readiness: `200` only where writes are served. |
| `GET` | [`/v1/metrics`](#get-v1metrics) | admin | Prometheus metrics. |
| `POST` | [`/v1/workflows`](#post-v1workflows) | admin | Register a compiled definition. |
| `GET` | [`/v1/workflows`](#get-v1workflows) | admin | List definitions. |
| `GET` | [`/v1/workflows/{name}`](#get-v1workflowsname) | admin | Latest version of a definition. |
| `GET` | [`/v1/workflows/{name}/{version}`](#get-v1workflowsnameversion) | admin | One version of a definition. |
| `POST` | [`/v1/executions`](#post-v1executions) | admin | Start an execution. |
| `GET` | [`/v1/executions`](#get-v1executions) | admin | List executions. |
| `GET` | [`/v1/executions/{id}`](#get-v1executionsid) | admin | Current state of an execution. |
| `GET` | [`/v1/executions/{id}/history`](#get-v1executionsidhistory) | admin | Event history. |
| `GET` | [`/v1/executions/{id}/wait`](#get-v1executionsidwait) | admin | Long-poll until terminal. |
| `POST` | [`/v1/executions/{id}/cancel`](#post-v1executionsidcancel) | admin | Request cancellation. |
| `POST` | [`/v1/executions/{id}/signals/{name}`](#post-v1executionsidsignalsname) | admin | Send a typed signal. |
| `GET` | [`/v1/executions/{id}/queries/{name}`](#get-v1executionsidqueriesname) | admin | Evaluate a declared query. |
| `GET` | [`/v1/workers`](#get-v1workers) | admin | Registered workers. |
| `POST` | [`/v1/workers/register`](#post-v1workersregister) | worker | Open a worker session. |
| `POST` | [`/v1/workers/heartbeat`](#post-v1workersheartbeat) | worker | Keep a session alive; receive cancellations. |
| `POST` | [`/v1/workers/poll`](#post-v1workerspoll) | worker | Long-poll for a task. |
| `POST` | [`/v1/workers/unregister`](#post-v1workersunregister) | worker | Close a session. |
| `POST` | [`/v1/tasks/complete`](#post-v1taskscomplete) | worker | Report a successful result. |
| `POST` | [`/v1/tasks/fail`](#post-v1tasksfail) | worker | Report a failed attempt. |

"admin" means the admin token when authentication is enabled; routes under `/v1/executions` and `GET /v1/workflows...` also accept a client identity with the matching action (see [Authentication](#authentication)). "worker" means any credential: admin, shared worker token or a per-worker identity.

## Health and metrics

### GET /v1/health

Liveness and version information. It never requires authentication. A single server binds its listener only after it has opened the database and recovered active executions, so a `200` also means the runtime is ready to serve. In cluster mode every server, leader or standby, answers `200`; use [`/v1/ready`](#get-v1ready) to find the leader.

Response `200`:

```json
{
  "status": "ok",
  "api_version": "orbit-api/1",
  "runtime_version": "0.2.0",
  "ir_versions": ["orbit-ir/1", "orbit-ir/2"],
  "active_executions": 0,
  "worker_sessions": 0,
  "role": "single",
  "node_id": null,
  "term": null,
  "leader": null
}
```

A cluster standby:

```json
{"status":"ok","api_version":"orbit-api/1","runtime_version":"0.2.0","ir_versions":["orbit-ir/1","orbit-ir/2"],"active_executions":0,"worker_sessions":0,"role":"standby","node_id":"b","term":1,"leader":{"node_id":"a","addr":"http://127.0.0.1:7301"}}
```

| Field | Type | Meaning |
|---|---|---|
| `status` | string | Always `ok`. |
| `api_version` | string | `orbit-api/1`. |
| `runtime_version` | string | Version of the `orbit` binary. |
| `ir_versions` | array of strings | IR versions this runtime accepts. ORBIT 0.1 lists only `orbit-ir/1`; workflows that use signals, queries, `await` or `compute` steps, or lambdas need a runtime that lists `orbit-ir/2`. |
| `active_executions` | integer | Executions not yet terminal, held in memory. |
| `worker_sessions` | integer | Worker sessions the runtime remembers, including ones that recently went silent. |
| `role` | string | `single` without `--cluster`; `leader` or `standby` in cluster mode. |
| `node_id` | string or null | The server's `--node-id`; `null` without `--cluster`. |
| `term` | integer or null | The leadership term this server last saw; `null` without `--cluster`. |
| `leader` | object or null | The current leader as `{"node_id": ..., "addr": ...}`, or `null`. |

### GET /v1/ready

Readiness for load balancers and orchestrators. It never requires authentication. It returns `200` on a single server and on a cluster leader, and `503` on a cluster standby, so a load balancer that checks it sends traffic only to the server that accepts writes.

Response `200` (single server):

```json
{"role":"single","ready":true}
```

Response `503` (cluster standby):

```json
{"role":"standby","node_id":"b","term":1,"leader":{"node_id":"a","addr":"http://127.0.0.1:7301"},"ready":false}
```

A leader that cannot confirm its leadership within its lease (3 seconds) answers `503` until it can. See [Running several servers](../runtime/cluster.md#load-balancer-configuration).

### GET /v1/metrics

Prometheus text exposition (`content-type: text/plain; version=0.0.4`). [Observability](../runtime/observability.md#prometheus-metrics) lists every metric.

```text
# HELP orbit_executions_started_total Executions started
# TYPE orbit_executions_started_total counter
orbit_executions_started_total 8
...
# HELP orbit_active_executions Executions not yet finished
# TYPE orbit_active_executions gauge
orbit_active_executions 0
```

## Workflow definitions

### POST /v1/workflows

Register a compiled workflow definition: the `orbit-ir/1` or `orbit-ir/2` JSON produced by `orbit compile`. Registering identical content again is a no-op.

Request body: the IR document. Abbreviated:

```json
{
  "ir_version": "orbit-ir/1",
  "name": "OrderProcessing",
  "version": "1.0.0",
  "description": "Validate, risk-score, reserve, charge and notify for one order",
  "definition_hash": "sha256:4fdfac082bf3f2fbbbc8ac5465d37cba01dffd1b8e4ae08e25be3e0d76486c41",
  "timeout_ms": 300000,
  "input": { "kind": "record", "fields": [ ... ] },
  "output": { ... },
  "steps": [ ... ],
  "result": { ... },
  "contracts": { ... },
  "source_map": { ... }
}
```

Responses:

| Status | Body | When |
|---|---|---|
| `201` | `{"name", "version", "definition_hash", "created": true}` | The definition is new. |
| `200` | the same, with `"created": false` | An identical definition is already registered. |
| `400` | `INVALID_IR`, `CHILD_NOT_REGISTERED` | |
| `409` | `DEFINITION_CONFLICT` | |

```json
{"name":"OrderProcessing","version":"1.0.0","definition_hash":"sha256:4fdfac082bf3f2fbbbc8ac5465d37cba01dffd1b8e4ae08e25be3e0d76486c41","created":false}
```

```json
{"error":{"code":"INVALID_IR","message":"definition_hash does not match the IR content (was the file modified?)"}}
```

### GET /v1/workflows

List every registered definition (all versions), sorted by name, then by version string.

Response `200`:

```json
{
  "workflows": [
    {
      "name": "OrderProcessing",
      "version": "1.0.0",
      "description": "Validate, risk-score, reserve, charge and notify for one order",
      "definition_hash": "sha256:4fdfac082bf3f2fbbbc8ac5465d37cba01dffd1b8e4ae08e25be3e0d76486c41",
      "steps": ["validate_order", "analyze_risk", "reserve_inventory", "process_payment", "send_notification"],
      "tasks": ["inventory.release", "inventory.reserve", "notifications.send", "orders.validate",
                "payments.charge", "payments.refund", "risk.analyze"]
    }
  ]
}
```

`description` is `null` when the workflow has none. `tasks` lists the contract keys the definition uses, including built-in tasks and child workflows (`workflow:<Name>`).

### GET /v1/workflows/{name} {#get-v1workflowsname}

The full IR of the highest registered version of `name`, ordered by semantic version. Response `200`: the IR document. Error `404 WORKFLOW_NOT_FOUND` (`workflow not found`).

### GET /v1/workflows/{name}/{version} {#get-v1workflowsnameversion}

The full IR of one version. Response `200`: the IR document. Error `404 WORKFLOW_NOT_FOUND`.

## Executions

### POST /v1/executions

Start an execution.

| Field | Type | Required | Meaning |
|---|---|---|---|
| `workflow` | string | yes | Workflow name. |
| `version` | string | no | Version to start. Default: the highest registered version. |
| `input` | any | no | Workflow input. `null` or absent means `{}`. Defaults declared in the workflow's `input` are applied, then the input is validated strictly. |
| `execution_id` | string | no | Client-chosen id. Default: a random UUID. Starting an id that already exists for the same definition returns the existing execution. |

```json
{
  "workflow": "OrderProcessing",
  "version": "1.0.0",
  "execution_id": "order-1001",
  "input": {"order_id": "1001", "customer_id": "c-42", "amount": 99.5, "items": [{"sku": "BOOK-1", "quantity": 1}]}
}
```

Responses:

| Status | Body | When |
|---|---|---|
| `201` | `{"execution_id": "order-1001", "created": true}` | A new execution was started. |
| `200` | `{"execution_id": "order-1001", "created": false}` | The id exists for the same definition. Nothing was started. |
| `400` | `INVALID_INPUT`, `INVALID_EXECUTION_ID` | |
| `404` | `WORKFLOW_NOT_FOUND` | |
| `409` | `EXECUTION_EXISTS` | The id exists for a different definition. |
| `413` | `PAYLOAD_TOO_LARGE` | The input, with defaults applied, is larger than `--max-payload-bytes`. |
| `429` | `RESOURCE_EXHAUSTED` | `--max-active-executions` is reached. An id that already exists still gets `200`. |

```json
{"error":{"code":"INVALID_INPUT","message":"workflow input is invalid at $.order_id: expected String, found an integer"}}
{"error":{"code":"WORKFLOW_NOT_FOUND","message":"workflow OrderProcessing@9.9.9 is not registered"}}
{"error":{"code":"EXECUTION_EXISTS","message":"execution `order-1001` already exists for a different workflow"}}
{"error":{"code":"PAYLOAD_TOO_LARGE","message":"execution input is 414 bytes; the limit is 200 (--max-payload-bytes)"}}
{"error":{"code":"RESOURCE_EXHAUSTED","message":"2 executions are already running (--max-active-executions)"}}
```

### GET /v1/executions

List executions, newest first.

| Query parameter | Type | Default | Meaning |
|---|---|---|---|
| `status` | string | all | Filter by status, case-insensitive (`failed` matches `FAILED`). |
| `workflow` | string | all | Filter by workflow name. |
| `limit` | integer | `50` | Maximum rows, clamped to 1 through 1000. |

For a client identity, rows of workflows it may not `read` are removed after the limit is applied, so it can receive fewer rows than `limit`; filter by `workflow` to get full pages.

Response `200`:

```json
{
  "executions": [
    {
      "execution_id": "order-1002",
      "workflow": "OrderProcessing",
      "version": "1.0.0",
      "status": "FAILED",
      "created_at_ms": 1794735367702,
      "updated_at_ms": 1794735369299
    }
  ]
}
```

### GET /v1/executions/{id} {#get-v1executionsid}

The current state of an execution, derived from its history. Error `404 NOT_FOUND`.

Response `200` (execution `order-1002`, which failed and was compensated; some units shortened):

```json
{
  "id": "order-1002",
  "workflow": "OrderProcessing",
  "version": "1.0.0",
  "definition_hash": "sha256:4fdfac082bf3f2fbbbc8ac5465d37cba01dffd1b8e4ae08e25be3e0d76486c41",
  "input": {"order_id": "1002", "customer_id": "c-42", "amount": 99.5,
            "items": [{"sku": "BOOK-1", "quantity": 1}], "currency": "USD"},
  "status": "FAILED",
  "created_at_ms": 1794735367702,
  "updated_at_ms": 1794735369299,
  "finished_at_ms": 1794735369299,
  "units": {
    "process_payment": {
      "status": "FAILED",
      "attempt": 1,
      "input": {"order_id": "1002", "amount": 99.5, "currency": "USD"},
      "idempotency_key": "charge-1002",
      "error": {"code": "CARD_DECLINED", "message": "card was declined", "retryable": false,
                "details": {"decline_code": "insufficient_funds"}},
      "last_error": {"code": "CARD_DECLINED", "message": "card was declined", "retryable": false,
                     "details": {"decline_code": "insufficient_funds"}},
      "redelivery": false,
      "scheduled_at_ms": 1794735368974,
      "dispatched_at_ms": 1794735369286,
      "deadline_ms": 1794735399286,
      "finished_at_ms": 1794735369290,
      "worker_id": "py-1",
      "worker_session": "30e57815-e28e-4a2b-9c35-df96774668ae",
      "tokens": [["3d932a10-600d-4989-acc7-5234a3183b06", 1]],
      "dispatch_count": 1
    },
    "reserve_inventory": {"status": "SUCCEEDED", "attempt": 1, "output": {"reservation_id": "res-77"}, "...": "..."},
    "compensate:reserve_inventory": {"status": "SUCCEEDED", "attempt": 1, "output": {"released": true}, "...": "..."},
    "send_notification": {"status": "CANCELLED", "attempt": 0, "redelivery": false,
                          "finished_at_ms": 1794735369290, "dispatch_count": 0}
  },
  "abort": {"kind": "failed", "error": {"code": "CARD_DECLINED",
            "message": "step `process_payment` failed: card was declined", "retryable": false,
            "details": {"decline_code": "insufficient_funds"}}},
  "compensation_order": ["reserve_inventory"],
  "compensation": "completed",
  "error": {"code": "CARD_DECLINED", "message": "step `process_payment` failed: card was declined",
            "retryable": false, "details": {"decline_code": "insufficient_funds"}},
  "last_seq": 27
}
```

Execution fields:

| Field | Type | Meaning |
|---|---|---|
| `id` | string | Execution id. |
| `parent` | object, optional | For a child workflow: `{"execution_id", "step"}` of the parent unit that started it. |
| `workflow`, `version`, `definition_hash` | string | The definition the execution is pinned to. |
| `input` | any | Input after defaults were applied. |
| `status` | string | `RUNNING`, `COMPENSATING`, `COMPLETED`, `FAILED`, `CANCELLED` or `TIMED_OUT`. |
| `created_at_ms`, `updated_at_ms` | integer | First and latest event times. |
| `finished_at_ms` | integer, optional | Set when terminal. |
| `units` | object | Unit state by unit key (`step`, `step[i]`, `compensate:<unit>`). Only units that have been touched appear. |
| `abort` | object, optional | Why the execution is being wound down: `{"kind": "failed", "error"}`, `{"kind": "cancelled", "reason"}` or `{"kind": "timed_out"}`. |
| `compensation_order` | array, optional | Units being compensated, in order. |
| `signals` | array, optional | The signal buffer: every signal received, in arrival order. Absent when none was received. See below. |
| `compensation` | string, optional | `none`, `completed` or `failed`. Set when terminal after an abort. |
| `output` | any, optional | Workflow output, when `COMPLETED`. |
| `error` | object, optional | `{"code", "message", "retryable", "details"?}`, when `FAILED`. |
| `last_seq` | integer | Sequence number of the latest event. |

Unit fields:

| Field | Type | Meaning |
|---|---|---|
| `status` | string | See [Unit statuses](../runtime/executions.md#unit-statuses). Absent means `PENDING`. |
| `attempt` | integer | Current attempt number, starting at 1. `0` for a unit that never started. |
| `input` | any, optional | Evaluated input. For a `for` step: the list of items. |
| `idempotency_key` | string, optional | Key sent to workers. |
| `output` | any, optional | Result of a succeeded unit. `null` for a skipped one. |
| `error` | object, optional | Final error of a failed unit. |
| `last_error` | object, optional | Error of the latest failed attempt. |
| `redelivery` | boolean | The current attempt is a redelivery of an uncertain one. |
| `scheduled_at_ms`, `dispatched_at_ms`, `finished_at_ms` | integer, optional | Transition times. |
| `deadline_ms` | integer, optional | When the in-flight attempt times out. |
| `wake_at_ms` | integer, optional | When a `sleep` timer fires, a retry becomes due, or an `await` step times out. |
| `worker_id`, `worker_session` | string, optional | Who holds or held the latest dispatch. Child workflows show `orbit-runtime` and `child:<child id>`; built-in tasks `orbit-runtime` and `builtin:<instance>`. |
| `token` | string, optional | Token of the in-flight dispatch. |
| `tokens` | array, optional | Every `[token, attempt]` issued for the unit. |
| `succeeded_seq` | integer, optional | Sequence number of the success, used to order compensation. |
| `dispatch_count` | integer | Number of dispatches, including redeliveries. |

Signal fields (`signals[]`):

| Field | Type | Meaning |
|---|---|---|
| `seq` | integer | Sequence number of the `SignalReceived` event. Identifies the signal. |
| `name` | string | Signal name. |
| `payload` | object | Validated payload, with defaults applied. |
| `received_at_ms` | integer | When the signal was recorded. |
| `delivered_to` | string, optional | The `await` unit that took the signal. Absent while it is buffered. |

```json
"signals": [
  {"seq": 5, "name": "review", "payload": {"approved": true, "reviewer": "dana", "comment": "verified by phone"},
   "received_at_ms": 1794746714669, "delivered_to": "decision"}
]
```

### GET /v1/executions/{id}/history {#get-v1executionsidhistory}

The full event history, in commit order. Error `404 NOT_FOUND`. [Executions and history](../runtime/executions.md#history-events) describes every event type.

Response `200` (first and last events of `order-1002`):

```json
{
  "execution_id": "order-1002",
  "events": [
    {"seq": 1, "timestamp_ms": 1794735367702, "type": "WorkflowCreated", "workflow": "OrderProcessing",
     "version": "1.0.0", "definition_hash": "sha256:4fdf...6c41",
     "input": {"order_id": "1002", "customer_id": "c-42", "amount": 99.5,
               "items": [{"sku": "BOOK-1", "quantity": 1}], "currency": "USD"}},
    {"seq": 2, "timestamp_ms": 1794735367702, "type": "WorkflowStarted"},
    {"seq": 3, "timestamp_ms": 1794735367703, "type": "StepScheduled", "step": "validate_order", "attempt": 1,
     "input": {"order_id": "1002", "amount": 99.5, "items": [{"sku": "BOOK-1", "quantity": 1}]},
     "idempotency_key": "order-1002/validate_order", "redelivery": false},
    "...",
    {"seq": 26, "timestamp_ms": 1794735369299, "type": "CompensationCompleted"},
    {"seq": 27, "timestamp_ms": 1794735369299, "type": "WorkflowFailed",
     "error": {"code": "CARD_DECLINED", "message": "step `process_payment` failed: card was declined",
               "retryable": false, "details": {"decline_code": "insufficient_funds"}},
     "compensation": "completed"}
  ]
}
```

The history endpoint returns values unredacted. `orbit logs --json` redacts them.

### GET /v1/executions/{id}/wait {#get-v1executionsidwait}

Long-poll until the execution is terminal, or until the timeout passes, then return the same body as `GET /v1/executions/{id}`. Check `status` to tell the two apart.

| Query parameter | Type | Default | Meaning |
|---|---|---|---|
| `timeout_ms` | integer | `30000` | Maximum wait, capped at `300000`. |

Error `404 NOT_FOUND`.

### POST /v1/executions/{id}/cancel {#post-v1executionsidcancel}

Request cancellation. The body is optional.

| Field | Type | Default | Meaning |
|---|---|---|---|
| `reason` | string | `requested via API` | Recorded in `CancelRequested`. |

Response `200`: the execution state right after the request. Requesting cancellation of an execution that is already being aborted (by an earlier cancel, a failure or a timeout) records nothing and returns the current state.

```json
{"id":"order-2001","workflow":"OrderProcessing","version":"1.0.0","...":"...","status":"CANCELLED","compensation":"none","...":"..."}
```

Errors: `404 NOT_FOUND`; `409 EXECUTION_FINISHED`:

```json
{"error":{"code":"EXECUTION_FINISHED","message":"execution is already CANCELLED"}}
```

### POST /v1/executions/{id}/signals/{name} {#post-v1executionsidsignalsname}

Send the signal `name` to an execution. The body is the payload, a JSON object; a request without a body sends `{}`. The runtime applies the declared defaults, validates the payload against the workflow's `signal` declaration, records `SignalReceived` and buffers the signal until an `await` step takes it. See [Signals and queries](../language/signals.md).

```bash
curl -X POST http://127.0.0.1:7233/v1/executions/order-A-100/signals/review \
  -H 'content-type: application/json' \
  -d '{"approved": true, "reviewer": "dana"}'
```

Response `200`:

```json
{"execution_id": "order-A-100", "signal": "review", "accepted": true, "seq": 5}
```

| Field | Type | Meaning |
|---|---|---|
| `execution_id` | string | The execution. |
| `signal` | string | The signal name. |
| `accepted` | boolean | Always `true` in a `200` response. |
| `seq` | integer | Sequence number of the recorded `SignalReceived` event. |

Errors: `404 NOT_FOUND`; `409 EXECUTION_FINISHED` (checked first); `404 UNKNOWN_SIGNAL`; `413 PAYLOAD_TOO_LARGE`; `422 SIGNAL_PAYLOAD_INVALID`:

```json
{"error":{"code":"UNKNOWN_SIGNAL","message":"OrderApproval@1.0.0 has no signal `reveiw`; declared: review"}}
{"error":{"code":"PAYLOAD_TOO_LARGE","message":"signal payload is 277 bytes; the limit is 200 (--max-payload-bytes)"}}
{"error":{"code":"SIGNAL_PAYLOAD_INVALID","message":"payload for signal `review` is invalid at $.approved: expected Bool, found a string"}}
```

A body sent without `content-type: application/json`, or a body that is not valid JSON, is rejected by the HTTP layer with a plain-text `415` or `400` (see [Errors](#errors)).

### GET /v1/executions/{id}/queries/{name} {#get-v1executionsidqueriesname}

Evaluate the query `name` against the execution's current state. Steps that have not succeeded read as `null`. Nothing is recorded. Works on running and terminal executions.

Response `200`:

```json
{"execution_id":"order-A-100","query":"status","value":{"needs_approval":true,"approved":true,"reviewer":"dana","charged":true}}
```

| Field | Type | Meaning |
|---|---|---|
| `execution_id` | string | The execution. |
| `query` | string | The query name. |
| `value` | any | The query result, of the query's declared type. |

Errors: `404 NOT_FOUND`; `404 UNKNOWN_QUERY`; `422 QUERY_FAILED` (`` evaluating query `<name>`: <cause> ``):

```json
{"error":{"code":"UNKNOWN_QUERY","message":"OrderApproval@1.0.0 has no query `stat`; declared: status"}}
```

## Workers

### GET /v1/workers

Every worker id that has ever registered, with its latest registration.

Response `200`:

```json
{
  "workers": [
    {
      "worker_id": "ts-1",
      "runtime": "typescript",
      "sdk": "curl",
      "contracts": [
        {"task_name": "orders.validate", "contract_version": "1.0.0"},
        {"task_name": "inventory.reserve", "contract_version": "1.0.0"}
      ],
      "last_seen_ms": 1794735369613,
      "registered_at_ms": 1794735367705,
      "online": false,
      "sessions": 0,
      "inflight": 0
    }
  ]
}
```

| Field | Type | Meaning |
|---|---|---|
| `worker_id`, `runtime`, `sdk` | string | As announced at registration; `runtime` is lowercased, `sdk` may be `null`. |
| `contracts` | array | Announced `{task_name, contract_version}` pairs. |
| `last_seen_ms`, `registered_at_ms` | integer | Latest contact and latest registration. |
| `online` | boolean | At least one session contacted the runtime within the session timeout. |
| `sessions` | integer | Live sessions. |
| `inflight` | integer | Tasks held by live sessions. |

## Worker protocol

These endpoints are the HTTP/JSON binding of `orbit.worker.v1.WorkerService`. The [Worker protocol](protocol.md) reference describes the lifecycle and the rules an implementation must follow; this section gives the exact bodies.

### POST /v1/workers/register

Open a worker session.

| Field | Type | Required | Meaning |
|---|---|---|---|
| `worker_id` | string | yes | Stable worker identity. Must not be empty. |
| `runtime` | string | yes | Worker language, matched case-insensitively against a step's `using <language>`. |
| `sdk` | string | no | Free-form SDK name and version. |
| `contracts` | array | yes | At least one `{task_name, contract_version}`. |

```json
{"worker_id": "py-1", "runtime": "python",
 "contracts": [{"task_name": "risk.analyze", "contract_version": "1.0.0"},
               {"task_name": "payments.charge", "contract_version": "1.0.0"}]}
```

Response `200`:

```json
{"accepted": true, "worker_session_id": "01081a88-bd5b-415f-9032-07d2bec51746",
 "heartbeat_interval_ms": 5000, "session_timeout_ms": 30000, "warnings": []}
```

`heartbeat_interval_ms` is one sixth of the session timeout, and at least 500. `warnings` lists contracts announced at a version that no registered workflow uses, which this worker will therefore never receive:

```json
{"accepted": true, "worker_session_id": "04586a02-...", "heartbeat_interval_ms": 5000, "session_timeout_ms": 30000,
 "warnings": ["`risk.analyze` is announced at version 0.9.0 but registered workflows use 1.0.0; this worker will not receive those tasks"]}
```

Errors: `400 INVALID_REQUEST`, `400 RESERVED_TASK`, `403 FORBIDDEN` (per-worker identities):

```json
{"error":{"code":"FORBIDDEN","message":"this credential belongs to worker `payments-1`, not `other`"}}
{"error":{"code":"FORBIDDEN","message":"worker `payments-1` must announce runtime `python`"}}
{"error":{"code":"FORBIDDEN","message":"worker `payments-1` may not serve: inventory.reserve (allowed: payments.*, audit.log)"}}
{"error":{"code":"RESERVED_TASK","message":"`orbit.http.request` is a built-in task"}}
```

### POST /v1/workers/heartbeat

Keep a session alive and learn which tasks to stop.

| Field | Type | Required | Meaning |
|---|---|---|---|
| `worker_session_id` | string | yes | Session from `register`. |

Response `200`:

```json
{"accepted": true, "cancel": ["5503f137-059e-455a-92c1-e65fa3690962"]}
```

`cancel` lists tokens this session holds that no longer matter: the step was resolved by another attempt, the attempt timed out, or the workflow is aborting. Stop that work if you can; results for those tokens are ignored.

Errors: `404 UNKNOWN_SESSION`; `403 FORBIDDEN` when a per-worker identity uses another worker's session.

### POST /v1/workers/poll

Long-poll for a task. The runtime hands out the oldest queued task that matches one of the session's contracts (same task name and contract version, and the same runtime when the step says `using <language>`).

| Field | Type | Required | Meaning |
|---|---|---|---|
| `worker_session_id` | string | yes | Session from `register`. |
| `wait_ms` | integer | no | Maximum wait. Default `20000`, capped at `60000`. |

Response `200`: a task.

```json
{
  "token": "3d932a10-600d-4989-acc7-5234a3183b06",
  "execution_id": "order-1002",
  "workflow": "OrderProcessing",
  "workflow_version": "1.0.0",
  "step_id": "process_payment",
  "task_name": "payments.charge",
  "contract_version": "1.0.0",
  "input": {"order_id": "1002", "amount": 99.5, "currency": "USD"},
  "idempotency_key": "charge-1002",
  "attempt": 1,
  "redelivery": false,
  "deadline_ms": 1794735399286,
  "timeout_ms": 30000
}
```

| Field | Type | Meaning |
|---|---|---|
| `token` | string | Identifies this dispatch. Send it with the result. |
| `execution_id`, `workflow`, `workflow_version` | string | The execution. |
| `step_id` | string | Unit key: `step`, `step[i]`, `compensate:<unit>` or `on_<kind>:<step>` (a step of an `on failure`, `on cancel` or `on timeout` handler). |
| `task_name`, `contract_version` | string | The contract to run. |
| `input` | any | Evaluated, validated input. |
| `idempotency_key` | string | Stable across retries and redeliveries of this unit. |
| `attempt` | integer | Attempt number, starting at 1. |
| `redelivery` | boolean | `true` when the previous delivery of this same attempt had an unknown outcome. |
| `deadline_ms`, `timeout_ms` | integer, optional | Present when the step has a `timeout`. |

Response `204` with an empty body: no task arrived within `wait_ms`.

Errors: `404 UNKNOWN_SESSION`; `403 FORBIDDEN`.

### POST /v1/workers/unregister

Close a session. Tasks it still holds become uncertain and are redelivered at once.

Request: `{"worker_session_id": "..."}`. Response `200`: `{"accepted": true}`. Errors: `404 UNKNOWN_SESSION`; `403 FORBIDDEN`.

### POST /v1/tasks/complete

Report a successful result.

| Field | Type | Required | Meaning |
|---|---|---|---|
| `token` | string | yes | Token from the task. |
| `output` | any | no | Result. `null` or absent means `{}`. Defaults are applied and it is validated against the task's output contract. |

```json
{"token": "41415faa-040f-4e35-9b92-b973d701ec2b", "output": {"valid": true}}
```

Response `200`:

| Body | Meaning |
|---|---|
| `{"accepted": true}` | The step succeeded with this output. |
| `{"accepted": false, "reason": "output violates the task contract"}` | The attempt failed with `OUTPUT_CONTRACT_VIOLATION` (not retried). |
| `{"accepted": false, "reason": "duplicate result; step already resolved"}` | The step was already resolved. Recorded as `StaleResultIgnored`. |
| `{"accepted": false, "reason": "unknown token (execution finished or token invalid)"}` | The token is unknown, or its execution has finished. |

A success is accepted for any token issued for a step that is not yet resolved, including the token of an earlier attempt that timed out or was considered lost. See [Delivery guarantees](../runtime/guarantees.md#reconciliation).

Errors: `400 INVALID_REQUEST` (`token is required`); `403 FORBIDDEN` when a per-worker identity reports for a token issued to another worker; `413 PAYLOAD_TOO_LARGE` when the output is larger than `--max-payload-bytes`. A refused output is not recorded and the attempt stays in flight, so the worker can still report a smaller result or a failure:

```json
{"error":{"code":"PAYLOAD_TOO_LARGE","message":"task result is 312 bytes; the limit is 200 (--max-payload-bytes)"}}
```

### POST /v1/tasks/fail

Report a failed attempt.

| Field | Type | Required | Default | Meaning |
|---|---|---|---|---|
| `token` | string | yes | | Token from the task. |
| `error_code` | string | yes | | Application error code, matched against the step's `non_retryable` list. |
| `error_message` | string | no | `""` | Human-readable message. |
| `retryable` | boolean | no | `true` | `false` fails the step without further attempts. |
| `details` | any | no | none | Structured details, stored with the error. |

```json
{"token": "3d932a10-600d-4989-acc7-5234a3183b06", "error_code": "CARD_DECLINED",
 "error_message": "card was declined", "retryable": false,
 "details": {"decline_code": "insufficient_funds"}}
```

Response `200`: `{"accepted": true}`, or `{"accepted": false, "reason": ...}` with `stale attempt` (the token is not the attempt currently in flight), `duplicate result; step already resolved`, or `unknown token (execution finished or token invalid)`.

Errors: `400 INVALID_REQUEST` (`error_code is required`); `403 FORBIDDEN`.

## gRPC binding

With `--grpc-addr`, the worker protocol is also served over gRPC as `orbit.worker.v1.WorkerService` (`RegisterWorker`, `Heartbeat`, `PollTask`, `CompleteTask`, `FailTask`, `UnregisterWorker`), defined in [`protocol/orbit/worker/v1/worker.proto`](https://github.com/AjmalX0/Orbit/blob/main/protocol/orbit/worker/v1/worker.proto). Credentials go in the `authorization: Bearer <token>` metadata entry. Errors map to gRPC status codes as follows, and carry the ORBIT code in the `orbit-error-code` metadata entry and as a `CODE: ` prefix of the message:

| HTTP status | gRPC status |
|---|---|
| 400 | `INVALID_ARGUMENT` |
| 401 | `UNAUTHENTICATED` |
| 403 | `PERMISSION_DENIED` |
| 404 | `NOT_FOUND` |
| 409 | `FAILED_PRECONDITION` |
| 413 (`PAYLOAD_TOO_LARGE`, `CompleteTask`) | `INTERNAL` |
| 500 | `INTERNAL` |
| 503 (`NOT_LEADER`, cluster standby) | `UNAVAILABLE` |

`PollTask` returns an `ExecuteTaskRequest` with an empty `token` where HTTP returns `204`. Client identities cannot use the gRPC binding: every call gets `PERMISSION_DENIED` with the message ``FORBIDDEN: client `app` may not use the worker protocol``. A result larger than `--max-payload-bytes` gets `INTERNAL` with `orbit-error-code: PAYLOAD_TOO_LARGE`, so check the metadata rather than the status code.

## See also

- [Worker protocol](protocol.md)
- [CLI](cli.md)
- [Executions and history](../runtime/executions.md)
- [Security](../runtime/security.md)
- [Delivery guarantees](../runtime/guarantees.md)
