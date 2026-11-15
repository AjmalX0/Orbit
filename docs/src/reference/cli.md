# CLI

The `orbit` binary is the compiler driver, the runtime server and the runtime client in one executable. This page lists every command and every flag, generated from `orbit <command> --help` for ORBIT 0.2, with the exit codes each command returns and examples you can run against a local server. Flags are long options (`--name value`) unless a short form is shown.

```text
orbit <COMMAND>

Commands:
  init      Create a new ORBIT project with an example workflow and workers
  check     Type-check workflow files without producing output
  validate  Check workflow files, and validate compiled IR (.json) files
  compile   Compile workflow files to orbit-ir JSON
  fmt       Format .orbit files in place (comments and line breaks are preserved)
  lsp       Run the language server (LSP over stdio), used by editor extensions
  graph     Render a workflow's step graph (Mermaid or Graphviz)
  compat    Check that new workflow and contract versions are compatible with old ones
  server    Run the ORBIT runtime server
  deploy    Compile and register workflow definitions with the runtime
  run       Deploy (if given a file) and start a workflow, then follow it to completion
  status    Show an execution's status and steps
  inspect   Inspect a compiled IR file, or an execution's full state
  logs      Show an execution's event history as a timeline
  cancel    Request cancellation of an execution (runs compensations)
  signal    Send a typed signal to a running execution
  query     Evaluate a query declared in the workflow against an execution
  ps        List recent executions
  token     Generate a random bearer token and the workers-file entry for it
  workers   Worker commands
  test      Run the `test` declarations in .orbit files, without a server or workers
  simulate  Run a workflow locally with mocked or generated task results
  replay    Re-run a recorded history through the current decider and report divergences
  help      Print this message or the help of the given subcommand(s)
```

## Global options

| Option | Meaning |
|---|---|
| `-h`, `--help` | Print help for the command. Available on every command. |
| `-V`, `--version` | Print the version (`orbit 0.1.0`). Top level only. |

## Exit codes

Every command uses the same convention, except `compat`, which also returns `2` for files that cannot be read or compiled (see [orbit compat](#orbit-compat)):

| Code | Meaning |
|---|---|
| `0` | Success. |
| `1` | The command ran and failed: a compilation error, an API error from the runtime, an unreachable runtime, a file that cannot be read or written, `fmt --check` found unformatted files, or `run` followed an execution that did not end `COMPLETED`. The message is printed on stderr, prefixed with `error:`. |
| `2` | Invalid usage: an unknown flag, a missing argument, a flag that requires another (for example `--tls-cert` without `--tls-key`), or an unreadable `--ca-cert`, `--client-cert` or `--client-key` file. |

API errors are printed as `error: <CODE> (<HTTP status>): <message>`, for example:

```text
error: EXECUTION_FINISHED (409): execution is already CANCELLED
```

## Output conventions

- Results go to stdout; progress messages (`deployed`, `started execution`, `formatted`) and diagnostics go to stderr. You can pipe stdout safely, for example `orbit run wf.orbit --no-wait` prints only the execution id on stdout.
- Colors are used only when the stream is a terminal and the `NO_COLOR` environment variable is not set.
- `inspect` (for executions), `logs --json` and the `output:` line of `status` redact values whose keys look sensitive. See [Security](../runtime/security.md#redaction).

## Runtime connection options

The commands that talk to a runtime (`deploy`, `run`, `status`, `inspect`, `logs`, `cancel`, `ps`, `workers list`) share these options:

| Option | Type | Default | Environment | Meaning |
|---|---|---|---|---|
| `--server <SERVER>` | URL | `http://127.0.0.1:7233` | `ORBIT_SERVER` | Runtime API address. Use `https://` for a server with TLS. |
| `--token <TOKEN>` | string | none | `ORBIT_TOKEN` | Bearer token sent as `Authorization: Bearer <token>`. Use the admin token; worker credentials cannot call these endpoints. An empty value means no token. |
| `--ca-cert <CA_CERT>` | path | none | `ORBIT_CA_CERT` | Extra PEM CA certificate to trust, for a runtime with a private certificate. It replaces the default roots for this connection. |
| `--client-cert <CLIENT_CERT>` | path | none | `ORBIT_CLIENT_CERT` | PEM client certificate for mutual TLS. Requires `--client-key`. |
| `--client-key <CLIENT_KEY>` | path | none | `ORBIT_CLIENT_KEY` | PEM private key for `--client-cert`. Requires `--client-cert`. |

The client waits at most 330 seconds for any single request. When `--server` points at a cluster standby, which answers `503 NOT_LEADER` with the leader's address, the client repeats the request once against that address. See [Running several servers](../runtime/cluster.md#clients-and-workers). A runtime that cannot be reached produces:

```text
error: cannot reach the ORBIT runtime at http://127.0.0.1:7233: io: Connection refused (os error 111)
  hint: start it with `orbit server` or set --server / ORBIT_SERVER
```

## orbit init

Create a new ORBIT project with an example workflow and workers.

```text
orbit init <NAME>
```

| Argument | Type | Meaning |
|---|---|---|
| `<NAME>` | path | Directory to create. It must not exist, or must be empty. |

Exit codes: `0` when the project was created; `1` when the directory exists and is not empty, or a file cannot be written.

```console
$ orbit init hello
  created hello/workers/typescript/package.json
  created hello/workers/python/requirements.txt
  created hello/.gitignore
  created hello/workflows/hello.orbit
  created hello/workers/python/worker.py
  created hello/workers/typescript/worker.ts
  created hello/README.md

Next: cd hello && orbit check workflows/hello.orbit
$ orbit init hello
error: hello already exists and is not empty
```

## orbit check

Type-check workflow files without producing output.

```text
orbit check [OPTIONS] [FILES]...
```

| Argument or option | Type | Default | Meaning |
|---|---|---|---|
| `[FILES]...` | paths | none | One or more `.orbit` files. Imports are followed. |
| `--format <FORMAT>` | `human` or `json` | `human` | `human` prints diagnostics on stderr and one `ok` line per workflow on stdout. `json` prints `{"diagnostics": [...]}` on stdout and nothing else. |
| `--strict` | flag | off | Treat warnings (missing compensations, unreachable steps, unused signals, waits without a timeout, ...) as errors. |

Exit codes: `0` when there are no errors (warnings are allowed, unless `--strict` is given); `1` when any file has an error, when `--strict` is given and there is any warning, or when no files are given. With `--strict` and only warnings, the human format prints the warnings, then `error: <n> warning(s) treated as errors (--strict)`, and no `ok` lines.

```console
$ orbit check examples/order_processing.orbit
ok OrderProcessing@1.0.0 (5 steps)
$ orbit check examples/contracts.orbit
ok no workflows (contracts only)
```

A file with an error:

```console
$ orbit check messy.orbit
error[E0300]: unknown worker task `greeting.compose`
 --> messy.orbit:6:25
  |
6 |     step greet { worker "greeting.compose" // the task
  |                         ^^^^^^^^^^^^^^^^^^
  = help: declare its contract: `task "greeting.compose" { version "1.0.0" input { ... } output { ... } }`

error: compilation failed: 1 error(s), 0 warning(s)
```

The same file with `--format json`:

```json
{
  "diagnostics": [
    {
      "severity": "error",
      "code": "E0300",
      "message": "unknown worker task `greeting.compose`",
      "location": { "file": "messy.orbit", "line": 6, "column": 25 },
      "end_line": 6,
      "end_column": 43,
      "labels": [],
      "notes": [],
      "help": "declare its contract: `task \"greeting.compose\" { version \"1.0.0\" input { ... } output { ... } }`"
    }
  ]
}
```

With `--strict`:

```console
$ orbit check --strict review.orbit
warning[W0605]: signal `escalate` is declared but no step awaits it
 --> review.orbit:5:12
  |
5 |     signal escalate { reason: String }
  |            ^^^^^^^^
  = help: add a step with `await escalate`, or remove the declaration

warning[W0606]: step `decision` waits for signal `review` with no timeout
 --> review.orbit:7:27
  |
7 |     step decision { await review }
  |                           ^^^^^^
  = help: add `timeout 7d` to the step (or a workflow `timeout`) so the execution cannot wait forever

error: 2 warning(s) treated as errors (--strict)
```

Every code is described in [Compiler diagnostics](diagnostics.md). See [Static safety checks](../language/safety.md) for the `W06xx` warnings.

## orbit validate

Check workflow files, and validate compiled IR (`.json`) files.

```text
orbit validate [FILES]...
```

| Argument | Type | Meaning |
|---|---|---|
| `[FILES]...` | paths | Any mix of `.orbit` source files and compiled `.json` IR files. Source files are checked as with `orbit check`. IR files are parsed and validated, including their `definition_hash`. |

Exit codes: `0` when every file is valid; `1` otherwise.

```console
$ orbit validate build/demo.Demo.orbit.json plain.orbit
ok build/demo.Demo.orbit.json is valid orbit-ir/1 (Demo@1.0.0, sha256:d5f75dbf860e1c8471f72adb871698e87f55286496dc3547267dcfe383496418)
ok Plain@1.0.0 (1 steps)
```

## orbit compile

Compile workflow files to orbit-ir JSON. Each workflow is compiled to `orbit-ir/1`, or to `orbit-ir/2` when it uses signals, queries, `await` or `compute` steps, or lambdas (see [Versioning](../language/versioning.md#ir-version)).

```text
orbit compile [OPTIONS] [FILES]...
```

| Argument or option | Type | Default | Meaning |
|---|---|---|---|
| `[FILES]...` | paths | none | One or more `.orbit` files. |
| `--strict` | flag | off | Treat warnings as errors: with any warning, print the diagnostics and `error: <n> warning(s) treated as errors (--strict)`, write nothing and exit `1`. |
| `-o`, `--out-dir <OUT_DIR>` | path | `build` | Output directory, created if needed. |
| `--stdout` | flag | off | Print the IR to stdout instead of writing files. |

Each workflow is written to `<out-dir>/<file stem>.orbit.json`. When one file defines several workflows, each is written to `<out-dir>/<file stem>.<Workflow>.orbit.json`.

Exit codes: `0` on success; `1` on compilation errors, on warnings with `--strict`, when no files are given, or when the files contain no workflow declarations (`nothing to compile: no workflow declarations found`).

```console
$ orbit compile demo.orbit -o build
compiled Demo@1.0.0 -> build/demo.Demo.orbit.json
compiled Queue@1.0.0 -> build/demo.Queue.orbit.json
$ orbit compile plain.orbit --stdout | head -5
{
  "ir_version": "orbit-ir/1",
  "name": "Plain",
  "version": "1.0.0",
  "definition_hash": "sha256:cc0640cd8d256b01c327613f0ab70930d5e9f1945f3adf8e08662381caafabd3",
```

## orbit fmt

Format `.orbit` files in place. Comments and line breaks are preserved. See [Formatter](../tooling/fmt.md) for the rules.

```text
orbit fmt [OPTIONS] [FILES]...
```

| Argument or option | Type | Default | Meaning |
|---|---|---|---|
| `[FILES]...` | paths | none | Files to format. |
| `--check` | flag | off | Do not write. Print `would reformat <file>` for each file that is not formatted. |

Exit codes: `0` when every file is formatted (or was rewritten); `1` when `--check` finds an unformatted file, a file has lexical errors, or no files are given.

```console
$ orbit fmt --check messy.orbit
would reformat messy.orbit
$ orbit fmt messy.orbit
formatted messy.orbit
```

## orbit lsp

Run the language server (LSP over stdio), used by editor extensions. It takes no options and exits when the client sends `exit` or closes stdin. See [Editor support](../tooling/editors.md).

```text
orbit lsp
```

Exit codes: `0` when the client ends the session; `1` on an I/O error.

## orbit graph

Render a workflow's step graph as Mermaid or Graphviz. See [Workflow graphs](../tooling/graph.md).

```text
orbit graph [OPTIONS] <FILE>
```

| Argument or option | Type | Default | Meaning |
|---|---|---|---|
| `<FILE>` | path | required | A `.orbit` file or a compiled `.json` IR file. |
| `--workflow <WORKFLOW>` | string | see below | Workflow to render when the file defines several. Without it, a file with one workflow renders that workflow, and a file with a parent and its children renders the one workflow no other workflow in the file starts. |
| `--format <FORMAT>` | `mermaid` or `dot` | `mermaid` | Output format. |

Exit codes: `0` on success; `1` on compilation errors, an unknown `--workflow`, or an ambiguous file.

```console
$ orbit graph examples/fulfilment.orbit --workflow Nope
error: no workflow `Nope`; available: Shipping, Fulfilment
```

## orbit compat

Check that new workflow and contract versions are compatible with old ones. See [Compatibility checks](../tooling/compat.md) for every rule.

```text
orbit compat [OPTIONS] <OLD> <NEW>
```

| Argument or option | Type | Default | Meaning |
|---|---|---|---|
| `<OLD>` | path | required | The old version: a `.orbit` file or a compiled IR `.json` file. |
| `<NEW>` | path | required | The new version: a `.orbit` file or a compiled IR `.json` file. |
| `--format <FORMAT>` | `human` or `json` | `human` | `human` prints one line per finding on stdout. `json` prints `{"compatible", "errors", "warnings", "findings": [...]}` on stdout and nothing else. |

Exit codes: `0` when there are no errors (warnings are allowed); `1` when any finding is an error; `2` when a file cannot be read, is not valid IR, or does not compile, or an argument is missing. Unlike the other commands, `compat` uses `2` for compile errors, so that CI can tell an incompatible change from a broken input.

```console
$ orbit compat v1.orbit minor.orbit
error   workflow Invoice 1.4.0 → 1.5.0: input.amount: Float → Int: incompatible type: expected `Int`, found `Float`; breaking for callers without a major version bump; hint: bump the major version (1.4.0 → 2.0.0)
error   workflow Invoice 1.4.0 → 1.5.0: input.currency: (added) → String: required field added without a default; breaking for callers without a major version bump; hint: bump the major version (1.4.0 → 2.0.0)
ok      workflow Invoice 1.4.0 → 1.5.0: output.total: Float → Int: narrowed; still accepted where the old type is expected
error: compatibility check failed: 2 error(s), 0 warning(s)
$ orbit compat v1.orbit major.orbit
info    workflow Invoice 1.4.0 → 2.0.0: input.amount: Float → Int: incompatible type: expected `Int`, found `Float`; breaking for callers, allowed by the major version bump
info    workflow Invoice 1.4.0 → 2.0.0: input.currency: (added) → String: required field added without a default; breaking for callers, allowed by the major version bump
ok      workflow Invoice 1.4.0 → 2.0.0: output.total: Float → Int: narrowed; still accepted where the old type is expected
```

## orbit test

Run the `test` declarations in `.orbit` files, without a server or workers. See [Testing workflows](../tooling/testing.md).

```text
Run the `test` declarations in .orbit files, without a server or workers

Usage: orbit test [OPTIONS] [FILES]...

Arguments:
  [FILES]...  Files with `test` declarations (and the workflows they import)

Options:
      --format <FORMAT>  `human` prints one line per test; `json` prints a single report object [default: human] [possible values: human, json]
      --filter <FILTER>  Only run tests whose name contains this text
  -h, --help             Print help
```

| Argument or option | Type | Default | Meaning |
|---|---|---|---|
| `[FILES]...` | paths | required | Files to compile. Every test in them and in the files they import runs. |
| `--format <FORMAT>` | `human` or `json` | `human` | `human` prints one line per test, failure details with the execution timeline, and a summary. `json` prints one report object on stdout; see [JSON format](../tooling/testing.md#json-format). |
| `--filter <FILTER>` | string | none | Run only tests whose name contains this text. |

Exit codes: `0` when every selected test passed, or no test was selected; `1` when a test failed or the files do not compile.

```console
$ orbit test examples/*.test.orbit --filter retried
running 1 test
ok      risk scoring is retried after temporary failures (3.0s simulated)

test result: ok. 1 passed, 0 failed, 9 filtered out (0.01s)
```

## orbit simulate

Run one execution of a workflow locally, with mocked or generated task results. See [Simulation](../tooling/simulate.md).

```text
Run a workflow locally with mocked or generated task results

Usage: orbit simulate [OPTIONS] <FILE>

Arguments:
  <FILE>  A `.orbit` file or compiled IR `.json`

Options:
      --workflow <WORKFLOW>      Workflow to run when the file defines several
  -i, --input <INPUT>            Workflow input as JSON
      --input-file <INPUT_FILE>  Read the workflow input from a JSON file
      --mocks <MOCKS>            JSON file mapping task names to the results they return
      --fail <TASK=CODE>         Make a task fail permanently: `<task>=<ERROR_CODE>` (repeatable)
      --signal <NAME=JSON>       Send a signal once the execution waits for it: `<name>='<json>'` (repeatable, in order)
      --json                     Print the final state, history and child executions as JSON
  -h, --help                     Print help
```

| Argument or option | Type | Default | Meaning |
|---|---|---|---|
| `<FILE>` | path | required | A `.orbit` file or a compiled IR `.json` file. |
| `--workflow <WORKFLOW>` | string | see [orbit graph](#orbit-graph) | Workflow to run when the file defines several. |
| `-i`, `--input <INPUT>` | JSON | `{}` | The execution input. |
| `--input-file <INPUT_FILE>` | path | none | Read the input from a JSON file. |
| `--mocks <MOCKS>` | path | none | A JSON object mapping task names to results. Tasks not listed get a [generated result](../tooling/simulate.md#generated-results). |
| `--fail <TASK=CODE>` | string | none | Fail every attempt of the task with this code (not retryable). Repeatable. |
| `--signal <NAME=JSON>` | string | none | Send this signal once the execution waits for it. `NAME=@file.json` reads the payload from a file. Repeatable, sent in order. |
| `--json` | flag | off | Print the result, state, history and child executions as one JSON object. |

Exit codes: `0` when the execution ends `COMPLETED`; `1` when it ends otherwise, cannot finish, or the file does not compile.

```console
$ orbit simulate examples/approval.orbit --input '{"order_id":"T1","customer_id":"c-1","amount":10}' --fail notifications.send=MAILBOX_FULL --json | jq -c '{status, error: .error.code, sampled_tasks}'
{"status":"FAILED","error":"MAILBOX_FULL","sampled_tasks":["payments.charge","payments.refund"]}
```

## orbit replay

Re-run a recorded history through the current decider and report every recorded decision it does not reproduce; with `--at`, show the execution's state after any event. See [Replay](../tooling/replay.md).

```text
Re-run a recorded history through the current decider and report divergences

Usage: orbit replay [OPTIONS] [EXECUTION_ID]

Arguments:
  [EXECUTION_ID]  Execution to replay; its history and definition are fetched from the runtime

Options:
      --history <HISTORY>          Replay a history file instead (`GET /v1/executions/{id}/history` output)
      --definition <DEFINITION>    The definition to replay against: compiled IR `.json` or `.orbit` source (required with --history; overrides the runtime's copy otherwise)
      --workflow <WORKFLOW>        Workflow to use when --definition defines several (default: the one the history ran)
      --at <SEQ>                   Print the reconstructed execution state right after this event
      --json                       Print the report as JSON
      --server <SERVER>            Runtime API address [env: ORBIT_SERVER=] [default: http://127.0.0.1:7233]
      --token <TOKEN>              Bearer token for the runtime API [env: ORBIT_TOKEN]
      --ca-cert <CA_CERT>          Extra PEM CA certificate to trust (for a runtime with a private certificate) [env: ORBIT_CA_CERT=]
      --client-cert <CLIENT_CERT>  PEM client certificate for mutual TLS (with --client-key) [env: ORBIT_CLIENT_CERT=]
      --client-key <CLIENT_KEY>    PEM private key for --client-cert [env: ORBIT_CLIENT_KEY=]
  -h, --help                       Print help
```

| Argument or option | Type | Default | Meaning |
|---|---|---|---|
| `[EXECUTION_ID]` | string | none | Replay this execution: its history and the definition it ran are fetched from the runtime. |
| `--history <HISTORY>` | path | none | Replay a history file instead of an execution id. |
| `--definition <DEFINITION>` | path | the runtime's copy | Compiled IR `.json` or `.orbit` source to replay against. Required with `--history`. |
| `--workflow <WORKFLOW>` | string | from the history | Workflow to use when `--definition` defines several. |
| `--at <SEQ>` | integer | none | Stop after this event and print the reconstructed state. |
| `--json` | flag | off | Print the report as one JSON object; see [JSON report](../tooling/replay.md#json-report). |
| runtime connection options | | | See [Runtime connection options](#runtime-connection-options). Used only with an execution id. |

Exit codes: `0` when every recorded decision is reproduced; `1` on a divergence, an API or connection error, or a history or definition that cannot be loaded; `2` when both an execution id and `--history` are given.

```console
$ orbit replay rem-1
replayed 9 events of rem-1 against Reminder@1.0.0 with the current decider
  6 decided, 3 external
consistent: every recorded decision is reproduced
```

## orbit server

Run the ORBIT runtime server. [Running the server](../runtime/server.md) explains each option in context.

```text
orbit server [OPTIONS]
```

| Option | Type | Default | Environment | Meaning |
|---|---|---|---|---|
| `--db <DB>` | path or URL | `orbit.db` | `ORBIT_DB` | Where definitions and execution history live: a SQLite file path, or a PostgreSQL URL such as `postgres://user:pass@host:5432/orbit`. |
| `--addr <ADDR>` | `host:port` | `127.0.0.1:7233` | `ORBIT_ADDR` | Address of the HTTP API, including the HTTP/JSON worker protocol. Must be an IP address and port. |
| `--otlp-endpoint <OTLP_ENDPOINT>` | URL | none | `OTEL_EXPORTER_OTLP_ENDPOINT` | Export OpenTelemetry traces (OTLP/HTTP, JSON) to this collector, for example `http://localhost:4318`. Honors `OTEL_EXPORTER_OTLP_HEADERS` and `OTEL_SERVICE_NAME`. |
| `--grpc-addr <GRPC_ADDR>` | `host:port` | none | `ORBIT_GRPC_ADDR` | Also serve the gRPC binding of the worker protocol on this address, with the same credentials and TLS settings as the HTTP API. |
| `--token <TOKEN>` | string | none | `ORBIT_TOKEN` | Require this bearer token on every API call except `/v1/health`. |
| `--insecure` | flag | off | | Allow binding to a non-loopback address without a token. |
| `--worker-token <WORKER_TOKEN>` | string | none | `ORBIT_WORKER_TOKEN` | Second credential that may only use the worker protocol (`/v1/workers/*`, `/v1/tasks/*`). Give it to worker hosts instead of the admin token. Requires `--token` and must differ from it. |
| `--tls-cert <TLS_CERT>` | path | none | `ORBIT_TLS_CERT` | PEM certificate chain. With `--tls-key`, serve HTTPS. |
| `--tls-key <TLS_KEY>` | path | none | `ORBIT_TLS_KEY` | PEM private key for `--tls-cert`. |
| `--tls-client-ca <TLS_CLIENT_CA>` | path | none | `ORBIT_TLS_CLIENT_CA` | Enable mutual TLS: verify client certificates signed by this PEM CA. Certificates identify workers declared with `client_cert` in `--workers-file`. Requires `--tls-cert`. |
| `--workers-file <WORKERS_FILE>` | path | none | `ORBIT_WORKERS_FILE` | TOML file declaring per-worker identities (`[[worker]]`: id, token hash or client certificate name, allowed task patterns) and client identities (`[[client]]`: id, credential, allowed workflows and actions). Requires `--token`. See [Security](../runtime/security.md#client-identities). |
| `--enable-http-adapter` | flag | off | | Execute `orbit.http.request` steps inside the runtime. |
| `--max-active-executions <MAX_ACTIVE_EXECUTIONS>` | integer | unlimited | `ORBIT_MAX_ACTIVE_EXECUTIONS` | Maximum running executions; further starts get `429 RESOURCE_EXHAUSTED`. |
| `--max-fan-out <MAX_FAN_OUT>` | integer | unlimited | `ORBIT_MAX_FAN_OUT` | Maximum elements one `for` step may fan out to; larger lists fail the step with `LIMIT_EXCEEDED`. |
| `--max-history-events <MAX_HISTORY_EVENTS>` | integer | unlimited | `ORBIT_MAX_HISTORY_EVENTS` | Maximum history events per execution; past it the execution fails with `LIMIT_EXCEEDED`. |
| `--max-payload-bytes <MAX_PAYLOAD_BYTES>` | integer, bytes | unlimited | `ORBIT_MAX_PAYLOAD_BYTES` | Maximum size in bytes of an execution input, task result or signal payload; larger ones get `413 PAYLOAD_TOO_LARGE`. |
| `--session-timeout <SESSION_TIMEOUT>` | integer, seconds | `30` | | Seconds without contact after which a worker is considered lost. |
| `--max-body <MAX_BODY>` | integer, bytes | `4194304` | | Maximum request body size. |
| `--log-format <LOG_FORMAT>` | `text` or `json` | `text` | | Log format. Logs go to stderr. |
| `--cluster` | flag | off | `ORBIT_CLUSTER` | Run as one of several servers on the same PostgreSQL database. By default one leader serves and the others stand by and take over if it fails. Requires a PostgreSQL `--db`. See [Running several servers](../runtime/cluster.md). |
| `--cluster-mode <CLUSTER_MODE>` | `standby` or `shared` | `standby` | `ORBIT_CLUSTER_MODE` | With `shared`, every server is active and they share executions and worker tasks. `shared` requires `--cluster`. See [Shared mode](../runtime/cluster.md#shared-mode-several-active-servers). |
| `--node-timeout <NODE_TIMEOUT>` | integer, seconds | `5` | `ORBIT_NODE_TIMEOUT` | Shared mode: seconds without a heartbeat after which a server is considered dead and the others take over its executions (minimum 2). |
| `--node-id <NODE_ID>` | string | the host name | `ORBIT_NODE_ID` | This server's name in the cluster. Must be unique among running servers. Requires `--cluster`. |
| `--advertise-addr <ADVERTISE_ADDR>` | URL | derived from `--addr` unless it is a wildcard address | `ORBIT_ADVERTISE_ADDR` | URL other servers give out to reach this one while it leads, and that shared mode lists in `/v1/health`. Requires `--cluster`. |

The cluster options as `orbit server --help` prints them:

```console
$ orbit server --help
...
      --cluster
          Run as one of several servers on the same PostgreSQL database. By default one leader serves and the others stand by and take over if it fails; see --cluster-mode [env: ORBIT_CLUSTER=]
      --cluster-mode <CLUSTER_MODE>
          With --cluster: `standby` (one leader serves, the others stand by) or `shared` (every server is active and they share executions and tasks) [env: ORBIT_CLUSTER_MODE=] [default: standby] [possible values: standby, shared]
      --node-timeout <NODE_TIMEOUT>
          Shared mode: seconds without a heartbeat after which a server is considered dead and the others take over its executions (minimum 2) [env: ORBIT_NODE_TIMEOUT=] [default: 5]
      --node-id <NODE_ID>
          This server's name in the cluster (default: the host name) [env: ORBIT_NODE_ID=]
      --advertise-addr <ADVERTISE_ADDR>
          URL other servers give out to reach this one while it leads, and that shared mode lists in /v1/health, e.g. http://orbit-0.orbit:7233 (default: derived from --addr when it is not a wildcard address) [env: ORBIT_ADVERTISE_ADDR=]
  -h, --help
          Print help
```

The server reads `ORBIT_LOG` (log filter) and `ORBIT_DB_CA_CERT` (extra CA for PostgreSQL) from the environment. See [Environment variables](environment.md). The four `--max-*` limits are described in [Running the server: resource limits](../runtime/server.md#resource-limits).

Exit codes: `0` after a graceful shutdown (Ctrl+C or `SIGINT`); `1` when the configuration is refused, the database cannot be opened, or an address cannot be bound; `2` on invalid usage.

Configuration that the server refuses:

```console
$ orbit server --addr 0.0.0.0:7233
error: refusing to listen on 0.0.0.0:7233 without authentication; pass --token (or ORBIT_TOKEN), or --insecure for trusted networks
$ orbit server --worker-token abc
error: --worker-token requires --token: without an admin token every endpoint would be open
$ orbit server --token abc --worker-token abc
error: --worker-token must differ from --token
$ orbit server --workers-file workers.toml
error: --workers-file requires --token: without an admin token administrative endpoints would be open
$ orbit server --token t --tls-cert server.pem --tls-key server.key --tls-client-ca ca.pem
error: --tls-client-ca needs --workers-file entries with `client_cert` to map certificates to workers
$ orbit server --cluster --db orbit.db
error: --cluster requires a PostgreSQL database (--db postgres://...): the servers share it for leader election and history
$ orbit server --cluster-mode shared --db postgres://orbit@db/orbit
error: --cluster-mode shared requires --cluster
```

## orbit deploy

Compile and register workflow definitions with the runtime.

```text
orbit deploy [OPTIONS] [FILES]...
```

| Argument or option | Type | Default | Meaning |
|---|---|---|---|
| `[FILES]...` | paths | none | `.orbit` files or compiled `.json` IR files. Every workflow in every file is registered, children before parents. |
| runtime connection options | | | See [Runtime connection options](#runtime-connection-options). |

For each workflow, `deploy` prints `deployed` when the definition is new and `up to date` when an identical definition is already registered. The hash prefix identifies the definition.

Exit codes: `0` when every definition is registered or unchanged (including when no files are given); `1` on compilation errors, `DEFINITION_CONFLICT`, `CHILD_NOT_REGISTERED`, or any other API error.

```console
$ orbit deploy examples/order_processing.orbit
deployed OrderProcessing@1.0.0 (sha256:4fdfac082bf3)
$ orbit deploy examples/order_processing.orbit
up to date OrderProcessing@1.0.0 (sha256:4fdfac082bf3)
```

Changing a definition without changing its version is refused:

```console
$ orbit deploy order_processing.orbit
error: DEFINITION_CONFLICT (409): OrderProcessing@1.0.0 is already registered with a different definition (sha256:4fdfac082bf3f2fbbbc8ac5465d37cba01dffd1b8e4ae08e25be3e0d76486c41); workflow definitions are immutable, bump the version
```

## orbit run

Deploy (if given a file) and start a workflow, then follow it to completion.

```text
orbit run [OPTIONS] <TARGET>
```

| Argument or option | Type | Default | Meaning |
|---|---|---|---|
| `<TARGET>` | string | required | A `.orbit` file, a compiled IR `.json` file, or the name of a registered workflow. A file is deployed first: every workflow in it is registered. |
| `--workflow <WORKFLOW>` | string | see [`graph`](#orbit-graph) | Workflow to run when the file defines several. Ignored when `<TARGET>` is a workflow name. |
| `--version <VERSION>` | string | latest | Version to start when `<TARGET>` is a workflow name. Without it, the highest registered version (by semantic version order) is started. Ignored for files, which start the version they define. |
| `-i`, `--input <INPUT>` | JSON | `{}` | Workflow input as a JSON value. |
| `--input-file <INPUT_FILE>` | path | none | Read the workflow input from a JSON file. `--input` takes precedence when both are given. |
| `--id <ID>` | string | generated UUID | Client-chosen execution id. Starting the same id twice is a no-op: the second call attaches to the existing execution. Letters, digits, `-`, `_`, `.` and `:`, at most 200 characters. |
| `--no-wait` | flag | off | Return immediately after starting, printing the execution id on stdout. |
| runtime connection options | | | See [Runtime connection options](#runtime-connection-options). |

Without `--no-wait`, `run` prints each step transition on stderr until the execution finishes, then prints a summary on stderr and the workflow output, as JSON, on stdout.

Exit codes: `0` when the execution ends `COMPLETED`, or was started with `--no-wait`; `1` when it ends `FAILED`, `CANCELLED` or `TIMED_OUT`, or on any error.

```console
$ orbit run examples/order_processing.orbit --no-wait \
    -i '{"order_id":"o-1","customer_id":"c-1","amount":42.5,"items":[{"sku":"A1","quantity":2}]}'
up to date OrderProcessing@1.0.0 (sha256:4fdfac082bf3)
started execution b013f1d7-09ff-45c2-ae1f-614471942a54
b013f1d7-09ff-45c2-ae1f-614471942a54
```

Following an execution that times out (`demo.orbit` declares `timeout 3s` and no worker serves `demo.work`):

```console
$ orbit run demo.orbit --workflow Demo --id timeout-1
up to date Demo@1.0.0 (sha256:d5f75dbf860e)
up to date Queue@1.0.0 (sha256:0e3a218e9ed9)
started execution timeout-1
  ✓ pause                          succeeded 283ms
  · work                           scheduled
  ⊘ work                           cancelled

TIMED_OUT timeout-1  Demo@1.0.0
  elapsed: 3.1s
  compensation: none
```

Starting a registered workflow by name:

```console
$ orbit run HttpPing --id ping-1 -i '{"url":"http://example.invalid"}'
started execution ping-1
  ✗ probe                          failed the HTTP adapter is disabled; start the server with --enable-http-adapter

FAILED ping-1  HttpPing@1.0.0
  elapsed: 90ms
  compensation: none
  error: ADAPTER_DISABLED step `probe` failed: the HTTP adapter is disabled; start the server with --enable-http-adapter
```

## orbit status

Show an execution's status and steps.

```text
orbit status [OPTIONS] <EXECUTION_ID>
```

| Argument or option | Type | Meaning |
|---|---|---|
| `<EXECUTION_ID>` | string | The execution to show. |
| runtime connection options | | See [Runtime connection options](#runtime-connection-options). |

The header shows the status, id, workflow and version, the elapsed time, the compensation outcome and the error, when present. Each unit that has been touched is listed in the order it last changed, with its status, attempt number, duration and a detail column (the error message, or the worker that ran it). Units that have not been scheduled yet are not listed.

Exit codes: `0` when the execution exists; `1` otherwise (`NOT_FOUND (404)`).

```console
$ orbit status order-1002
FAILED order-1002  OrderProcessing@1.0.0
  elapsed: 1.6s
  compensation: completed
  error: CARD_DECLINED step `process_payment` failed: card was declined

  STEP                         STATUS       ATTEMPT     TIME  DETAIL
  validate_order               SUCCEEDED          1      3ms  worker ts-1
  reserve_inventory            SUCCEEDED          1      2ms  worker ts-1
  analyze_risk                 SUCCEEDED          2      2ms  worker py-1
  process_payment              FAILED             1      4ms  card was declined
  send_notification            CANCELLED          0
  compensate:reserve_inventory SUCCEEDED          1      1ms  worker ts-1
```

## orbit inspect

Inspect a compiled IR file, or an execution's full state.

```text
orbit inspect [OPTIONS] <TARGET>
```

| Argument or option | Type | Meaning |
|---|---|---|
| `<TARGET>` | string | When it names an existing file: a `.json` IR file, or a `.orbit` file (compiled first), summarized as text. Otherwise it is an execution id, and the full execution state is printed as JSON with sensitive values redacted. |
| runtime connection options | | Used only for execution ids. See [Runtime connection options](#runtime-connection-options). |

Exit codes: `0` on success; `1` on compilation errors, a file that defines several unrelated workflows, or an unknown execution.

```console
$ orbit inspect examples/order_processing.orbit
workflow OrderProcessing@1.0.0
  Validate, risk-score, reserve, charge and notify for one order
  ir:      orbit-ir/1
  hash:    sha256:4fdfac082bf3f2fbbbc8ac5465d37cba01dffd1b8e4ae08e25be3e0d76486c41
  input:   { order_id: String, customer_id: String, amount: Float, currency: String, items: List<LineItem> }
  output:  { status: String, transaction_id: String?, risk_score: Float }
  timeout: 5m00s

steps (execution order)
  validate_order         orders.validate@1.0.0 using typescript, timeout 10.0s
  analyze_risk           risk.analyze@1.0.0 using python, retry 3x exponential
  reserve_inventory      inventory.reserve@1.0.0 using typescript after validate_order
                         ↺ compensate inventory.release
  process_payment        payments.charge@1.0.0 using python, retry 5x exponential, timeout 30.0s after analyze_risk, reserve_inventory [conditional]
                         ↺ compensate payments.refund
  send_notification      notifications.send@1.0.0 using typescript after process_payment

contracts
  inventory.release@1.0.0
    in:  { reservation_id: String }
    out: { released: Bool }
  ...
```

For an execution id, the output is the same document as [`GET /v1/executions/{id}`](http-api.md#get-v1executionsid):

```console
$ orbit inspect u-1
{
  "id": "u-1",
  "workflow": "Plain",
  "version": "1.0.0",
  "definition_hash": "sha256:cc0640cd8d256b01c327613f0ab70930d5e9f1945f3adf8e08662381caafabd3",
  "input": {},
  "status": "COMPLETED",
  ...
}
```

## orbit logs

Show an execution's event history as a timeline.

```text
orbit logs [OPTIONS] <EXECUTION_ID>
```

| Argument or option | Type | Default | Meaning |
|---|---|---|---|
| `<EXECUTION_ID>` | string | required | The execution to show. |
| `--json` | flag | off | Print raw JSON events, one per line, with sensitive fields redacted. |
| runtime connection options | | | See [Runtime connection options](#runtime-connection-options). |

The timeline shows the time since the first event, the sequence number, the event type and the fields `step`, `attempt`, `worker_id`, `reason`, `delay_ms`, `redelivery`, `outcome` and `error` when the event has them. [Executions and history](../runtime/executions.md#history-events) describes every event type.

Exit codes: `0` when the execution exists; `1` otherwise.

```console
$ orbit logs u-1
+     0ms   #1  WorkflowCreated
+     0ms   #2  WorkflowStarted
+     1ms   #3  StepScheduled             step=work attempt=1
+    39ms   #4  StepDispatched            step=work attempt=1 worker_id=flaky-1
+    5.1s   #5  StepAttemptUncertain      step=work attempt=1 reason=worker `flaky-1` stopped responding
+    5.1s   #6  StepScheduled             step=work attempt=1 redelivery=true
+    6.6s   #7  StepDispatched            step=work attempt=1 worker_id=flaky-2
+    6.7s   #8  StepSucceeded             step=work attempt=1
+    6.7s   #9  WorkflowCompleted
$ orbit logs u-1 --json | tail -2
{"seq":8,"timestamp_ms":1794735434127,"type":"StepSucceeded","step":"work","attempt":1,"output":{"ok":true}}
{"seq":9,"timestamp_ms":1794735434129,"type":"WorkflowCompleted","output":{}}
```

## orbit cancel

Request cancellation of an execution. Work that has not started is withdrawn, in-flight attempts are awaited, and succeeded steps are compensated. See [Cancellation](../runtime/executions.md#cancellation).

```text
orbit cancel [OPTIONS] <EXECUTION_ID>
```

| Argument or option | Type | Default | Meaning |
|---|---|---|---|
| `<EXECUTION_ID>` | string | required | The execution to cancel. |
| `--reason <REASON>` | string | `requested via API` | Reason recorded in the `CancelRequested` event. |
| runtime connection options | | | See [Runtime connection options](#runtime-connection-options). |

The command prints the execution status right after the request. An execution with nothing in flight and nothing to compensate is already `CANCELLED`; otherwise it is still `RUNNING` or `COMPENSATING` and reaches `CANCELLED` later.

Exit codes: `0` when cancellation was requested (or was already requested); `1` when the execution is unknown (`NOT_FOUND`) or already finished (`EXECUTION_FINISHED`).

```console
$ orbit cancel c-1 --reason "duplicate order"
cancellation requested; execution is CANCELLED
$ orbit cancel c-1
error: EXECUTION_FINISHED (409): execution is already CANCELLED
```

## orbit signal

Send a typed signal to a running execution. The runtime validates the payload against the workflow's `signal` declaration and buffers it until an `await` step takes it. See [Signals and queries](../language/signals.md).

```text
orbit signal [OPTIONS] <EXECUTION_ID> <NAME>
```

| Argument or option | Type | Default | Meaning |
|---|---|---|---|
| `<EXECUTION_ID>` | string | required | The execution to signal. |
| `<NAME>` | string | required | The signal name declared in the workflow (`signal <name> { ... }`). |
| `--data <DATA>` | JSON or `@path` | `{}` | The payload as JSON. `@file.json` reads it from a file. |
| runtime connection options | | | See [Runtime connection options](#runtime-connection-options). |

On success the command prints the history sequence number of the recorded `SignalReceived` event on stdout. "Delivered" here means that the runtime recorded the signal; whether a step has taken it shows in the history as `SignalDelivered`.

Exit codes: `0` when the signal was recorded; `1` when `--data` is not valid JSON or the file cannot be read, or on an API error: `NOT_FOUND`, `UNKNOWN_SIGNAL` (404), `EXECUTION_FINISHED` (409) or `SIGNAL_PAYLOAD_INVALID` (422).

```console
$ orbit signal order-A-100 reveiw --data '{}'
error: UNKNOWN_SIGNAL (404): OrderApproval@1.0.0 has no signal `reveiw`; declared: review
$ orbit signal order-A-100 review --data '{"approved": "yes", "reviewer": "dana"}'
error: SIGNAL_PAYLOAD_INVALID (422): payload for signal `review` is invalid at $.approved: expected Bool, found a string
$ orbit signal order-A-100 review --data 'x'
error: --data is not valid JSON: expected value at line 1 column 1
$ orbit signal order-A-100 review --data '{"approved": true, "reviewer": "dana", "comment": "verified by phone"}'
signal `review` recorded for order-A-100 (history #5); the next `await review` step takes it
$ orbit signal order-A-100 review --data @review.json
error: EXECUTION_FINISHED (409): execution is already COMPLETED
```

The execution in this example completes as soon as the signal is delivered, so the last command fails. An execution that is already terminal returns `EXECUTION_FINISHED` before the signal name and payload are checked.

## orbit query

Evaluate a query declared in the workflow against the current state of an execution, and print its value as JSON. Works on running and finished executions. See [Signals and queries](../language/signals.md#queries).

```text
orbit query [OPTIONS] <EXECUTION_ID> <NAME>
```

| Argument or option | Type | Default | Meaning |
|---|---|---|---|
| `<EXECUTION_ID>` | string | required | The execution to query. |
| `<NAME>` | string | required | The query name declared in the workflow (`query <name> = ...`). |
| runtime connection options | | | See [Runtime connection options](#runtime-connection-options). |

The value is printed on stdout as indented JSON, without the surrounding response object, so it can be piped to `jq`.

Exit codes: `0` on success; `1` on an API error: `NOT_FOUND`, `UNKNOWN_QUERY` (404) or `QUERY_FAILED` (422).

```console
$ orbit query order-A-100 status
{
  "needs_approval": true,
  "approved": null,
  "reviewer": null,
  "charged": false
}
$ orbit query order-A-100 stat
error: UNKNOWN_QUERY (404): OrderApproval@1.0.0 has no query `stat`; declared: status
```

## orbit ps

List recent executions, newest first.

```text
orbit ps [OPTIONS]
```

| Option | Type | Default | Meaning |
|---|---|---|---|
| `--status <STATUS>` | string | all | Only executions with this status (`RUNNING`, `COMPENSATING`, `COMPLETED`, `FAILED`, `CANCELLED`, `TIMED_OUT`; case-insensitive). |
| `--workflow <WORKFLOW>` | string | all | Only executions of this workflow name. |
| `--limit <LIMIT>` | integer | `20` | Maximum number of rows. The runtime caps it at 1000. |
| runtime connection options | | | See [Runtime connection options](#runtime-connection-options). |

Exit codes: `0` on success; `1` on an API or connection error.

```console
$ orbit ps
EXECUTION                              WORKFLOW                 VERSION    STATUS        AGE
order-1002                             OrderProcessing          1.0.0      FAILED        5.4s
order-1001                             OrderProcessing          1.0.0      COMPLETED     7.4s
$ orbit ps --status running --limit 3
EXECUTION                              WORKFLOW                 VERSION    STATUS        AGE
inspect-me                             Plain                    1.0.0      RUNNING       11ms
```

## orbit token

Generate a random bearer token and the workers-file entry for it. The token (244 random bits from the operating system's generator, as 64 hexadecimal characters) is printed on stdout; the `[[worker]]` entry, which contains only the token's SHA-256 hash, is printed on stderr. The command does not contact a runtime.

```text
orbit token [OPTIONS]
```

| Option | Type | Default | Meaning |
|---|---|---|---|
| `--worker <WORKER>` | string | `my-worker` | Worker id for the generated `[[worker]]` entry. |
| `--task <TASKS>` | string, repeatable | `*` | Task pattern the worker may serve. Repeat the flag for several patterns. |

Exit codes: `0`.

```console
$ orbit token --worker payments-1 --task 'payments.*' --task audit.log
8a7df28ad62d4237a229d884ee8f8d659f3406f060554af19f10cd96c731b2c4

Give the token above to the worker (ORBIT_WORKER_TOKEN). Add this to the workers file:

[[worker]]
id = "payments-1"
token_sha256 = "cf65d58bce0a5ed7d7c7a09b53a5287eaa138e3ee3e72f22f97df7861cab7bc0"
tasks = ["payments.*", "audit.log"]
```

Because the token goes to stdout and the entry to stderr, you can capture them separately:

```bash
orbit token --worker payments-1 --task 'payments.*' > payments-1.token 2>> workers.toml
```

## orbit workers list

List registered workers.

```text
orbit workers list [OPTIONS]
```

| Option | Type | Meaning |
|---|---|---|
| runtime connection options | | See [Runtime connection options](#runtime-connection-options). |

A worker is `online` while it has at least one session that contacted the runtime within the session timeout. `INFLIGHT` counts the tasks its live sessions hold. Workers stay listed after they disconnect.

Exit codes: `0` on success; `1` on an API or connection error.

```console
$ orbit workers list
WORKER                       RUNTIME      STATE    INFLIGHT  TASKS
py-1                         python       offline  0  risk.analyze@1.0.0, payments.charge@1.0.0, payments.refund@1.0.0
ts-1                         typescript   offline  0  orders.validate@1.0.0, inventory.reserve@1.0.0, inventory.release@1.0.0, notifications.send@1.0.0
```

## See also

- [HTTP API](http-api.md)
- [Environment variables](environment.md)
- [Compiler diagnostics](diagnostics.md)
- [Running the server](../runtime/server.md)
- [Executions and history](../runtime/executions.md)
- [Formatter](../tooling/fmt.md)
- [Compatibility checks](../tooling/compat.md)
- [Testing workflows](../tooling/testing.md)
- [Simulation](../tooling/simulate.md)
- [Replay](../tooling/replay.md)
