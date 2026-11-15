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
{"status":"ok","api_version":"orbit-api/1","runtime_version":"0.1.0","ir_versions":["orbit-ir/1"],"active_executions":0,"worker_sessions":0}
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
| `--workers-file` | `ORBIT_WORKERS_FILE` | path | none | Per-worker identities (TOML). Requires `--token`. |
| `--tls-cert` | `ORBIT_TLS_CERT` | path | none | PEM certificate chain. With `--tls-key`, the HTTP API (and gRPC) serve TLS. |
| `--tls-key` | `ORBIT_TLS_KEY` | path | none | PEM private key (PKCS#8, PKCS#1 or SEC1). |
| `--tls-client-ca` | `ORBIT_TLS_CLIENT_CA` | path | none | Enables mutual TLS: client certificates signed by this CA identify workers. Requires `--tls-cert` and a workers file with `client_cert` entries. |
| `--otlp-endpoint` | `OTEL_EXPORTER_OTLP_ENDPOINT` | URL | off | Export OpenTelemetry traces over OTLP/HTTP (JSON) to this base URL. |
| `--enable-http-adapter` | | flag | off | Run `orbit.http.request` steps inside the runtime. |
| `--session-timeout` | | seconds | `30` | A worker session silent for this long is considered lost, and its in-flight attempts are redelivered. Workers are told to heartbeat every sixth of it (at least every 500 ms). |
| `--max-body` | | bytes | `4194304` (4 MiB) | Maximum request body size. Larger requests get `413`. |
| `--log-format` | | `text` or `json` | `text` | Log format. Logs go to stderr. |
| `--insecure` | | flag | off | Allow a non-loopback `--addr` without a token. Use only on a trusted network. |

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

The file format is described in [Security](security.md#per-worker-identities).

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

Because recovery runs before the listener is bound, connection attempts fail until the server is ready. A runtime with many active executions takes longer to start.

## Health and readiness

`GET /v1/health` never requires authentication and returns `200` once the server is listening:

```json
{"status":"ok","api_version":"orbit-api/1","runtime_version":"0.1.0","ir_versions":["orbit-ir/1"],"active_executions":0,"worker_sessions":0}
```

Use it for both liveness and readiness probes: the server answers only after recovery. There is no separate readiness endpoint. If you use mutual TLS, a probe that presents a client certificate matching no worker gets `403`; send probes without a client certificate.

For monitoring beyond liveness, scrape `/v1/metrics` with the admin token. See [Observability](observability.md).

## Background work

Every 100 ms the server runs a tick that:

- marks attempts whose worker session went silent as uncertain and redelivers them;
- fires due timers, retries, attempt timeouts, queue timeouts and workflow timeouts;
- runs built-in tasks (the HTTP adapter);
- starts child workflows, delivers finished children's results to their parents and cancels children whose parent no longer waits for them.

Worker sessions that stay silent for four session timeouts are forgotten; the worker must register again.

## Shutdown

Press Ctrl+C or send `SIGINT` for a graceful shutdown:

```console
INFO orbit: shutting down
```

The server stops accepting connections, waits for in-flight HTTP requests to finish, stops the gRPC listener and exits with status `0`. Long polls are not interrupted: a worker poll can hold shutdown for up to its `wait_ms` (at most 60 seconds), and a `GET /v1/executions/{id}/wait` for up to its `timeout_ms` (at most 300 seconds). With TLS, open connections get at most 10 seconds before they are closed.

`SIGTERM` is not handled: the process ends immediately, as with `SIGKILL`. This is safe, because every state change is committed to the database before it takes effect; on the next start the server recovers exactly what was committed. In a container, where the server runs as process 1 and a signal without a handler is ignored, set the stop signal to `SIGINT` (`docker run --stop-signal SIGINT`, `stop_signal: SIGINT` in Compose) so that `docker stop` shuts the server down gracefully instead of waiting for its timeout. Under systemd, set `KillSignal=SIGINT`. See [Deployment](deployment.md).

## Single node

One server owns a database. Do not run two servers against the same SQLite file or PostgreSQL database: each would keep its own in-memory view of active executions and dispatch the same work. For availability, run one server under a supervisor that restarts it (systemd, Kubernetes with one replica), and rely on recovery. See [Storage](storage.md#single-node).

## See also

- [Security](security.md)
- [Storage](storage.md)
- [Observability](observability.md)
- [Deployment](deployment.md)
- [CLI: orbit server](../reference/cli.md#orbit-server)
- [Environment variables](../reference/environment.md)
