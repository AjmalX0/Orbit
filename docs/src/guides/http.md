# Calling HTTP services

This guide shows how to call an HTTP endpoint from a workflow without writing
a worker, using the built-in task `orbit.http.request`. You enable the
adapter on the server, run the health-check example, post a JSON webhook with
headers and retries, and learn how the adapter maps HTTP statuses to step
results.

## When to use the adapter

The HTTP adapter runs inside the runtime process. It suits calls that need no
custom code around them: webhooks, health checks, and simple JSON APIs. Write a
worker instead when the call needs request signing, pagination, response
transformation, client certificates, secrets that should not pass through
workflow input, or any business logic.

## Enable the adapter

The adapter is off by default, because it lets workflow authors make outbound
requests from the runtime host. Start the server with
`--enable-http-adapter`:

```bash
orbit server --enable-http-adapter
```

```text
INFO orbit: starting ORBIT runtime db=orbit.db backend="sqlite" http_adapter=true auth=false
```

If the adapter is disabled, a step that uses it fails immediately with the
non-retryable code `ADAPTER_DISABLED`:

```text
  ✗ probe                          failed the HTTP adapter is disabled; start the server with --enable-http-adapter

FAILED ping-off  HttpPing@1.0.0
  elapsed: 71ms
  compensation: none
  error: ADAPTER_DISABLED step `probe` failed: the HTTP adapter is disabled; start the server with --enable-http-adapter
```

With Docker, append the flag to the server command:
`docker run ... ghcr.io/ajmalx0/orbit:v0.1.0 server --enable-http-adapter`.

## The built-in contract

`orbit.http.request` is declared by the compiler's prelude, so you use it
without a `task` declaration:

```orbit
task "orbit.http.request" {
    version "1.0.0"
    input {
        method: String = "GET"
        url: String
        headers: Map<String, String> = {}
        body: JSON = null
    }
    output {
        status: Int
        headers: Map<String, String>
        body: JSON
    }
}
```

| Input field | Behavior |
|---|---|
| `method` | `GET` (default), `POST`, `PUT`, `PATCH`, `DELETE`, `HEAD` or `OPTIONS`. Case does not matter. Any other method fails with a non-retryable `HTTP_ERROR`. |
| `url` | Must start with `http://` or `https://`. Other schemes fail with a non-retryable `HTTP_ERROR`. |
| `headers` | Sent as request headers. |
| `body` | For `POST`, `PUT` and `PATCH` only. `null` sends an empty body, a string is sent as is, and any other value is sent as JSON. |

| Output field | Contents |
|---|---|
| `status` | The HTTP status code. |
| `headers` | Response headers, with lowercase names. |
| `body` | The response body parsed as JSON, or the raw text as a string if it is not JSON. At most 8 MiB is read. |

The step's `timeout` applies to the whole request. Without one, a request may
take up to 30 seconds. HTTPS certificates are verified against the public web
PKI.

## How statuses map to results

| Response | Step result |
|---|---|
| `2xx`, `3xx`, `4xx` other than `429` | The attempt succeeds. The workflow receives the status and decides what it means. |
| `429` and `5xx` | The attempt fails with a retryable `HTTP_ERROR` (`server responded <status>`), so the step's `retry` policy applies. The status and body are kept in the error details. |
| Connection error or timeout | The attempt fails with a retryable `HTTP_ERROR` (`request failed: ...`). |

A `404` is therefore a successful step with `status: 404`. Test the status in
`return`, in a `when` condition of a later step, or both.

## Run the health-check example

`examples/http_ping.orbit` calls a URL and reports whether it answered `200`:

```orbit
// Calls an HTTP endpoint with the built-in adapter: no worker required.
// Start the runtime with `orbit server --enable-http-adapter`.

workflow HttpPing {
    version "1.0.0"
    input { url: String = "http://127.0.0.1:7233/v1/health" }

    step probe {
        worker "orbit.http.request"
        with { url: input.url, headers: { accept: "application/json" } }
        timeout 5s
        retry { attempts: 3, backoff: exponential, initial_delay: 200ms }
    }

    return {
        status: probe.status
        healthy: probe.status == 200
    }
}
```

The default URL is the runtime's own health endpoint, so the example works
with no other service. From a clone of the repository, with the server
running:

```bash
orbit run examples/http_ping.orbit --id ping-1
```

```text
deployed HttpPing@1.0.0 (sha256:0c0d025d4d99)
started execution ping-1
  ✓ probe                          succeeded 4ms

COMPLETED ping-1  HttpPing@1.0.0
  elapsed: 9ms
{
  "status": 200,
  "healthy": true
}
```

No worker was involved. `orbit inspect ping-1` shows the request the runtime
sent, with the input defaults applied, and the response it received
(abridged):

```json
"probe": {
  "status": "SUCCEEDED",
  "input": {
    "url": "http://127.0.0.1:7233/v1/health",
    "headers": { "accept": "application/json" },
    "method": "GET",
    "body": null
  },
  "output": {
    "status": 200,
    "headers": {
      "content-type": "application/json",
      "x-orbit-api-version": "orbit-api/1",
      "content-length": "140",
      "date": "Sun, 15 Nov 2026 09:37:40 GMT"
    },
    "body": {
      "status": "ok",
      "api_version": "orbit-api/1",
      "runtime_version": "0.1.0",
      "ir_versions": ["orbit-ir/1"],
      "active_executions": 1,
      "worker_sessions": 0
    }
  },
  "worker_id": "orbit-runtime"
}
```

A path that does not exist returns `404`, which completes the workflow with
`"healthy": false`:

```bash
orbit run examples/http_ping.orbit --id ping-404 --input '{"url": "http://127.0.0.1:7233/nope"}'
```

```text
COMPLETED ping-404  HttpPing@1.0.0
  elapsed: 56ms
{
  "status": 404,
  "healthy": false
}
```

A URL where nothing listens fails each attempt with a connection error. The
step retries twice, with 200 ms and 400 ms of backoff, then fails:

```bash
orbit run examples/http_ping.orbit --id ping-down --input '{"url": "http://127.0.0.1:7999/"}'
```

```text
  ✗ probe                          failed request failed: io: Connection refused (os error 111) (gave up after 3 attempts)

FAILED ping-down  HttpPing@1.0.0
  elapsed: 888ms
  compensation: none
  error: HTTP_ERROR step `probe` failed: request failed: io: Connection refused (os error 111) (gave up after 3 attempts)
```

## Post a webhook

The following workflow posts a JSON event with an authorization header and an
idempotency header, retries server errors, and reports whether the receiver
answered `201 Created`:

```orbit
workflow NotifyWebhook {
    version "1.1.0"
    input {
        order_id: String
        webhook_url: String
        api_key: String
    }
    output { accepted: Bool, status: Int }

    step post_event {
        worker "orbit.http.request"
        with {
            method: "POST"
            url: input.webhook_url
            headers: {
                authorization: "Bearer " + input.api_key
                "idempotency-key": "order-created-" + input.order_id
            }
            body: { event: "order.created", order_id: input.order_id }
        }
        timeout 10s
        retry { attempts: 3, backoff: exponential, initial_delay: 1s }
    }

    return {
        accepted: post_event.status == 201
        status: post_event.status
    }
}
```

- Header names that are not identifiers, such as `idempotency-key`, are
  written as quoted keys.
- The adapter does not add an idempotency header by itself. Retries and
  redeliveries can send the same request more than once, so pass a stable key
  that the receiver can deduplicate on. Build it from workflow data, as here,
  or from `execution.id`.
- `body` is a record, so it is sent as JSON.

Against a receiver that answers `201`:

```bash
orbit run notify.orbit --id wh-3 --input '{"order_id": "A-1", "webhook_url": "https://hooks.example.com/orders", "api_key": "k-123"}'
```

```text
deployed NotifyWebhook@1.1.0 (sha256:961caf628d35)
started execution wh-3
  ✓ post_event                     succeeded 5ms

COMPLETED wh-3  NotifyWebhook@1.1.0
  elapsed: 51ms
{
  "accepted": true,
  "status": 201
}
```

Against a receiver that answers `503`, each attempt fails with a retryable
error, and the step gives up after the third:

```text
  ↻ post_event                     retry_wait server responded 503
  ↻ post_event                     retry_wait server responded 503
  ✗ post_event                     failed server responded 503 (gave up after 3 attempts)

FAILED wh-4  NotifyWebhook@1.1.0
  elapsed: 3.3s
  compensation: none
  error: HTTP_ERROR step `post_event` failed: server responded 503 (gave up after 3 attempts)
```

```text
+    70ms   #5  StepAttemptFailed         step=post_event attempt=1 error=HTTP_ERROR server responded 503
+    71ms   #6  RetryScheduled            step=post_event delay_ms=1000
+    1.2s   #9  StepAttemptFailed         step=post_event attempt=2 error=HTTP_ERROR server responded 503
+    1.2s  #10  RetryScheduled            step=post_event delay_ms=2000
+    3.3s  #13  StepAttemptFailed         step=post_event attempt=3 error=HTTP_ERROR server responded 503
+    3.3s  #14  StepFailed                step=post_event error=HTTP_ERROR server responded 503 (gave up after 3 attempts)
```

## Secrets and the adapter

Everything a step sends is part of the execution's history: the workflow input
in `WorkflowCreated`, and the evaluated step input, including headers, in
`StepScheduled`. `orbit logs`, `orbit logs --json` and `orbit inspect` hide
values whose keys look sensitive, such as `api_key` and `authorization`:

```json
"headers": { "authorization": "***", "idempotency-key": "order-created-A-1" }
```

That redaction applies to the CLI's output only. The database and the HTTP API
(`GET /v1/executions/{id}` and `/history`) hold the values in clear text, and
anyone with the admin token can read them. For long-lived credentials, call
the service from a worker that reads the secret from its own environment
instead of passing it through workflow input.

## Security checklist

- Enable the adapter only on runtimes whose workflow authors you trust to make
  outbound requests from the runtime host.
- The adapter accepts any `http://` or `https://` URL, including internal
  addresses reachable from the runtime host. Restrict the host's egress with a
  firewall or proxy if workflows must not reach internal services.
- Prefer `https://` URLs. Plain `http://` sends headers and bodies
  unencrypted.

## See also

- [HTTP adapter](../language/http-adapter.md)
- [Retries and timeouts](../language/retries-timeouts.md)
- [Running the server](../runtime/server.md)
- [Security](../runtime/security.md)
- [Idempotency](../language/idempotency.md)
