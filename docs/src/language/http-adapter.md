# HTTP adapter

`orbit.http.request` is a built-in task that calls an HTTP endpoint from the
runtime itself, without a worker. Use it for webhooks, health checks and
simple API calls that need no business logic. This page covers the task's
contract, how requests are built, how responses and errors are mapped, and
how to enable the adapter on the server.

## Enabling the adapter

The adapter lets workflow authors make outbound requests from the runtime
host, so it is disabled by default. Start the server with
`--enable-http-adapter`:

```bash
orbit server --enable-http-adapter
```

On a server without the flag, every `orbit.http.request` attempt fails with
`ADAPTER_DISABLED`, which is not retryable. Workflows that use the adapter
still compile and register. See [Running the server](../runtime/server.md) and
[Security](../runtime/security.md).

## Contract

The task is declared in the prelude that every compilation includes:

```text
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

| Input field | Type | Default | Meaning |
|-------------|------|---------|---------|
| `method` | `String` | `"GET"` | The HTTP method, case-insensitive: `GET`, `HEAD`, `OPTIONS`, `DELETE`, `POST`, `PUT` or `PATCH`. |
| `url` | `String` | required | The URL. Only `http://` and `https://` URLs are allowed. |
| `headers` | `Map<String, String>` | `{}` | Request headers. |
| `body` | `JSON` | `null` | The request body. Sent only for `POST`, `PUT` and `PATCH`. |

| Output field | Type | Meaning |
|--------------|------|---------|
| `status` | `Int` | The HTTP status code. |
| `headers` | `Map<String, String>` | Response headers, with lowercase names. If a header repeats, the last value wins. |
| `body` | `JSON` | The response body, parsed as JSON when possible, otherwise the body text as a string (`""` for an empty body). |

## Example

```orbit
workflow NotifyWebhook {
    version "1.0.0"
    input { order_id: String, url: String }

    step deliver {
        worker "orbit.http.request"
        with {
            method: "POST"
            url: input.url
            headers: { "content-type": "application/json", "x-order-id": input.order_id }
            body: { event: "order.completed", order_id: input.order_id }
        }
        timeout 10s
        retry { attempts: 5, backoff: exponential, initial_delay: 1s, max_delay: 30s }
    }

    return { delivered: deliver.status < 300, status: deliver.status }
}
```

`deliver.status` is an `Int`, `deliver.headers["content-type"]` is a
`String?`, and `deliver.body` is `JSON`, so its fields can be read without
type errors (`deliver.body.id`) and are checked at runtime.

## Request body encoding

For `POST`, `PUT` and `PATCH`:

| `body` value | What is sent |
|--------------|--------------|
| `null` (the default) | an empty body |
| a string | the string as is; set `content-type` yourself |
| any other value | the value serialized as JSON |

For `GET`, `HEAD`, `OPTIONS` and `DELETE`, `body` is ignored.

## Status codes and errors

| Outcome | Result | Retryable |
|---------|--------|-----------|
| `1xx` to `4xx` except `429` | The step succeeds with the response. Check `status` in the workflow. | |
| `429` and `5xx` | The attempt fails with `HTTP_ERROR` (`server responded 503`). The error details carry the status and body. | yes |
| Network or DNS failure | The attempt fails with `HTTP_ERROR` (`request failed: ...`). | yes |
| Request slower than the timeout | The attempt fails with `HTTP_ERROR` or `TIMEOUT`, whichever the runtime records first. | yes |
| URL not `http://` or `https://` | `HTTP_ERROR` (`only http:// and https:// URLs are allowed`) | no |
| Unsupported method | `HTTP_ERROR` (`unsupported method TRACE`) | no |
| Missing `url` at runtime | `HTTP_ERROR` (`missing url`) | no |
| Adapter disabled | `ADAPTER_DISABLED` | no |

A `404` or `400` is a successful step: the workflow decides what it means. To
treat those as failures, branch on `status` or check it in a later step.
Retryable failures are retried only if the step has a `retry` policy with more
than one attempt; see [Retries and timeouts](retries-timeouts.md).

## Timeouts and limits

| Setting | Value |
|---------|-------|
| Request timeout | the step `timeout`, or 30 seconds when the step sets none |
| Response body | read up to 8 MiB |

The step `timeout` applies to the whole request, including connecting and
reading the body. `queue_timeout` and `using` have no practical effect,
because the runtime executes the request as soon as the step is scheduled.

## Idempotency

The adapter does not send the step's idempotency key. If the endpoint supports
idempotency keys, pass one as a header:

```orbit
workflow Charge {
    version "1.0.0"
    input { order_id: String, amount: Float }
    step charge {
        worker "orbit.http.request"
        with {
            method: "POST"
            url: "https://payments.example.com/v1/charges"
            headers: { "idempotency-key": "charge-" + input.order_id }
            body: { amount: input.amount }
        }
        retry { attempts: 3, backoff: exponential, initial_delay: 500ms }
    }
}
```

## See also

- [Calling HTTP services guide](../guides/http.md)
- [Task contracts](tasks.md#built-in-tasks)
- [Retries and timeouts](retries-timeouts.md)
- [Running the server](../runtime/server.md)
- [Security](../runtime/security.md)
