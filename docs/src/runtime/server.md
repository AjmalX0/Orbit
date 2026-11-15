# Running the server

`orbit server` runs the ORBIT runtime: one process that stores workflow definitions and execution history, decides what happens next in every execution, hands tasks to workers and serves the HTTP API. Workers are separate processes in any language; they never own workflow state. This page covers starting the server, every option, configuration examples from local development to production, health checks and shutdown.

```text
              orbit CLI / apps ──HTTP(S)──┐
                                          ▼
 ┌────────────────────────── orbit server ───────────────────────────┐
 │  HTTP API ─┐                                                       │
 │  gRPC  ────┼─► Engine ─► decide(state, IR, now)  (pure)            │
 │            │     │  ▲                                              │
 │            │     ▼  │ fold(events)          OTLP traces ─► collector│
 │            │   SQLite or PostgreSQL: execution_events (authoritative)│
 └────────────┴──────▲─────────────────────────────────▲─────────────┘
         long-poll / results (HTTP/JSON or gRPC, optional mTLS)
      Python · TypeScript · Go · Rust · Java · C# workers
```

## Start a local server

With no options, the server stores everything in `orbit.db` in the current directory and listens on `127.0.0.1:7233` without authentication:

```console
$ orbit server
2026-11-15T09:35:12.795070Z  INFO orbit: starting ORBIT runtime db=orbit.db backend="sqlite" http_adapter=false auth=false
2026-11-15T09:35:12.797133Z  INFO orbit_runtime::server: ORBIT runtime listening addr=127.0.0.1:7233 tls=false
```

In another terminal, check it and deploy a workflow:

```console
$ curl -s http://127.0.0.1:7233/v1/health
{"status":"ok","api_version":"orbit-api/1","runtime_version":"0.2.0","ir_versions":["orbit-ir/1","orbit-ir/2"],"active_executions":0,"worker_sessions":0,"role":"single","node_id":null,"term":null,"leader":null}
$ orbit deploy examples/order_processing.orbit
deployed OrderProcessing@1.0.0 (sha256:4fdfac082bf3)
```

An unauthenticated server is allowed only on a loopback address. To listen on another interface, set a token (see [Security](security.md)).

## Options

Every option can also be set through the environment variable listed, except where the column is empty. A flag overrides its environment variable.

| Option | Environment | Type | Default | Meaning |
|---|---|---|---|---|
| `--db` | `ORBIT_DB` | path or URL | `orbit.db` | SQLite file path, or a `postgres://` / `postgresql://` URL. See [Storage](storage.md). |
| `--addr` | `ORBIT_ADDR` | `ip:port` | `127.0.0.1:7233` | HTTP API address. It serves the administrative API and the HTTP/JSON worker protocol. It must be an IP address, not a host name: use `0.0.0.0:7233` or `[::]:7233` for all interfaces. |
| `--grpc-addr` | `ORBIT_GRPC_ADDR` | `ip:port` | off | Also serve the gRPC worker protocol on this address, for example `127.0.0.1:7234`. |
| `--token` | `ORBIT_TOKEN` | string | none | Admin bearer token, required on every endpoint except `/v1/health`. |
| `--worker-token` | `ORBIT_WORKER_TOKEN` | string | none | Shared worker credential, accepted only on `/v1/workers/*` and `/v1/tasks/*`. Requires `--token` and must differ from it. |
| `--workers-file` | `ORBIT_WORKERS_FILE` | path | none | Worker identities (`[[worker]]`) and client identities (`[[client]]`), in TOML. Requires `--token`. See [Security](security.md#per-worker-identities). |
| `--tls-cert` | `ORBIT_TLS_CERT` | path | none | PEM certificate chain. With `--tls-key`, the HTTP API (and gRPC) serve TLS. |
| `--tls-key` | `ORBIT_TLS_KEY` | path | none | PEM private key (PKCS#8, PKCS#1 or SEC1). |
| `--tls-client-ca` | `ORBIT_TLS_CLIENT_CA` | path | none | Enables mutual TLS: client certificates signed by this CA identify workers. Requires `--tls-cert` and a workers file with `client_cert` entries. |
| `--otlp-endpoint` | `OTEL_EXPORTER_OTLP_ENDPOINT` | URL | off | Export OpenTelemetry traces over OTLP/HTTP (JSON) to this base URL. |
| `--enable-http-adapter` | | flag | off | Run `orbit.http.request` steps inside the runtime. |
| `--max-active-executions` | `ORBIT_MAX_ACTIVE_EXECUTIONS` | integer | unlimited | Maximum running executions. Further starts get `429 RESOURCE_EXHAUSTED`. See [Resource limits](#resource-limits). |
| `--max-fan-out` | `ORBIT_MAX_FAN_OUT` | integer | unlimited | Maximum elements one `for` step may fan out to. A larger list fails the step with `LIMIT_EXCEEDED`. |
| `--max-history-events` | `ORBIT_MAX_HISTORY_EVENTS` | integer | unlimited | Maximum history events per execution. Past it the execution fails with `LIMIT_EXCEEDED`. |
| `--max-payload-bytes` | `ORBIT_MAX_PAYLOAD_BYTES` | bytes | unlimited | Maximum serialized size of an execution input, a task result or a signal payload. Larger ones get `413 PAYLOAD_TOO_LARGE`. |
| `--session-timeout` | | seconds | `30` | A worker session silent for this long is considered lost, and its in-flight attempts are redelivered. Workers are told to heartbeat every sixth of it (at least every 500 ms). |
| `--max-body` | | bytes | `4194304` (4 MiB) | Maximum request body size. Larger requests get `413`. |
| `--log-format` | | `text` or `json` | `text` | Log format. Logs go to stderr. |
| `--insecure` | | flag | off | Allow a non-loopback `--addr` without a token. Use only on a trusted network. |
| `--cluster` | `ORBIT_CLUSTER` | flag | off | Run as one of several servers on the same PostgreSQL database: one leader serves, the others stand by. Requires a PostgreSQL `--db`. See [Running several servers](cluster.md). |
| `--node-id` | `ORBIT_NODE_ID` | string | host name | This server's name in the cluster. Requires `--cluster`. |
| `--advertise-addr` | `ORBIT_ADVERTISE_ADDR` | URL | from `--addr` | URL standbys hand out for this server while it leads, for example `http://orbit-0.orbit:7233`. Without it, a wildcard `--addr` advertises nothing. Requires `--cluster`. |

The server also reads `ORBIT_LOG` (log filter), `ORBIT_DB_CA_CERT` (CA for PostgreSQL TLS), `OTEL_EXPORTER_OTLP_HEADERS` and `OTEL_SERVICE_NAME`. See [Environment variables](../reference/environment.md).

### Configuration the server refuses

The server checks its configuration before it opens the database and exits with status `1` if:

| Condition | Message |
|---|---|
| A non-loopback `--addr` without `--token` or `--insecure` | `refusing to listen on 0.0.0.0:7233 without authentication; pass --token (or ORBIT_TOKEN), or --insecure for trusted networks` |
| `--worker-token` without `--token` | `--worker-token requires --token: without an admin token every endpoint would be open` |
| `--worker-token` equal to `--token` | `--worker-token must differ from --token` |
| `--workers-file` without `--token` | `--workers-file requires --token: without an admin token administrative endpoints would be open` |
| `--tls-client-ca` without `client_cert` entries | `` --tls-client-ca needs --workers-file entries with `client_cert` to map certificates to workers `` |
| An invalid workers file | for example `` workers.toml: worker `a` needs `token_sha256` or `client_cert` `` |
| `--cluster` with a SQLite `--db` | `--cluster requires a PostgreSQL database (--db postgres://...): the servers share it for leader election and history` |

An empty token (`ORBIT_TOKEN=`) counts as no token.

## Configuration examples

### Local development

```bash
orbit server
```

Equivalent to `orbit server --db orbit.db --addr 127.0.0.1:7233`. Use a separate database file per project, or delete `orbit.db` to start over.

### Production with tokens

Generate the credentials once and keep them in a secret store:

```bash
export ORBIT_TOKEN=$(orbit token 2>/dev/null)          # admin token
export ORBIT_WORKER_TOKEN=$(orbit token 2>/dev/null)   # shared worker token
orbit server --db /var/lib/orbit/orbit.db --addr 0.0.0.0:7233 --log-format json
```

Give `ORBIT_WORKER_TOKEN` to worker hosts, and `ORBIT_TOKEN` only to operators and the applications that start executions. To give each worker its own credential and limit the tasks it may serve, use a workers file instead of, or in addition to, the shared worker token:

```bash
orbit server --addr 0.0.0.0:7233 --token "$ORBIT_TOKEN" --workers-file /etc/orbit/workers.toml
```

The same file can declare client identities (`[[client]]`): credentials for applications that may only start, signal, query, read or cancel executions of the workflows you list, without the admin token. The file format is described in [Security](security.md#per-worker-identities) and [Security: client identities](security.md#client-identities).

### TLS

```bash
orbit server --addr 0.0.0.0:7233 --token "$ORBIT_TOKEN" \
  --tls-cert /etc/orbit/tls/server.pem --tls-key /etc/orbit/tls/server.key
```

```console
INFO orbit_runtime::server: ORBIT runtime listening addr=0.0.0.0:7233 tls=true mtls=false
```

Clients use `https://` and, for a private CA, `--ca-cert` or `ORBIT_CA_CERT`:

```bash
orbit ps --server https://orbit.internal:7233 --ca-cert /etc/orbit/tls/ca.pem
```

TLS 1.2 and 1.3 are supported, with HTTP/2 and HTTP/1.1 negotiated through ALPN.

### Mutual TLS

```bash
orbit server --addr 0.0.0.0:7233 --token "$ORBIT_TOKEN" \
  --tls-cert /etc/orbit/tls/server.pem --tls-key /etc/orbit/tls/server.key \
  --tls-client-ca /etc/orbit/tls/workers-ca.pem \
  --workers-file /etc/orbit/workers.toml
```

```console
INFO orbit_runtime::server: ORBIT runtime listening addr=0.0.0.0:7233 tls=true mtls=true
```

Client certificates are optional at the TLS layer: a client without a certificate, such as the admin CLI, can still connect and authenticate with a token. See [Security](security.md#mutual-tls).

### PostgreSQL

```bash
orbit server --addr 0.0.0.0:7233 --token "$ORBIT_TOKEN" \
  --db 'postgres://orbit:secret@db.internal:5432/orbit?sslmode=require'
```

The schema is created on first start. The password is redacted in logs (`postgres://orbit:***@db.internal:5432/orbit?sslmode=require`). See [Storage](storage.md).

### OpenTelemetry tracing

```bash
OTEL_SERVICE_NAME=orbit-prod \
OTEL_EXPORTER_OTLP_HEADERS="x-honeycomb-team=$HONEYCOMB_KEY" \
orbit server --token "$ORBIT_TOKEN" --otlp-endpoint https://api.honeycomb.io
```

```console
INFO orbit_runtime::engine: exporting OpenTelemetry traces endpoint=https://api.honeycomb.io service=orbit-prod
```

See [Observability](observability.md#opentelemetry-tracing).

### HTTP adapter

```bash
orbit server --enable-http-adapter
```

With the adapter enabled, steps that call the built-in `orbit.http.request` task run inside the runtime, with no worker. Only `http://` and `https://` URLs are allowed. The adapter lets workflow authors make outbound requests from the runtime host, so enable it only when you trust everyone who can deploy workflows. When it is disabled, such steps fail with `ADAPTER_DISABLED` (not retried). See [HTTP adapter](../language/http-adapter.md).

### gRPC worker protocol

```bash
orbit server --token "$ORBIT_TOKEN" --worker-token "$ORBIT_WORKER_TOKEN" \
  --addr 0.0.0.0:7233 --grpc-addr 0.0.0.0:7234
```

```console
INFO orbit_runtime::server: gRPC worker protocol listening addr=0.0.0.0:7234 tls=false
INFO orbit_runtime::server: ORBIT runtime listening addr=0.0.0.0:7233 tls=false
```

The gRPC listener shares the engine, credentials, per-worker identities, TLS and mutual TLS settings with the HTTP API. Only the worker protocol is served over gRPC; the administrative API is HTTP only. The official SDKs use HTTP/JSON. See [Worker protocol](../reference/protocol.md).

## Resource limits

By default the server accepts as many executions, fan-out elements, history events and bytes as the host can hold. Four options cap them, so one workflow or one caller cannot exhaust the runtime. Each limit is off until you set it, and each can be set through its environment variable.

| Option | Limits | When it is reached |
|---|---|---|
| `--max-active-executions` | Executions that are not terminal, held in memory. | A new start gets `429 RESOURCE_EXHAUSTED`. Nothing is recorded. |
| `--max-fan-out` | Elements of one `for` step. | The step fails with `LIMIT_EXCEEDED` (not retried) before any element is scheduled. |
| `--max-history-events` | Events in one execution's history. | A `LimitExceeded` event is recorded and the execution fails with `LIMIT_EXCEEDED`. |
| `--max-payload-bytes` | Serialized JSON size of an execution input (after defaults are applied), a task result, or a signal payload. | The request gets `413 PAYLOAD_TOO_LARGE`. Nothing is recorded. |

```bash
orbit server --token "$ORBIT_TOKEN" \
  --max-active-executions 10000 --max-fan-out 1000 \
  --max-history-events 50000 --max-payload-bytes 262144
```

`--max-payload-bytes` is separate from `--max-body`: `--max-body` limits the whole HTTP request body and is enforced before the request is read, with a plain-text `413`; `--max-payload-bytes` limits the value inside it and answers with the JSON error envelope.

### Per-workflow limits

A workflow can tighten the fan-out, history and payload limits for its own executions with a `limits` block. The stricter of the workflow's value and the server's option applies; a workflow cannot raise a server limit. `--max-active-executions` is server-wide only.

```orbit
workflow Shipment {
    version "1.0.0"
    limits { max_fan_out: 100, max_history_events: 5000, max_payload_bytes: 65536 }
    ...
}
```

| Field | Same as |
|---|---|
| `max_fan_out` | `--max-fan-out` |
| `max_history_events` | `--max-history-events` |
| `max_payload_bytes` | `--max-payload-bytes`, for this workflow's input, task results and signal payloads |

Each value is a positive integer literal. An unknown field is error `E0320` (with a suggestion), and a zero, negative or computed value is error `E0321`. A workflow with a `limits` block compiles to `orbit-ir/2`, and `orbit inspect` lists the block. When a workflow limit is reached, the error is the same as for the server option and names both: ``the execution reached 5000 history events (--max-history-events or the workflow's `limits`)``.

### Active executions

The limit applies to executions started through `POST /v1/executions` (and `orbit run`). Starting an `execution_id` that already exists is still answered with `200` and `"created": false` when the limit is reached, because it starts nothing. Child workflows are never refused and do not count toward the total, so a running parent can always start its children and the limit counts top-level executions only.

```console
$ orbit server --token "$ORBIT_TOKEN" --max-active-executions 2
$ curl -s -H "authorization: Bearer $ORBIT_TOKEN" -H 'content-type: application/json' \
    -d '{"workflow":"Review","execution_id":"review-2","input":{"order_id":"2"}}' \
    -w ' %{http_code}\n' http://127.0.0.1:7233/v1/executions
{"error":{"code":"RESOURCE_EXHAUSTED","message":"2 executions are already running (--max-active-executions)"}} 429
$ curl -s -H "authorization: Bearer $ORBIT_TOKEN" -H 'content-type: application/json' \
    -d '{"workflow":"Review","execution_id":"review-1","input":{"order_id":"1"}}' \
    -w ' %{http_code}\n' http://127.0.0.1:7233/v1/executions
{"execution_id":"review-1","created":false} 200
```

Treat `429` as a signal to retry later with backoff, using the same `execution_id` so a retry never starts a second execution.

### Fan-out

```orbit
task "labels.print" {
    version "1.0.0"
    input { sku: String }
    output { label: String }
}

workflow PrintLabels {
    version "1.0.0"
    input { skus: List<String> }

    step print {
        for sku in input.skus
        worker "labels.print"
        with { sku: sku }
    }

    on failure {
        step alert {
            compute { code: failure.code, message: failure.message }
        }
    }
}
```

With `--max-fan-out 3`, an execution with four SKUs fails at the `for` step. The failure aborts the execution like any other step failure: compensation and `on failure` handlers run.

```console
$ orbit logs labels-2
+     0ms   #1  WorkflowCreated           
+     0ms   #2  WorkflowStarted           
+     2ms   #3  StepFailed                step=print error=LIMIT_EXCEEDED `for` over 4 elements exceeds the fan-out limit of 3 (--max-fan-out or the workflow's `limits`)
+     2ms   #4  HandlersStarted           kind=failure
+     2ms   #5  StepSucceeded             step=on_failure:alert attempt=1
+     2ms   #6  WorkflowFailed            error=LIMIT_EXCEEDED step `print` failed: `for` over 4 elements exceeds the fan-out limit of 3 (--max-fan-out or the workflow's `limits`)
```

### History events

The runtime checks the limit each time it decides what an execution does next. When the next events would take the history past the limit, it records `LimitExceeded` instead of scheduling more work. The execution then aborts as after a failure: steps that have not started are cancelled, in-flight attempts finish, succeeded steps are compensated, the `on failure` handler runs, and `WorkflowFailed` is recorded with `LIMIT_EXCEEDED`. The events of that shutdown are recorded even though they take the history past the limit, and so are results that workers report while the decision is pending, so a history can end a few events above the limit.

With `--max-history-events 10`, the same workflow with three SKUs:

```console
$ orbit logs labels-3
+     0ms   #1  WorkflowCreated           
+     0ms   #2  WorkflowStarted           
+     1ms   #3  StepFanOut                step=print
+     1ms   #4  StepScheduled             step=print[0] attempt=1
+     1ms   #5  StepScheduled             step=print[1] attempt=1
+     1ms   #6  StepScheduled             step=print[2] attempt=1
+   2m14s   #7  StepDispatched            step=print[0] attempt=1 worker_id=printer
+   2m15s   #8  StepSucceeded             step=print[0] attempt=1
+   2m15s   #9  StepDispatched            step=print[1] attempt=1 worker_id=printer
+   2m15s  #10  StepSucceeded             step=print[1] attempt=1
+   2m15s  #11  StepDispatched            step=print[2] attempt=1 worker_id=printer
+   2m15s  #12  StepSucceeded             step=print[2] attempt=1
+   2m15s  #13  LimitExceeded             limit=max_history_events
+   2m15s  #14  StepCancelled             step=print
+   2m15s  #15  HandlersStarted           kind=failure
+   2m15s  #16  StepSucceeded             step=on_failure:alert attempt=1
+   2m15s  #17  WorkflowFailed            error=LIMIT_EXCEEDED the execution reached 10 history events (--max-history-events or the workflow's `limits`)
```

The limit protects the runtime from executions that never stop growing, such as a workflow that retries forever. Set it well above the size of your largest legitimate history: about three events per task attempt, plus one per fan-out step, signal and timer.

### Payload size

The size is the length of the value serialized as compact JSON. The check covers execution input, task results reported over HTTP and gRPC, and signal payloads:

```console
$ orbit server --token "$ORBIT_TOKEN" --max-payload-bytes 200
$ curl -s ... http://127.0.0.1:7233/v1/executions
{"error":{"code":"PAYLOAD_TOO_LARGE","message":"execution input is 414 bytes; the limit is 200 (--max-payload-bytes or the workflow's `limits`)"}} 413
$ curl -s ... http://127.0.0.1:7233/v1/executions/review-1/signals/decision
{"error":{"code":"PAYLOAD_TOO_LARGE","message":"signal payload is 277 bytes; the limit is 200 (--max-payload-bytes or the workflow's `limits`)"}} 413
$ curl -s ... http://127.0.0.1:7233/v1/tasks/complete
{"error":{"code":"PAYLOAD_TOO_LARGE","message":"task result is 312 bytes; the limit is 200 (--max-payload-bytes or the workflow's `limits`)"}} 413
```

A refused task result is not recorded: the attempt stays in flight, so the worker can report a smaller result or a failure with `POST /v1/tasks/fail` before the attempt's timeout. Over gRPC the same refusal is a status with the `orbit-error-code` metadata `PAYLOAD_TOO_LARGE` (see [HTTP API: gRPC binding](../reference/http-api.md#grpc-binding)). Error details sent with `POST /v1/tasks/fail` are not checked.

## Startup and recovery

On startup, the server:

1. checks its configuration;
2. opens the database and creates the schema if needed;
3. loads every workflow definition;
4. loads every execution that is not terminal, replays its history and re-indexes the tokens of in-flight attempts;
5. runs the decider for each recovered execution, which fires overdue timers and retries and schedules ready steps;
6. binds its listeners and starts serving.

```console
INFO orbit_runtime::engine: recovered active executions from history executions=1 in_flight=0
```

Attempts that were in flight before the restart get one session timeout for their workers to reconnect and report a result. After that, attempts nobody reported are redelivered. See [Delivery guarantees](guarantees.md#crash-recovery).

Because recovery runs before the listener is bound, connection attempts fail until the server is ready. A runtime with many active executions takes longer to start. In cluster mode the order differs: a server binds its listener at once as a standby and recovers only when it is elected; see [Running several servers](cluster.md).

## Health and readiness

`GET /v1/health` never requires authentication and returns `200` once the server is listening:

```json
{"status":"ok","api_version":"orbit-api/1","runtime_version":"0.2.0","ir_versions":["orbit-ir/1","orbit-ir/2"],"active_executions":0,"worker_sessions":0,"role":"single","node_id":null,"term":null,"leader":null}
```

Use it for liveness probes, and for readiness of a single server: a single server answers only after recovery. The response also carries `role` (`single`, or `leader` and `standby` in cluster mode), `node_id`, `term` and `leader`.

`GET /v1/ready` never requires authentication either. It returns `200` on a single server and on a cluster leader, and `503` on a standby. Use it as the readiness or load-balancer health check when you run several servers, so traffic reaches only the leader. See [Running several servers](cluster.md#load-balancer-configuration).

If you use mutual TLS, a probe that presents a client certificate matching no worker gets `403`; send probes without a client certificate.

For monitoring beyond liveness, scrape `/v1/metrics` with the admin token. See [Observability](observability.md).

## Background work

Every 100 ms the server runs a tick that:

- marks attempts whose worker session went silent as uncertain and redelivers them;
- fires due timers, retries, attempt timeouts, queue timeouts and workflow timeouts;
- runs built-in tasks (the HTTP adapter);
- starts child workflows, delivers finished children's results to their parents and cancels children whose parent no longer waits for them.

Worker sessions that stay silent for four session timeouts are forgotten; the worker must register again.

## Shutdown

Press Ctrl+C, or send `SIGINT` or `SIGTERM`, for a graceful shutdown:

```console
INFO orbit: shutting down
```

The server stops accepting connections, waits for in-flight HTTP requests to finish, stops the gRPC listener and exits with status `0`. Long polls are not interrupted: a worker poll can hold shutdown for up to its `wait_ms` (at most 60 seconds), and a `GET /v1/executions/{id}/wait` for up to its `timeout_ms` (at most 300 seconds). With TLS, open connections get at most 10 seconds before they are closed.

Version 0.1.0 handles only `SIGINT`; `SIGTERM` ends a 0.1.0 server immediately, as `SIGKILL` would. Later versions handle both. Either way no committed state is lost, because every state change is committed to the database before it takes effect, and the next start recovers exactly what was committed. To get a graceful stop from every version, set the stop signal to `SIGINT`: `docker run --stop-signal SIGINT`, `stop_signal: SIGINT` in Compose, or `KillSignal=SIGINT` under systemd. See [Deployment](deployment.md).

## Single node and cluster mode

Without `--cluster`, one server owns a database. Do not run two such servers against the same SQLite file or PostgreSQL database: each would keep its own in-memory view of active executions and dispatch the same work. For availability, either run one server under a supervisor that restarts it (systemd, Kubernetes with one replica) and rely on recovery, or run several servers with `--cluster` on PostgreSQL: one is elected leader, the others take over if it fails. See [Running several servers](cluster.md) and [Storage](storage.md#single-node).

## See also

- [Security](security.md)
- [Storage](storage.md)
- [Running several servers](cluster.md)
- [Observability](observability.md)
- [Deployment](deployment.md)
- [CLI: orbit server](../reference/cli.md#orbit-server)
- [Environment variables](../reference/environment.md)
