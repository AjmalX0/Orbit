# Security

The runtime authenticates every caller, separates administrators from workers, and can give each worker its own credential that limits which tasks it may serve, and each application its own credential that limits which workflows it may start and act on. Traffic can be encrypted with TLS, and workers can authenticate with client certificates over mutual TLS. This page describes each mechanism, what each kind of caller can do, and a checklist for production.

## Threat model

The runtime owns all workflow state. Workers run your business logic on other hosts, often with access to sensitive systems, and are the most exposed part of a deployment. The design goals are:

- a compromised worker host cannot start, cancel or read executions, or register workflows;
- a worker with its own identity cannot impersonate another worker, serve tasks it was not assigned, or report results for tasks issued to someone else;
- no credential is stored in plain text on the runtime host, apart from the admin and shared worker tokens you pass in;
- nothing a worker sends can change workflow state except through validated results for tokens issued to it;
- an application with a client identity can act only on the workflows it was granted, and cannot deploy workflows or act as a worker;
- no single caller or workflow can exhaust the runtime, when [resource limits](server.md#resource-limits) are set.

## Credentials

| Credential | Configured with | Who uses it |
|---|---|---|
| Admin token | `--token` / `ORBIT_TOKEN` | Operators, the `orbit` CLI, CI pipelines that deploy workflows, and applications without a client identity. |
| Shared worker token | `--worker-token` / `ORBIT_WORKER_TOKEN` | Worker hosts, when you do not need per-worker identities. |
| Per-worker token | a `token_sha256` entry in `--workers-file` | One worker identity. |
| Client certificate | a `client_cert` entry in `--workers-file`, with `--tls-client-ca` | One worker identity, over mutual TLS. |
| Client identity token | a `token_sha256` entry of a `[[client]]` table in `--workers-file` | One application, limited to the execution API for some workflows. |
| Client identity certificate | a `client_cert` entry of a `[[client]]` table, with `--tls-client-ca` | One application, over mutual TLS. |

Tokens are sent as `Authorization: Bearer <token>` (an HTTP header, or gRPC metadata). Comparisons are constant-time.

### No authentication

Without `--token` and without `--workers-file`, every endpoint is open. The server allows this only on a loopback address (`127.0.0.1`, `::1`). On any other address it refuses to start unless you pass `--insecure`:

```console
$ orbit server --addr 0.0.0.0:7233
error: refusing to listen on 0.0.0.0:7233 without authentication; pass --token (or ORBIT_TOKEN), or --insecure for trusted networks
```

The container image listens on `0.0.0.0:7233`, so it refuses to start without `ORBIT_TOKEN`.

### Admin token

```bash
export ORBIT_TOKEN=$(orbit token 2>/dev/null)
orbit server --addr 0.0.0.0:7233 --token "$ORBIT_TOKEN"
```

With a token configured, every endpoint except `GET /v1/health` requires a credential. The admin token can call every endpoint. The CLI reads the same `ORBIT_TOKEN` variable, or `--token`.

Use a long random value. `orbit token` prints 64 hexadecimal characters (244 random bits) on stdout.

### Shared worker token

```bash
orbit server --token "$ORBIT_TOKEN" --worker-token "$ORBIT_WORKER_TOKEN"
```

The shared worker token can call only the worker protocol: `/v1/workers/*` and `/v1/tasks/*`. Anything else returns `403 FORBIDDEN`:

```json
{"error":{"code":"FORBIDDEN","message":"worker credentials may only call /v1/workers/* and /v1/tasks/*"}}
```

It requires `--token` (otherwise the administrative endpoints would be open) and must differ from it. Every holder of the shared token is equivalent: it can register under any worker id and announce any task. The SDKs read `ORBIT_WORKER_TOKEN` before `ORBIT_TOKEN`.

## Per-worker identities

A workers file gives each worker its own credential and limits. The same file can also declare [client identities](#client-identities) for applications.

```toml
# /etc/orbit/workers.toml

[[worker]]
id = "payments-1"
# SHA-256 of the worker's bearer token, as 64 hex characters.
token_sha256 = "cf65d58bce0a5ed7d7c7a09b53a5287eaa138e3ee3e72f22f97df7861cab7bc0"
tasks = ["payments.*", "audit.log"]
runtime = "python"

[[worker]]
id = "inventory"
# Authenticate with a client certificate (mutual TLS) instead of a token.
client_cert = "inventory.workers.internal"
tasks = ["inventory.reserve", "inventory.release"]
```

```bash
orbit server --token "$ORBIT_TOKEN" --workers-file /etc/orbit/workers.toml
```

### Fields

Each `[[worker]]` table accepts exactly these fields. Unknown fields are rejected.

| Field | Type | Required | Meaning |
|---|---|---|---|
| `id` | string | yes | Worker id. The worker must register with this exact `worker_id`. Must not be empty, and must be unique in the file. |
| `token_sha256` | string | one of `token_sha256` or `client_cert` | Hex SHA-256 of the worker's bearer token, 64 characters (case-insensitive). The token itself is never stored. |
| `client_cert` | string | one of `token_sha256` or `client_cert` | Certificate name for mutual TLS: matched, case-insensitively, against the certificate's DNS subject alternative names and subject common names. |
| `tasks` | array of strings | yes, at least one | Task patterns the worker may announce: an exact name (`audit.log`), a prefix ending in `*` (`payments.*` matches `payments.charge`), or `*` for every task. |
| `runtime` | string | no | When set, the worker must announce this runtime (case-insensitive), for example `python`. |

An entry may have both `token_sha256` and `client_cert`; either then authenticates it. The file is read once, at startup. To change it, edit it and restart the server.

The server refuses to start when the file is invalid:

```console
$ orbit server --token t --workers-file bad.toml
error: bad.toml: worker `a` needs `token_sha256` or `client_cert`
```

Other messages: `` worker `x` is declared twice ``, ``worker `x`: `token_sha256` must be 64 hex characters (use `orbit token`)``, ``worker `x` must list at least one task pattern``, ``a worker has an empty `id` ``, and TOML parse errors. A workers file requires `--token`.

### Generating a worker token

`orbit token` prints a new token on stdout and the matching `[[worker]]` entry on stderr:

```console
$ orbit token --worker payments-1 --task 'payments.*' --task audit.log
8a7df28ad62d4237a229d884ee8f8d659f3406f060554af19f10cd96c731b2c4

Give the token above to the worker (ORBIT_WORKER_TOKEN). Add this to the workers file:

[[worker]]
id = "payments-1"
token_sha256 = "cf65d58bce0a5ed7d7c7a09b53a5287eaa138e3ee3e72f22f97df7861cab7bc0"
tasks = ["payments.*", "audit.log"]
```

Give the token to the worker as `ORBIT_WORKER_TOKEN`, add the entry (and an optional `runtime`) to the workers file, and restart the server. To compute the hash of an existing token yourself:

```bash
printf %s "$TOKEN" | sha256sum | cut -d' ' -f1
```

### What a worker identity may do

A per-worker identity (by token or by certificate) can call only `/v1/workers/*` and `/v1/tasks/*`, and is further restricted:

| Action | Rule | Error |
|---|---|---|
| Register | `worker_id` must equal its `id`. | ``this credential belongs to worker `payments-1`, not `other` `` |
| Register | Every announced task must match one of its `tasks` patterns. | ``worker `payments-1` may not serve: inventory.reserve (allowed: payments.*, audit.log)`` |
| Register | If `runtime` is set, the announced runtime must match. | ``worker `payments-1` must announce runtime `python` `` |
| Heartbeat, poll, unregister | The session must be one this worker opened. | ``session belongs to worker `payments-1`, not `inventory` `` |
| Complete, fail | The token must have been issued to this worker. | ``task token was issued to worker `payments-1`, not `inventory` `` |

All of these return `403 FORBIDDEN`.

## Client identities

The admin token can do everything, including deploying workflows and reading every execution. An application that only needs to start and follow some workflows should have a client identity instead: a `[[client]]` entry in the same workers file, with its own credential, the workflows it may act on and the actions it may take.

```toml
# /etc/orbit/workers.toml

[[client]]
id = "warehouse-app"
token_sha256 = "4854f0bb0c1dd54b3884060e1a136550851354eccf27e8d1a5a6307b7f522df1"
workflows = ["Print*"]
# actions defaults to ["start", "signal", "query", "read"]

[[client]]
id = "ops-console"
client_cert = "ops-console.clients.internal"
workflows = ["*"]
actions = ["read", "cancel"]
```

```bash
orbit server --token "$ORBIT_TOKEN" --workers-file /etc/orbit/workers.toml
```

### Client fields

Each `[[client]]` table accepts exactly these fields. Unknown fields are rejected.

| Field | Type | Required | Meaning |
|---|---|---|---|
| `id` | string | yes | Client name, used in error messages. Must not be empty, and must be unique in the file across workers and clients. |
| `token_sha256` | string | one of `token_sha256` or `client_cert` | Hex SHA-256 of the client's bearer token, 64 characters (case-insensitive). |
| `client_cert` | string | one of `token_sha256` or `client_cert` | Certificate name for mutual TLS, matched like a worker's `client_cert`. |
| `workflows` | array of strings | yes, at least one | Workflow name patterns: an exact name (`Refund`), a prefix ending in `*` (`Invoice*` matches `InvoiceMonthly`), or `*` for every workflow. |
| `actions` | array of strings | no | Granted actions, from `start`, `signal`, `query`, `read` and `cancel`. Default: `start`, `signal`, `query` and `read`. `cancel` must be granted explicitly. |

The server refuses to start when an entry is invalid:

```console
$ orbit server --token t --workers-file bad.toml
error: bad.toml: client `app`: unknown action `deploy` (allowed: start, signal, query, read, cancel)
```

Other messages: ``client `app` needs `token_sha256` or `client_cert` ``, ``client `app`: `token_sha256` must be 64 hex characters (use `orbit token`)``, ``client `app` must list at least one workflow pattern``, ``a client has an empty `id` ``, ``identity `x` is declared twice`` (a client reuses the id of a worker or of another client), and TOML parse errors such as ``missing field `workflows` ``. A workers file requires `--token`, even when it declares only clients.

### Generating a client token

`orbit token --client <id>` prints a new token on stdout and the matching `[[client]]` entry on stderr. `--workflow` (repeatable, default `*`) sets the workflow patterns and `--action` (repeatable, default `start`, `signal`, `query`, `read`) the granted actions:

```console
$ orbit token --client billing --workflow 'Order*' --action start --action read
c47e25df244d4b9c80df65eb6e3d457f43f565e91ae544599834c47de7301932

Give the token above to the application (ORBIT_TOKEN). Add this to the workers file:

[[client]]
id = "billing"
token_sha256 = "a632898a1f1166e52c3dd7c1c68ba2f0b495e566dc65e195b2b2fc37dbf94f4a"
workflows = ["Order*"]
actions = ["start", "read"]
```

An unknown action is rejected: ``error: unknown action `deploy` (allowed: start, signal, query, read, cancel)``. To hash a token you already have:

```bash
printf %s "$TOKEN" | sha256sum | cut -d' ' -f1
```

### What a client identity may do

A client identity can use only the execution API, for workflows that match one of its `workflows` patterns:

| Request | Needs |
|---|---|
| Start an execution (`POST /v1/executions`, `orbit run`) | `start` |
| Send a signal (`orbit signal`) | `signal` |
| Evaluate a query (`orbit query`) | `query` |
| Read an execution, its history, or wait for it (`orbit status`, `logs`, `inspect`, `run` without `--no-wait`); read a workflow definition | `read` |
| Cancel an execution (`orbit cancel`) | `cancel` |
| List executions or workflows (`orbit ps`) | nothing; the list shows only workflows it may `read`. A page of executions still holds up to `limit` entries it may read, taken from the 1000 most recent matching the other filters. |

For a request about an existing execution, the workflow is the execution's workflow. Everything else is refused with `403 FORBIDDEN`: deploying workflows, `GET /v1/workers`, `/v1/metrics`, the worker protocol over HTTP (`/v1/workers/*`, `/v1/tasks/*`) and the gRPC binding.

```console
$ export ORBIT_TOKEN=$WAREHOUSE_TOKEN
$ orbit run PrintLabels --id labels-9 --no-wait --input '{"skus":["A"]}'
started execution labels-9
labels-9
$ orbit run Review --no-wait --input '{"order_id":"1"}'
error: FORBIDDEN (403): client `warehouse-app` may not start executions of `Review`
$ orbit deploy labels.orbit
error: FORBIDDEN (403): client `warehouse-app` may only use the execution API (start, signal, query, read, cancel)
$ curl -s -X POST -H "authorization: Bearer $WAREHOUSE_TOKEN" http://127.0.0.1:7233/v1/executions/labels-1/cancel
{"error":{"code":"FORBIDDEN","message":"client `warehouse-app` may not cancel executions of `PrintLabels`"}}
```

The SDKs' `Client` classes authenticate with a client token the same way as with the admin token: pass it as the client's token. Workers cannot use a client token; give them a worker identity.

## TLS

```bash
orbit server --token "$ORBIT_TOKEN" \
  --tls-cert /etc/orbit/tls/server.pem --tls-key /etc/orbit/tls/server.key
```

- The server serves HTTPS directly, with rustls: TLS 1.2 and 1.3, HTTP/2 and HTTP/1.1 via ALPN. The same certificate is used for the gRPC listener when `--grpc-addr` is set.
- `--tls-cert` is a PEM file with the certificate chain, leaf first. `--tls-key` is the PEM private key (PKCS#8, PKCS#1 or SEC1).
- Clients trust a private CA with `--ca-cert` or `ORBIT_CA_CERT` (CLI, Python, Go, Rust, Java, C#) or `NODE_EXTRA_CA_CERTS` (Node.js). The file must contain a CA certificate (`basicConstraints=CA:TRUE`, `keyCertSign`), not the server certificate itself.
- The server certificate must name the host clients connect to in a subject alternative name (DNS name or IP address).

You can instead terminate TLS at a reverse proxy or load balancer in front of the runtime. Then you cannot use mutual TLS for worker identities, because the runtime does not see client certificates.

### Creating a private CA for testing

```bash
openssl req -x509 -newkey rsa:2048 -nodes -keyout ca.key -out ca.pem -days 365 \
  -subj "/CN=Orbit CA" \
  -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign"

openssl req -newkey rsa:2048 -nodes -keyout server.key -out server.csr -subj "/CN=orbit.internal"
printf "subjectAltName=DNS:orbit.internal,IP:127.0.0.1\nextendedKeyUsage=serverAuth\n" > server.ext
openssl x509 -req -in server.csr -CA ca.pem -CAkey ca.key -CAcreateserial \
  -out server.pem -days 365 -extfile server.ext
```

## Mutual TLS

```bash
orbit server --token "$ORBIT_TOKEN" \
  --tls-cert server.pem --tls-key server.key \
  --tls-client-ca workers-ca.pem \
  --workers-file workers.toml
```

- Client certificates signed by the CA in `--tls-client-ca` are verified during the TLS handshake.
- A verified certificate identifies the worker whose `client_cert` matches one of its DNS subject alternative names or subject common names.
- A verified certificate that matches no `[[worker]]` entry identifies the `[[client]]` entry whose `client_cert` matches it, if any (worker entries are checked first).
- A verified certificate that matches neither is refused with `403` on every endpoint, including `/v1/health`: ``the client certificate is valid but matches no worker or client in the workers file``.
- Client certificates are optional at the TLS layer. A client without a certificate can still connect and authenticate with a token, which is how the admin CLI and health probes connect.
- When a request carries both a bearer token and a certificate, the token is checked first.
- `--tls-client-ca` requires a workers file with at least one entry, and `--tls-cert`.

Issue a worker certificate whose subject alternative name matches its `client_cert`:

```bash
openssl req -newkey rsa:2048 -nodes -keyout inventory.key -out inventory.csr \
  -subj "/CN=inventory.workers.internal"
printf "subjectAltName=DNS:inventory.workers.internal\nextendedKeyUsage=clientAuth\n" > client.ext
openssl x509 -req -in inventory.csr -CA workers-ca.pem -CAkey workers-ca.key -CAcreateserial \
  -out inventory.pem -days 365 -extfile client.ext
```

Every SDK supports client certificates through `ORBIT_CLIENT_CERT` and `ORBIT_CLIENT_KEY`; the CLI through `--client-cert` and `--client-key`.

## Permission matrix

| Endpoint | No auth configured | Admin token | Shared worker token | Per-worker identity | Client identity | No or invalid credential (auth configured) |
|---|---|---|---|---|---|---|
| `GET /v1/health` | yes | yes | yes | yes | yes | yes |
| `GET /v1/metrics` | yes | yes | 403 | 403 | 403 | 401 |
| `POST /v1/workflows` | yes | yes | 403 | 403 | 403 | 401 |
| `GET /v1/workflows` | yes | yes | 403 | 403 | filtered to readable workflows | 401 |
| `GET /v1/workflows/{name}...` | yes | yes | 403 | 403 | with `read` | 401 |
| `POST /v1/executions` | yes | yes | 403 | 403 | with `start` | 401 |
| `GET /v1/executions` | yes | yes | 403 | 403 | filtered to readable workflows | 401 |
| `GET /v1/executions/{id}`, `history`, `wait` | yes | yes | 403 | 403 | with `read` | 401 |
| `POST /v1/executions/{id}/signals/{name}` | yes | yes | 403 | 403 | with `signal` | 401 |
| `GET /v1/executions/{id}/queries/{name}` | yes | yes | 403 | 403 | with `query` | 401 |
| `POST /v1/executions/{id}/cancel` | yes | yes | 403 | 403 | with `cancel` | 401 |
| `GET /v1/workers` | yes | yes | 403 | 403 | 403 | 401 |
| `POST /v1/workers/register` | yes | yes | yes, any id and tasks | own id, allowed tasks, required runtime | 403 | 401 |
| `POST /v1/workers/heartbeat`, `poll`, `unregister` | yes | yes | yes | own sessions only | 403 | 401 |
| `POST /v1/tasks/complete`, `fail` | yes | yes | yes | own tokens only | 403 | 401 |
| gRPC worker protocol | yes | yes | yes | as over HTTP | `PERMISSION_DENIED` | `UNAUTHENTICATED` |
| `GET /ui/*` (web UI assets) and `GET /` | yes | yes | yes | yes | yes | yes (no credential needed; off with `--no-ui`) |

"with `start`" and the like mean: when the workflow matches one of the client's patterns and the action is granted; otherwise `403`. A client certificate that is verified but matches no worker or client gets `403` everywhere.

## Web UI

The server serves a [web UI](web-ui.md) at `/ui/` unless it is started with `--no-ui` (`ORBIT_NO_UI=true`).

- The UI's files (one HTML page, one script, one stylesheet, one icon) are compiled into the binary and served without authentication, so the sign-in page can load. They contain no data and no secrets. Every piece of data the UI shows comes from the HTTP API, called with the token the user enters, so the permission matrix above applies unchanged: a client identity sees only the workflows and actions it was granted, and refused requests are shown as `Not permitted` with the API's message. Worker credentials cannot sign in, because they cannot read executions.
- The token is kept in the browser tab's `sessionStorage` (cleared when the tab closes, or by **Sign out**) and sent as `Authorization: Bearer`. It is never put in a URL or a cookie. The UI does not use TLS client certificates: requests are made with `credentials: "omit"`.
- Every UI response carries `Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; connect-src 'self'; font-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'`, `X-Content-Type-Options: nosniff`, `X-Frame-Options: DENY`, `Referrer-Policy: no-referrer`, `Cross-Origin-Opener-Policy: same-origin`, `Cross-Origin-Resource-Policy: same-origin`, a `Permissions-Policy` that disables camera, microphone, geolocation and payment, and `Cache-Control: no-cache`. The page has no inline script or style, loads nothing from other origins, and cannot be framed.
- Values from executions (inputs, outputs, errors, signal payloads, names) are inserted into the page as text, never as markup.
- Like the API, the UI shows values unredacted to whoever holds a token that may read them.
- The UI uses the API's normal endpoints, so running it adds no endpoint and no privilege. Turn it off with `--no-ui` if browsers should not reach the server at all.

## Input validation and limits

- Request bodies are limited to `--max-body` bytes (4 MiB by default); larger requests get `413`.
- With `--max-payload-bytes`, an execution input, a task result (over HTTP or gRPC) or a signal payload larger than the limit, as compact JSON, is refused with `413 PAYLOAD_TOO_LARGE` and not recorded.
- With `--max-active-executions`, a start beyond the limit gets `429 RESOURCE_EXHAUSTED`. Restarting an existing `execution_id` and starting child workflows are never refused.
- With `--max-fan-out`, a `for` step over a longer list fails with `LIMIT_EXCEEDED`; with `--max-history-events`, an execution whose history reaches the limit fails with `LIMIT_EXCEEDED`. Compensation and `on failure` handlers still run.
- Execution input is validated strictly against the workflow's input type. Step inputs are validated against task input contracts before dispatch, and worker output against task output contracts before it is accepted.
- Execution ids are limited to 200 characters of letters, digits, `-`, `_`, `.` and `:`.
- Workers cannot announce built-in tasks (`orbit.*`) or child-workflow contracts (`RESERVED_TASK`).
- Results are accepted only for tokens the runtime issued; stale and duplicate results are recorded and ignored.

The limits are off by default. Set them on any server that untrusted or many callers can reach. See [Running the server: resource limits](server.md#resource-limits).

## HTTP adapter

The built-in `orbit.http.request` task is off by default. When enabled with `--enable-http-adapter`, anyone who can deploy a workflow can make the runtime host send HTTP requests, including to internal addresses. Only `http://` and `https://` URLs are allowed. Enable it only when you trust everyone holding the admin token, and restrict the runtime host's outbound network access. ORBIT has no shell or code execution feature.

## Redaction

- Runtime logs never include inputs, outputs or error details.
- `orbit inspect <id>`, `orbit logs --json` and the output line of `orbit status` replace the value of every key whose name contains, case-insensitively, `password`, `passwd`, `secret`, `token`, `api_key`, `apikey`, `authorization`, `credential`, `private_key`, `card_number` or `cvv` with `"***"`. The protocol's own `token` field is not redacted, because it is an opaque dispatch id.
- The HTTP API returns values unredacted to admin callers. Treat history as sensitive data, and keep secrets out of workflow inputs and step outputs where you can: pass references (a vault path, a customer id) instead.
- The PostgreSQL password in `--db` is redacted in logs.

## Production checklist

1. Set `--token` (or `ORBIT_TOKEN`) to a value from `orbit token`, stored in a secret manager.
2. Give workers per-worker identities (`--workers-file`) with the narrowest `tasks` patterns, or at least a shared `--worker-token`. Never give workers the admin token.
3. Give applications that start or follow executions client identities (`[[client]]`) with the narrowest `workflows` patterns and actions, and keep the admin token for operators and deployment pipelines.
4. Set resource limits (`--max-active-executions`, `--max-fan-out`, `--max-history-events`, `--max-payload-bytes`) that fit your largest legitimate workload.
5. Serve TLS with `--tls-cert` and `--tls-key`. Use mutual TLS (`--tls-client-ca`) for worker identities where your PKI allows it.
6. Use `sslmode=require` for PostgreSQL, with `ORBIT_DB_CA_CERT` for a private CA.
7. Keep the HTTP adapter disabled unless you need it.
8. Do not pass `--insecure`.
9. Restrict network access to the API port to the hosts that need it. `/v1/metrics` requires the admin token, so give your monitoring system a way to send it, or scrape through a proxy that adds it.
10. Decide whether operators should use the [web UI](web-ui.md) from a browser; if not, pass `--no-ui`.
11. Protect the database and its backups as sensitive data: they contain every input and output.
12. Rotate a token by changing the server's configuration (or the workers file), restarting the server, and giving the new token to its holders. Requests with the old token get `401`, so restart each worker with its new token.

To report a vulnerability, follow the [security policy](https://github.com/AjmalX0/Orbit/blob/main/SECURITY.md): use GitHub's private vulnerability reporting, not a public issue.

## See also

- [Running the server](server.md)
- [Deployment](deployment.md)
- [HTTP API: authentication](../reference/http-api.md#authentication)
- [Running the server: resource limits](server.md#resource-limits)
- [Environment variables](../reference/environment.md)
- [How workers work](../sdks/overview.md)
