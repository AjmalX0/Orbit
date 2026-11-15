# Environment variables

This page lists every environment variable that ORBIT components read: the `orbit` CLI, the runtime server, the worker SDKs, the install script and the VS Code extension. A command-line flag always takes precedence over its environment variable, and an SDK option passed in code takes precedence over the environment. An empty value is treated as unset for credentials.

## Runtime server

Read by `orbit server`.

| Variable | Flag | Default | Meaning |
|---|---|---|---|
| `ORBIT_DB` | `--db` | `orbit.db` | SQLite file path or PostgreSQL URL (`postgres://` or `postgresql://`). |
| `ORBIT_ADDR` | `--addr` | `127.0.0.1:7233` | HTTP API listen address. The container image sets it to `0.0.0.0:7233`. |
| `ORBIT_GRPC_ADDR` | `--grpc-addr` | unset | Also serve the gRPC worker protocol on this address. |
| `ORBIT_TOKEN` | `--token` | unset | Admin bearer token. Required on every endpoint except `/v1/health` when set. |
| `ORBIT_WORKER_TOKEN` | `--worker-token` | unset | Shared worker credential, limited to `/v1/workers/*` and `/v1/tasks/*`. |
| `ORBIT_WORKERS_FILE` | `--workers-file` | unset | Path of the workers file (TOML): per-worker identities and client identities. |
| `ORBIT_TLS_CERT` | `--tls-cert` | unset | PEM certificate chain for HTTPS (and TLS on gRPC). |
| `ORBIT_TLS_KEY` | `--tls-key` | unset | PEM private key for `ORBIT_TLS_CERT`. |
| `ORBIT_TLS_CLIENT_CA` | `--tls-client-ca` | unset | PEM CA that signs worker client certificates (mutual TLS). |
| `OTEL_EXPORTER_OTLP_ENDPOINT` | `--otlp-endpoint` | unset | OTLP/HTTP collector base URL. Spans are posted to `<endpoint>/v1/traces`. |
| `OTEL_EXPORTER_OTLP_HEADERS` | | unset | Extra headers for trace export, as comma-separated `key=value` pairs, for example `x-honeycomb-team=abc123`. Read only when trace export is enabled. |
| `OTEL_SERVICE_NAME` | | `orbit-runtime` | Value of the `service.name` resource attribute on exported spans. |
| `ORBIT_LOG` | | `info` | Log filter in [`tracing-subscriber` `EnvFilter`](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/filter/struct.EnvFilter.html) syntax, for example `debug` or `info,orbit_runtime=debug`. Setting it replaces the default `info` filter entirely. |
| `ORBIT_DB_CA_CERT` | | unset | PEM CA bundle trusted for PostgreSQL TLS, in addition to the public web PKI roots. |
| `ORBIT_MAX_ACTIVE_EXECUTIONS` | `--max-active-executions` | unlimited | Maximum running executions; further starts get `429 RESOURCE_EXHAUSTED`. |
| `ORBIT_MAX_FAN_OUT` | `--max-fan-out` | unlimited | Maximum elements of one `for` step; larger lists fail the step with `LIMIT_EXCEEDED`. |
| `ORBIT_MAX_HISTORY_EVENTS` | `--max-history-events` | unlimited | Maximum history events per execution; past it the execution fails with `LIMIT_EXCEEDED`. |
| `ORBIT_MAX_PAYLOAD_BYTES` | `--max-payload-bytes` | unlimited | Maximum size in bytes of an execution input, task result or signal payload; larger ones get `413 PAYLOAD_TOO_LARGE`. |
| `ORBIT_CLUSTER` | `--cluster` | unset | `true` to run as one of several servers on one PostgreSQL database. |
| `ORBIT_NODE_ID` | `--node-id` | host name | This server's name in the cluster. |
| `ORBIT_ADVERTISE_ADDR` | `--advertise-addr` | derived from `--addr` | URL handed out for this server while it leads. |

The flags `--insecure`, `--enable-http-adapter`, `--session-timeout`, `--max-body` and `--log-format` have no environment variable.

## CLI client

Read by `orbit deploy`, `run`, `status`, `inspect`, `logs`, `cancel`, `ps` and `workers list`.

| Variable | Flag | Default | Meaning |
|---|---|---|---|
| `ORBIT_SERVER` | `--server` | `http://127.0.0.1:7233` | Runtime API address. |
| `ORBIT_TOKEN` | `--token` | unset | Bearer token: the admin token, or a client identity's token for the commands its actions allow. |
| `ORBIT_CA_CERT` | `--ca-cert` | unset | Extra PEM CA certificate to trust. |
| `ORBIT_CLIENT_CERT` | `--client-cert` | unset | PEM client certificate for mutual TLS. |
| `ORBIT_CLIENT_KEY` | `--client-key` | unset | PEM private key for the client certificate. |
| `NO_COLOR` | | unset | When set to any value, disables colored output in every command. |

`ORBIT_TOKEN` is shared by the server and the client. On a host that runs both, the CLI picks up the server's admin token, which is usually what you want.

## Worker SDKs

Every SDK reads the same variables when the corresponding option is not set in code.

| Variable | Default | Meaning | Read by |
|---|---|---|---|
| `ORBIT_SERVER` | `http://127.0.0.1:7233` | Runtime API address. | Python, TypeScript, Go, Rust, Java, C# |
| `ORBIT_WORKER_TOKEN` | unset | Worker credential: the shared worker token or a per-worker token. Preferred over `ORBIT_TOKEN`. | Python, TypeScript, Go, Rust, Java, C# |
| `ORBIT_TOKEN` | unset | Used as the worker credential when `ORBIT_WORKER_TOKEN` is not set. | Python, TypeScript, Go, Rust, Java, C# |
| `ORBIT_CA_CERT` | unset | Extra PEM CA certificate to trust for HTTPS. | Python, Go, Rust, Java, C#; TypeScript only together with a client certificate (see below) |
| `ORBIT_CLIENT_CERT` | unset | PEM client certificate for mutual TLS. | Python, TypeScript, Go, Rust, Java, C# |
| `ORBIT_CLIENT_KEY` | unset | PEM private key for the client certificate. | Python, TypeScript, Go, Rust, Java, C# |
| `NODE_EXTRA_CA_CERTS` | unset | Read by Node.js itself: extra CA certificates for every HTTPS connection. Use it to trust a private CA from a TypeScript or JavaScript worker that does not use a client certificate. | Node.js |
| `ORBIT_LOG_LEVEL` | `INFO` | Log level for the worker's `logging.basicConfig` call. | Python |
| `HOSTNAME` | system host name | Used in the default worker id `java-<host>-<pid>`. | Java |

Language-specific options and defaults are on each SDK's page, for example [Python](../sdks/python.md) and [Go](../sdks/go.md).

## Install script

Read by `install.sh` (`curl -fsSL https://github.com/AjmalX0/Orbit/releases/latest/download/install.sh | sh`).

| Variable | Default | Meaning |
|---|---|---|
| `ORBIT_VERSION` | `latest` | Release tag to install, for example `v0.1.0`. |
| `ORBIT_INSTALL_DIR` | `~/.local/bin`, or `/usr/local/bin` as root | Directory the binary is installed into. |
| `ORBIT_DOWNLOAD_URL` | GitHub release URL | Base URL of a mirror serving the release assets (`orbit-<target>.tar.gz` and `SHA256SUMS`). Overrides `ORBIT_VERSION`. |

## VS Code extension

The extension does not read environment variables from your shell for configuration. It sets `ORBIT_SERVER` to the value of the `orbit.runtimeUrl` setting in the integrated terminal it opens for the Run, Deploy and Check commands. See [Editor support](../tooling/editors.md).

## Development and tests

These variables are read only by the project's own test suites:

| Variable | Read by | Meaning |
|---|---|---|
| `ORBIT_TEST_POSTGRES_URL` | `crates/orbit-runtime/tests/postgres.rs` | PostgreSQL URL for the PostgreSQL backend tests. The tests are skipped when it is unset. |
| `ORBIT_BIN` | `editors/vscode/test` | Path of the `orbit` binary used by the extension tests. |

## See also

- [CLI](cli.md)
- [Running the server](../runtime/server.md)
- [Security](../runtime/security.md)
- [Storage](../runtime/storage.md)
- [Observability](../runtime/observability.md)
- [How workers work](../sdks/overview.md)
