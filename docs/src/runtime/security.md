# Security

The runtime authenticates every caller, separates administrators from workers, and can give each worker its own credential that limits which tasks it may serve. Traffic can be encrypted with TLS, and workers can authenticate with client certificates over mutual TLS. This page describes each mechanism, what each kind of caller can do, and a checklist for production.

## Threat model

The runtime owns all workflow state. Workers run your business logic on other hosts, often with access to sensitive systems, and are the most exposed part of a deployment. The design goals are:

- a compromised worker host cannot start, cancel or read executions, or register workflows;
- a worker with its own identity cannot impersonate another worker, serve tasks it was not assigned, or report results for tasks issued to someone else;
- no credential is stored in plain text on the runtime host, apart from the admin and shared worker tokens you pass in;
- nothing a worker sends can change workflow state except through validated results for tokens issued to it.

## Credentials

| Credential | Configured with | Who uses it |
|---|---|---|
| Admin token | `--token` / `ORBIT_TOKEN` | Operators, the `orbit` CLI, CI pipelines and applications that deploy workflows and start executions. |
| Shared worker token | `--worker-token` / `ORBIT_WORKER_TOKEN` | Worker hosts, when you do not need per-worker identities. |
| Per-worker token | a `token_sha256` entry in `--workers-file` | One worker identity. |
| Client certificate | a `client_cert` entry in `--workers-file`, with `--tls-client-ca` | One worker identity, over mutual TLS. |

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

A workers file gives each worker its own credential and limits:

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
- A verified certificate that matches no worker is refused with `403` on every endpoint, including `/v1/health`: ``the client certificate is valid but matches no worker in the workers file``.
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

| Endpoint | No auth configured | Admin token | Shared worker token | Per-worker identity | No or invalid credential (auth configured) |
|---|---|---|---|---|---|
| `GET /v1/health` | yes | yes | yes | yes | yes |
| `GET /v1/metrics` | yes | yes | 403 | 403 | 401 |
| `POST`/`GET /v1/workflows...` | yes | yes | 403 | 403 | 401 |
| `POST`/`GET /v1/executions...`, cancel, history, wait | yes | yes | 403 | 403 | 401 |
| `GET /v1/workers` | yes | yes | 403 | 403 | 401 |
| `POST /v1/workers/register` | yes | yes | yes, any id and tasks | own id, allowed tasks, required runtime | 401 |
| `POST /v1/workers/heartbeat`, `poll`, `unregister` | yes | yes | yes | own sessions only | 401 |
| `POST /v1/tasks/complete`, `fail` | yes | yes | yes | own tokens only | 401 |

A client certificate that is verified but matches no worker gets `403` everywhere.

## Input validation and limits

- Request bodies are limited to `--max-body` bytes (4 MiB by default); larger requests get `413`.
- Execution input is validated strictly against the workflow's input type. Step inputs are validated against task input contracts before dispatch, and worker output against task output contracts before it is accepted.
- Execution ids are limited to 200 characters of letters, digits, `-`, `_`, `.` and `:`.
- Workers cannot announce built-in tasks (`orbit.*`) or child-workflow contracts (`RESERVED_TASK`).
- Results are accepted only for tokens the runtime issued; stale and duplicate results are recorded and ignored.

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
3. Serve TLS with `--tls-cert` and `--tls-key`. Use mutual TLS (`--tls-client-ca`) for worker identities where your PKI allows it.
4. Use `sslmode=require` for PostgreSQL, with `ORBIT_DB_CA_CERT` for a private CA.
5. Keep the HTTP adapter disabled unless you need it.
6. Do not pass `--insecure`.
7. Restrict network access to the API port to the hosts that need it. `/v1/metrics` requires the admin token, so give your monitoring system a way to send it, or scrape through a proxy that adds it.
8. Protect the database and its backups as sensitive data: they contain every input and output.
9. Rotate a token by changing the server's configuration (or the workers file), restarting the server, and giving the new token to its holders. Requests with the old token get `401`, so restart each worker with its new token.

To report a vulnerability, follow the [security policy](https://github.com/AjmalX0/Orbit/blob/main/SECURITY.md): use GitHub's private vulnerability reporting, not a public issue.

## See also

- [Running the server](server.md)
- [Deployment](deployment.md)
- [HTTP API: authentication](../reference/http-api.md#authentication)
- [Environment variables](../reference/environment.md)
- [How workers work](../sdks/overview.md)
