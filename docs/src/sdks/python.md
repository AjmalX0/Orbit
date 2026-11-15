# Python

The Python SDK, `orbit-lang-sdk`, runs ORBIT task handlers written in Python.
It uses only the standard library, supports plain and `async def` handlers,
runs a configurable number of tasks in parallel on threads, and handles
registration, long polling, heartbeats, cancellation, result delivery through
runtime outages, and graceful shutdown on `SIGINT` and `SIGTERM`. It also
includes a small `Client` for starting workflows and waiting for their results.
Read [How workers work](overview.md) first for the concepts this page relies on.

## Install

```bash
pip install orbit-lang-sdk
```

The package requires Python 3.9 or later and has no dependencies. The
distribution is named `orbit-lang-sdk`; the import name is `orbit_sdk`.

## A minimal worker

This worker implements `text.normalize`, the Python step of
[`examples/polyglot.orbit`](https://github.com/AjmalX0/Orbit/blob/main/examples/polyglot.orbit):

```orbit
task "text.normalize" { version "1.0.0" input { text: String } output { text: String } }

step normalize { worker "text.normalize" using python with { text: input.text } }
```

```python
import re

from orbit_sdk import TaskError, Worker

worker = Worker("text-python")


@worker.task("text.normalize", version="1.0.0")
def normalize(input, ctx):
    text = input["text"]
    if not text.strip():
        raise TaskError("EMPTY_TEXT", "text must not be blank", retryable=False)
    ctx.log("normalizing", chars=len(text))
    text = re.sub(r"[^\w\s']", " ", text.lower())
    return {"text": re.sub(r"\s+", " ", text).strip()}


if __name__ == "__main__":
    worker.run()
```

Run it against a local runtime. The workflow has four steps in four
languages, so start the other three example workers from the repository root
as well:

```bash
orbit server &
python3 text_worker.py &
node examples/workers/polyglot/text_worker.ts &
(cd examples/workers/polyglot/go && go run .) &
cargo run -p orbit-worker --example text_worker &

orbit run examples/polyglot.orbit \
  --input '{"text": "The quick brown fox jumps over the lazy dog. The dog sleeps."}'
```

With blank text the handler raises a non-retryable error, so the step fails
on its first attempt, the other steps are cancelled, and `orbit run` reports:

```text
FAILED 4ea9f162-...  DocumentInsights@1.0.0
  compensation: none
  error: EMPTY_TEXT step `normalize` failed: text must not be blank
```

## Registering tasks

Register each task with the `@worker.task` decorator or with
`worker.register`. The decorator returns your function unchanged, so you can
still call it directly in tests.

```python
@worker.task("payments.charge", version="1.0.0")
def charge(input, ctx):
    ...

def refund(input, ctx):
    ...

worker.register("payments.refund", refund, version="1.0.0")
```

The name and version must match the `task` declaration in your `.orbit` files
exactly. A worker registers each task name once; registering the same name
again raises `ValueError`. Register every task before calling `run()`.

## Handlers and results

A handler takes two arguments, the task input as a `dict` and a
[`Context`](#context), and returns a `dict` that becomes the step's output:

```python
@worker.task("inventory.reserve", version="1.0.0")
def reserve(input, ctx):
    return {"reservation_id": f"res-{input['order_id']}"}
```

- When the runtime sends no input, `input` is `{}`.
- Returning `None` reports `{}`.
- Returning anything other than a `dict` fails the attempt with
  `INVALID_RESULT`, which is not retried.
- The runtime validates the output against the contract's output schema; a
  mismatch fails the step with `OUTPUT_CONTRACT_VIOLATION`.

`async def` handlers are supported. The SDK runs each one to completion with
`asyncio.run` on the poll thread that received the task:

```python
@worker.task("text.fetch", version="1.0.0")
async def fetch(input, ctx):
    body = await download(input["url"])
    return {"bytes": len(body)}
```

## Raising task errors

Raise `TaskError` to fail an attempt with a structured error:

```python
from orbit_sdk import TaskError

@worker.task("payments.charge", version="1.0.0")
def charge(input, ctx):
    if input["amount"] > 5000:
        raise TaskError("CARD_DECLINED", "limit exceeded", retryable=False,
                        details={"limit": 5000})
    ...
```

| Argument | Type | Default | Meaning |
|---|---|---|---|
| `code` | `str` | required | Error code, matched exactly by the step's `non_retryable` list. |
| `message` | `str` | `""` | Human-readable message. |
| `retryable` | `bool` | `True` | `False` fails the step immediately, whatever its retry policy. |
| `details` | any JSON value | `None` | Stored with the error in the execution history. |

Any other exception is reported as a retryable `UNHANDLED_EXCEPTION`. The
message is `<ExceptionType>: <text>`, and `details.traceback` holds the last
frames of the traceback. The SDK also logs the traceback at `ERROR` level.

## Context

The second handler argument describes the attempt being executed.

| Member | Type | Meaning |
|---|---|---|
| `token` | `str` | Identifies this delivery. The SDK echoes it in the result. |
| `execution_id` | `str` | The workflow execution. |
| `workflow` | `str` | Workflow name. |
| `workflow_version` | `str` | Workflow version. |
| `step_id` | `str` | The step, or `compensate:<step>` for a compensation; `step[i]` for a fan-out element. |
| `task_name` | `str` | The task name, for example `text.normalize`. |
| `attempt` | `int` | 1-based attempt number. Redeliveries keep the number. |
| `idempotency_key` | `str` | Stable across retries and redeliveries. Use it for side effects. |
| `redelivery` | `bool` | `True` when the outcome of an earlier delivery of this attempt is unknown. |
| `deadline_ms` | `int` or `None` | Absolute deadline of the attempt in Unix epoch milliseconds, when the step has a `timeout`. |
| `cancelled` | `threading.Event` | Set when the runtime asks the attempt to stop. |
| `is_cancelled` | `bool` (property) | `True` once `cancelled` is set. |
| `remaining_ms()` | `int` or `None` | Milliseconds left before `deadline_ms`, never negative. |
| `log(message, **fields)` | `None` | Logs `message` at `INFO` with the execution, step, attempt and `fields` as JSON. |

The context does not expose the contract version or `timeout_ms`.

## Cancellation and timeouts

When an attempt times out, or its workflow is cancelled or fails, the runtime
lists the attempt in the next heartbeat response and the SDK sets
`ctx.cancelled`. The SDK does not interrupt your handler and does not act on
`deadline_ms` by itself, so check the flag between units of work:

```python
@worker.task("reports.render", version="1.0.0")
def render(input, ctx):
    pages = []
    for section in input["sections"]:
        if ctx.is_cancelled:
            raise TaskError("CANCELLED", "stopped by the runtime")
        pages.append(render_section(section, timeout=(ctx.remaining_ms() or 30000) / 1000))
    return {"pages": len(pages)}
```

`ctx.cancelled.wait(seconds)` is a cancellable sleep. Heartbeats run on a
separate daemon thread at the interval the runtime requests, so a long-running
handler keeps its session alive.

## Concurrency

`Worker(concurrency=N)` starts N poll threads (default 4). Each thread runs
one task at a time, so N is the maximum number of tasks this process runs at
once. Synchronous handlers run on the poll thread; make them thread-safe if
they share state. For CPU-bound work, run several worker processes rather than
raising `concurrency`, because of the global interpreter lock.

## Credentials and TLS

The worker reads its credentials from the environment:

| Variable | Meaning |
|---|---|
| `ORBIT_SERVER` | Runtime URL. Default `http://127.0.0.1:7233`. |
| `ORBIT_WORKER_TOKEN` | Bearer token. Takes precedence over `ORBIT_TOKEN`. |
| `ORBIT_TOKEN` | Bearer token used when `ORBIT_WORKER_TOKEN` is unset. |
| `ORBIT_CA_CERT` | PEM CA certificate. When set, it is the only CA trusted. |
| `ORBIT_CLIENT_CERT` | PEM client certificate for mutual TLS. |
| `ORBIT_CLIENT_KEY` | PEM private key. Optional when the key is in the certificate file. |

The `server` and `token` arguments of `Worker` override the first three. The
`Worker` constructor has no TLS arguments; it builds its `Client` from the
environment. To configure TLS in code, replace `worker.client` before calling
`run()`, and keep the timeout above the poll wait:

```python
from orbit_sdk import Client, Worker

worker = Worker("payments-1")
worker.client = Client(
    server="https://orbit.internal:7233",
    ca_cert="ca.pem",
    client_cert="payments-1.pem",
    client_key="payments-1.key",
    timeout=worker.poll_wait_ms / 1000 + 15,
)
```

With a per-worker identity from a workers file, set the worker id to the
identity's `id` and the token to the identity's token.

## Logging

The SDK logs to the standard `logging` logger named `orbit`. `Worker.run()`
calls `logging.basicConfig` with the level from `ORBIT_LOG_LEVEL` (default
`INFO`); because `basicConfig` does nothing when logging is already
configured, your own configuration takes precedence when you set it up before
calling `run()`. At `INFO` the SDK logs registration, each task it executes,
task failures, cancellations and ignored results. Runtime warnings, such as an
unused contract version, are logged at `WARNING`.

## Shutdown

`worker.run()` blocks until the process receives `SIGINT` or `SIGTERM`, or
until `worker.stop()` is called from another thread. It then stops polling,
lets running handlers finish and deliver their results, unregisters, and
returns. A poll that is already waiting is not interrupted, so shutdown can
take up to `poll_wait_ms` (20 seconds by default); lower it for faster
shutdown.

`run()` installs signal handlers only when called from the main thread.
Pass `install_signal_handlers=False` to keep your own. `worker.start()` runs
the worker on a background daemon thread without signal handlers and returns
the thread.

## Starting workflows from Python

`Client` calls the runtime's HTTP API. Starting executions requires the admin
token, so pass it explicitly when the environment also holds a worker token:

```python
import os

from orbit_sdk import Client

client = Client(token=os.environ["ORBIT_ADMIN_TOKEN"])
execution_id = client.start("DocumentInsights", {"text": "Hello world"},
                            execution_id="doc-42")
result = client.wait(execution_id, timeout_ms=60000)
print(result["status"], result.get("output"))
```

Starting with an `execution_id` that already exists returns the existing
execution instead of creating a duplicate.

### Signals and queries

`signal` sends a typed signal declared with `signal <name> { ... }` in the
workflow, and `query` evaluates a declared query. Both need the admin token
when authentication is enabled.

```python
import os

from orbit_sdk import ApiError, Client

client = Client(token=os.environ["ORBIT_ADMIN_TOKEN"])

print(client.query("order-A-100", "status"))
# {'needs_approval': True, 'approved': None, 'reviewer': None, 'charged': False}

seq = client.signal("order-A-100", "review", {"approved": True, "reviewer": "dana"})

try:
    client.signal("order-A-100", "review", {"approved": "yes", "reviewer": "dana"})
except ApiError as e:
    print(e.code)  # SIGNAL_PAYLOAD_INVALID
```

`signal` returns the history sequence number of the recorded
`SignalReceived` event. The runtime validates the payload and buffers the
signal until an `await` step takes it. See
[Signals and queries](../language/signals.md).

## API reference

### `Worker`

```python
Worker(worker_id=None, server=None, token=None, concurrency=4,
       runtime="python", poll_wait_ms=20000)
```

| Parameter | Type | Default | Meaning |
|---|---|---|---|
| `worker_id` | `str` | `python-<hostname>-<pid>` | Stable identity sent at registration. |
| `server` | `str` | `ORBIT_SERVER`, then `http://127.0.0.1:7233` | Runtime URL. |
| `token` | `str` | `ORBIT_WORKER_TOKEN`, then `ORBIT_TOKEN` | Bearer token. Pass `""` to send none. |
| `concurrency` | `int` | `4` | Number of poll threads; values below 1 become 1. |
| `runtime` | `str` | `"python"` | Announced runtime, matched against `using <language>`. |
| `poll_wait_ms` | `int` | `20000` | `wait_ms` of each long poll. The runtime caps it at 60000. |

| Member | Meaning |
|---|---|
| `task(name, version="1.0.0")` | Decorator that registers `fn(input, ctx)` for `name@version` and returns `fn`. |
| `register(name, fn, version="1.0.0")` | Registers `fn` for `name@version`. Raises `ValueError` for a duplicate name. |
| `run(install_signal_handlers=True)` | Registers, polls and blocks until stopped. Raises `RuntimeError` when no task is registered, and `ApiError` when the runtime refuses registration (for example `401` or `403`). Connection errors and 5xx responses during registration are retried. |
| `start()` | Runs `run(install_signal_handlers=False)` on a daemon thread and returns the `threading.Thread`. |
| `stop()` | Asks `run()` to finish. Returns immediately. |
| `worker_id`, `runtime`, `concurrency`, `poll_wait_ms` | The resolved settings. |
| `client` | The `Client` used for every protocol call. |

### `TaskError`

```python
TaskError(code, message="", retryable=True, details=None)
```

An `Exception` subclass with the attributes `code`, `message`, `retryable`
and `details`. See [Raising task errors](#raising-task-errors).

### `Context`

See [Context](#context). The SDK creates it; you do not construct one.

### `Client`

```python
Client(server=None, token=None, timeout=70.0, ca_cert=None,
       client_cert=None, client_key=None)
```

| Parameter | Type | Default | Meaning |
|---|---|---|---|
| `server` | `str` | `ORBIT_SERVER`, then `http://127.0.0.1:7233` | Runtime URL. |
| `token` | `str` | `ORBIT_WORKER_TOKEN`, then `ORBIT_TOKEN` | Bearer token. |
| `timeout` | `float` | `70.0` | Socket timeout in seconds. |
| `ca_cert` | `str` | `ORBIT_CA_CERT` | PEM CA file to trust. |
| `client_cert` | `str` | `ORBIT_CLIENT_CERT` | PEM client certificate. |
| `client_key` | `str` | `ORBIT_CLIENT_KEY` | PEM private key. |

| Method | Returns | Meaning |
|---|---|---|
| `request(method, path, body=None)` | `(status, body)` | Sends a JSON request. `body` is `None` for an empty response. |
| `get(path)` | parsed body | `GET` request. |
| `post(path, body)` | `(status, body)` | `POST` request. |
| `start(workflow, input=None, version=None, execution_id=None)` | `str` | Starts an execution and returns its id. |
| `wait(execution_id, timeout_ms=30000)` | `dict` | Long-polls until the execution finishes or the timeout elapses, and returns its state. |
| `signal(execution_id, name, payload=None)` | `int` | Sends the signal `name` with `payload` (`None` sends `{}`) and returns the history sequence number of the recorded signal. Raises `ApiError` with `UNKNOWN_SIGNAL`, `SIGNAL_PAYLOAD_INVALID`, `EXECUTION_FINISHED` or `NOT_FOUND`. |
| `query(execution_id, name)` | any | Evaluates the query `name` and returns its value. Raises `ApiError` with `UNKNOWN_QUERY`, `QUERY_FAILED` or `NOT_FOUND`. |

### `ApiError`

Raised by `Client` for an error response or a connection failure. Attributes:
`status` (`int`, `0` for a connection failure), `code` (the runtime's error
code, `CONNECTION`, or `HTTP_ERROR`), `message`, and the property
`is_connection_error`.

## Testing

Handlers are plain functions, so unit-test them without a runtime. A
`SimpleNamespace` can stand in for the context:

```python
from types import SimpleNamespace

from text_worker import normalize

def test_normalize():
    ctx = SimpleNamespace(log=lambda *a, **k: None, is_cancelled=False)
    assert normalize({"text": "Hello,  World!"}, ctx) == {"text": "hello world"}
```

For an integration test, start `orbit server` with a temporary database,
deploy your workflow, run the worker on a background thread with
`worker.start()`, and drive executions with `Client`:

```python
worker = Worker("test-worker", poll_wait_ms=1000)
...
thread = worker.start()
client = Client(token="admin-token")
execution_id = client.start("DocumentInsights", {"text": "a b a"})
assert client.wait(execution_id)["status"] == "COMPLETED"
worker.stop()
thread.join()
```

A short `poll_wait_ms` makes `stop()` return quickly. The SDK's own tests in
`sdks/python/tests` use a scripted fake runtime built on `http.server`, which
is a useful model for testing retries and session loss.

## See also

- [How workers work](overview.md)
- [Worker protocol](../reference/protocol.md)
- [Task contracts](../language/tasks.md)
- [Retries and timeouts](../language/retries-timeouts.md)
- [Idempotency](../language/idempotency.md)
- [Security](../runtime/security.md)
- [Polyglot workflows](../guides/polyglot.md)
