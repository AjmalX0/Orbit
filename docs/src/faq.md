# FAQ and troubleshooting

This page lists the errors you are most likely to meet, with the exact
message, the cause and the fix, followed by answers to common questions about
ORBIT's design. For every compiler error code, see
[Compiler diagnostics](reference/diagnostics.md).

## Troubleshooting

### Cannot reach the runtime

```text
error: cannot reach the ORBIT runtime at http://127.0.0.1:7233: io: Connection refused (os error 111)
  hint: start it with `orbit server` or set --server / ORBIT_SERVER
```

Workers report the same condition and keep retrying with backoff:

```text
WARNING orbit cannot reach ORBIT runtime at http://127.0.0.1:7233: <urlopen error [Errno 111] Connection refused>; retrying in 0.2s
```

Causes and fixes:

- **The server is not running.** Start it with `orbit server` and check
  `curl http://127.0.0.1:7233/v1/health`.
- **The client points at the wrong address.** CLI commands and SDKs default to
  `http://127.0.0.1:7233`. Set `ORBIT_SERVER` (or the CLI's `--server`) to the
  runtime's URL, including the scheme and port.
- **The server listens on loopback only.** The default `--addr` is
  `127.0.0.1:7233`, which other machines and containers cannot reach. Listen
  on another interface with `--addr 0.0.0.0:7233` and a token.
- **The container port is not published.** Run the image with
  `-p 7233:7233`.

### Address already in use

```text
error: cannot listen on 127.0.0.1:7233: Address already in use (os error 98)
```

Another process, often an earlier `orbit server`, holds the port. Stop it, or
start the server on another port with `--addr 127.0.0.1:7300` and set
`ORBIT_SERVER=http://127.0.0.1:7300` for clients and workers.

### The server refuses to start without a token

```text
error: refusing to listen on 0.0.0.0:7233 without authentication; pass --token (or ORBIT_TOKEN), or --insecure for trusted networks
```

The server refuses to listen on a non-loopback address without
authentication. This is also why the Docker image exits immediately when
`ORBIT_TOKEN` is not set. Pass a token:

```bash
orbit server --addr 0.0.0.0:7233 --token "$(openssl rand -hex 32)"
```

Use `--insecure` only on a network where every host is trusted. See
[Security](runtime/security.md).

### Unauthenticated or forbidden

```text
error: UNAUTHENTICATED (401): missing or invalid credentials
```

The server requires a bearer token and the request carried none, or the wrong
one. Pass the token that the server was started with:

| Client | Setting |
|---|---|
| CLI | `--token` or `ORBIT_TOKEN` |
| SDK workers | `ORBIT_WORKER_TOKEN` (preferred) or `ORBIT_TOKEN` |

An SDK worker without a valid token stops at startup with the same error, for
example `orbit_sdk.client.ApiError: UNAUTHENTICATED (401): missing or invalid
credentials` in Python or `ApiError: UNAUTHENTICATED (401): ...` in Node.js.

```text
error: FORBIDDEN (403): worker credentials may only call /v1/workers/* and /v1/tasks/*
```

You used the shared worker token (`--worker-token`) for an admin command such
as `orbit run` or `orbit ps`. Worker credentials can only use the worker
protocol. Use the admin token for the CLI. A per-worker identity from
`--workers-file` is also refused with `403` when it registers under another
worker's id or announces a task its patterns do not allow.

```text
error: FORBIDDEN (403): client `warehouse-app` may not start executions of `Review`
```

The token belongs to a client identity (`[[client]]` in the workers file),
which may act only on the workflows and actions its entry grants, and never
deploy workflows or call the worker protocol. Add the workflow or action to
the entry and restart the server, or use the admin token. See
[Security: client identities](runtime/security.md#client-identities).

```text
error: RESOURCE_EXHAUSTED (429): 2 executions are already running (--max-active-executions)
```

The server runs with `--max-active-executions` and that many executions are
running. Retry later with the same execution id, or raise the limit. See
[Resource limits](runtime/server.md#resource-limits).

### A step stays SCHEDULED: no worker for the task

`orbit run` keeps waiting, and `orbit status` shows the step as `SCHEDULED`
with no worker:

```text
  STEP                         STATUS       ATTEMPT     TIME  DETAIL
  frequencies                  SCHEDULED          1
```

The task is queued, and no connected worker matches it. The runtime does not
treat this as an error: the task waits, durably, until a matching worker
connects. Compare the step with `orbit workers list`:

```text
WORKER                       RUNTIME      STATE    INFLIGHT  TASKS
text-python                  python       online   0  text.normalize@1.0.0
text-rust                    rust         online   0  text.fingerprint@1.0.0
```

A worker matches only if all of the following hold:

1. its `STATE` is `online`;
2. it lists the step's task name, spelled exactly the same;
3. it lists the same contract version (see the next section);
4. if the step says `using <language>`, its `RUNTIME` is that language.

To fail instead of waiting, add `queue_timeout` to the step. When it elapses,
the step fails with a non-retryable `QUEUE_TIMEOUT`:

```text
error: QUEUE_TIMEOUT step `compose` failed: no worker picked up the task before queue_timeout
```

### Contract version mismatch

The workflow uses a contract version that no worker announced. For example,
the workflow declares:

```orbit
task "greeting.compose" {
    version "2.0.0"
    input { name: String }
    output { message: String }
}
```

while the worker registers:

```python
@worker.task("greeting.compose", version="1.0.0")
```

`orbit workers list` shows `greeting.compose@1.0.0`, and the step waits as
described above. Contract versions must match exactly. Either register the
handler under the version the workflow uses, or deploy a workflow version
that uses the contract the workers implement. To serve both versions during a
migration, run a second worker process for the new version. See
[Versioning](language/versioning.md).

### Worker output does not match its contract

```text
error: OUTPUT_CONTRACT_VIOLATION step `compose` failed: worker output for `greeting.compose` does not match its contract at $.message: expected String, found an integer
```

The worker returned a result whose shape differs from the contract's `output`
type. The runtime validates every result and fails the step without retrying,
because retrying the same code would produce the same result. Fix the worker,
or change the contract and bump its version. The path (`$.message`) points at
the first mismatch.

### Workflow input is invalid

```text
error: INVALID_INPUT (400): workflow input is invalid at $.name: missing required field `name` of type String
```

Execution input is validated strictly before the execution is created:
missing required fields, wrong types and unknown fields are all rejected.
Compare the JSON you passed with `--input` or `--input-file` with the
workflow's `input` block. Fields with a default can be omitted.

### E0405: value may be null

```text
error[E0405]: value may be `null` but `String` is required
  --> order_processing.orbit:97:32
   |
97 |         with { transaction_id: process_payment.transaction_id }
   |                                ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
   = help: provide a fallback with `coalesce(value, default)`, or declare the target as optional with `?`
```

An expression of an optional type (`T?`) is used where a non-optional `T` is
required. Common sources of optional values:

- the output of a step with `when`, because a skipped step's output is `null`;
- a field declared with `?` in a schema or contract;
- an index such as `items[0]` or `headers["x"]`.

Fixes, depending on what the value means:

- supply a default: `coalesce(process_payment.transaction_id, "none")`;
- make the target optional: declare the contract field as
  `transaction_id: String?`;
- skip the consuming step in the same case: give it the same `when` condition.

### Definition already registered with different content

```text
error: DEFINITION_CONFLICT (409): Hello@1.0.0 is already registered with a different definition (sha256:505930b4d4668ff2f8cb95e04519d5292ef2fbf0630d575349aaf92c0e2b42be); workflow definitions are immutable, bump the version
```

You changed a workflow and deployed it under a version that is already
registered. Definitions are immutable per `name@version`, so running
executions can never change underneath you. Bump the workflow's `version`
and deploy again. Reformatting and comment changes do not change the
definition hash and never cause this error.

During local development you can also start over with an empty database:
stop the server and delete `orbit.db`, `orbit.db-wal` and `orbit.db-shm`.
This deletes all history.

### TLS errors

The runtime's certificate is not trusted:

```text
error: cannot reach the ORBIT runtime at https://127.0.0.1:7233: io: invalid peer certificate: UnknownIssuer
```

The server uses a certificate from a private CA. Give clients the CA
certificate, not the server certificate:

| Client | Setting |
|---|---|
| CLI | `--ca-cert ca.pem` or `ORBIT_CA_CERT` |
| Python, Go, Rust, Java, C# workers | `ORBIT_CA_CERT` |
| Node.js workers | `NODE_EXTRA_CA_CERTS` |

The file must be a real CA certificate (`basicConstraints=CA:TRUE`,
`keyCertSign`). Passing the server's own certificate produces the same
`UnknownIssuer` error. The server certificate must also name the host the
client connects to (for example `DNS:localhost` or `IP:127.0.0.1` in its
subject alternative names).

The client speaks HTTPS to a server that serves plain HTTP:

```text
error: cannot reach the ORBIT runtime at https://127.0.0.1:7233: io: received corrupt message of type InvalidContentType
```

The client speaks plain HTTP to a server that serves HTTPS:

```text
error: cannot reach the ORBIT runtime at http://127.0.0.1:7233: protocol: http parse fail: invalid HTTP version
```

In both cases, make the scheme in `ORBIT_SERVER` match the server: `https://`
when it was started with `--tls-cert` and `--tls-key`, `http://` otherwise.

### The HTTP adapter is disabled

```text
error: ADAPTER_DISABLED step `probe` failed: the HTTP adapter is disabled; start the server with --enable-http-adapter
```

Steps that use `orbit.http.request` need the server flag
`--enable-http-adapter`. See [Calling HTTP services](guides/http.md).

## Frequently asked questions

### Does ORBIT guarantee exactly-once execution?

No. ORBIT delivers tasks at least once and never promises exactly-once
execution of external side effects. A worker can crash after charging a card
but before reporting the result; no orchestrator can tell that case apart from
a crash before the charge. ORBIT treats such an outcome as "possibly done",
redelivers the same attempt with the same idempotency key, and still accepts a
late result from the original delivery. The runtime records exactly one
result per step, but your worker must make the side effect itself idempotent,
for example by storing results by `ctx.idempotency_key` or passing the key to
an API that deduplicates on it. See [Delivery guarantees](runtime/guarantees.md).

### Can I run several runtime nodes?

Yes, for availability, with PostgreSQL. Start every server with `--cluster`
and the same `--db postgres://...`: one is elected leader and runs the engine,
the others stand by, serve read-only routes and take over within about a
second if the leader stops. Only one server executes workflows at a time, so
cluster mode does not add throughput. Put a load balancer in front that checks
`GET /v1/ready`. See [Running several servers](runtime/cluster.md).

Without `--cluster`, run only one `orbit server` per database, with a restart
policy; after a restart it recovers every unfinished execution from history.
See [Deployment](runtime/deployment.md).

### How does ORBIT compare with Temporal?

Both record workflow history durably, recover by replaying it, deliver work
to workers at least once, and support workers in several languages. The main
differences:

| | ORBIT 0.5 | Temporal |
|---|---|---|
| Workflow definition | A declarative `.orbit` file, compiled and type-checked against task contracts before deployment | Code in an SDK language, with determinism rules enforced at replay |
| Business logic | Workers, one handler per task contract; small deterministic transformations as `compute` steps | Activities, called from workflow code |
| Control flow | Dependencies, conditions, fan-out, child workflows, sleep, waiting for signals and compensation, declared in the language | Anything the host language can express |
| Signals and queries | Declared in the workflow with typed payloads and results; every `await`, every use of a payload field and every query expression is type-checked at compile time, and the runtime validates each payload against its declaration | Defined in workflow code through SDK handlers |
| Static checks | Type checking plus warnings for missing compensations, constant conditions, unused signals and unbounded waits (`orbit check --strict`), and organisation [policies](tooling/policy.md) (`orbit check --policy`) | The host language's type checker and tooling |
| Changing a workflow | A new immutable `name@version`; running executions keep their definition; `orbit compat` reports breaking changes | Versioning APIs and worker build IDs |
| Failure handling | Compensation per step, then `on failure`, `on cancel` and `on timeout` handler steps | Code in the workflow, such as `try`/`catch` and cancellation scopes |
| Recurring messages | `on event <signal>` blocks run once per arriving signal, alongside the main steps | Signal handlers in workflow code |
| Recurring starts | [Schedules](runtime/schedules.md): cron expressions (with a time zone) or fixed intervals, with overlap and catch-up policies, managed with `/v1/schedules` and `orbit schedule` | Schedules |
| Long-running workflows | [`continue_as_new`](language/continue-as-new.md), declared in the workflow and type-checked against its input; each run has its own history | Continue-As-New, called from workflow code |
| Testing | `test` declarations run by `orbit test` with mocked tasks and simulated time; `orbit simulate` and `orbit replay` | SDK test frameworks with time skipping, and replay testing |
| Operations | A [web UI](runtime/web-ui.md) built into the server, the CLI, Prometheus metrics and OpenTelemetry traces | The Temporal Web UI, CLI, metrics and tracing |
| Process documentation | `orbit graph`, `orbit docs`, `orbit schema` and `orbit diff` generate diagrams, reference pages, JSON Schema and process diffs from the definition | Not built in |
| Deployment | One binary with SQLite or PostgreSQL; one server, active/standby servers, or several active servers sharing the work on PostgreSQL | A multi-service cluster with several supported databases |
| Features not yet in ORBIT | | Updates, Nexus, multi-region replication, a managed cloud |

ORBIT suits processes that you want to read, review and check as a whole,
across services in different languages. Temporal is a mature platform that
suits workflows whose logic is best expressed as general-purpose code.

### Do workers need the `.orbit` files?

No. A worker only needs the task names and contract versions it implements,
and the runtime's address and credentials. The compiler uses the `.orbit`
files; the runtime uses the compiled IR.

### Which languages can workers use?

The SDKs cover Python, TypeScript and JavaScript, Go, Rust, Java and C#. Any
other language can implement the worker protocol over HTTP/JSON or gRPC. See
[Worker protocol](reference/protocol.md).

### What happens if the runtime crashes?

Every state change is committed to the database before it takes effect. On
restart, the runtime reloads every unfinished execution, replays its history,
fires overdue timers and retries, and gives workers one session timeout
(30 seconds by default) to report results for tasks that were in flight. Tasks
that nobody reports are redelivered with the same idempotency key. The SDKs
keep retrying result delivery during an outage, so results produced while
the runtime was down are not lost.

### Can I change a workflow while executions are running?

Yes, by deploying a new version. Running executions stay pinned to the
definition hash they started with, and new executions can use the new version.
You cannot change the definition of a running execution.

### Where is the data stored?

In the database given by `--db`: a SQLite file (`orbit.db` in the current
directory by default) or a PostgreSQL URL. It holds definitions, executions
and the full event history. See [Storage](runtime/storage.md).

### Is there a web UI?

Yes. `orbit server` serves one at `/ui/`: executions with their steps,
history and graph, workflows, workers and schedules, and actions to run
queries, send signals and cancel executions. Disable it with `--no-ui`. See
[Web UI](runtime/web-ui.md). The CLI (`orbit ps`, `status`, `logs`,
`inspect`, `workers list`), Prometheus metrics at `/v1/metrics` and
OpenTelemetry traces are also available; see
[Observability](runtime/observability.md).

### Does ORBIT run on Windows and macOS?

Yes. Prebuilt binaries are published for Linux (x86_64, arm64) and Windows
(x86_64). On macOS, install with `cargo install orbit-lang`. See
[Installation](getting-started/installation.md).

## See also

- [Compiler diagnostics](reference/diagnostics.md)
- [Running the server](runtime/server.md)
- [Security](runtime/security.md)
- [Environment variables](reference/environment.md)
