# orbit-sdk (Python)

Python worker SDK for the [ORBIT](../../README.md) runtime. Standard library
only, Python 3.9+.

```python
from orbit_sdk import Worker, TaskError

worker = Worker("payments-worker", concurrency=8)   # server: $ORBIT_SERVER, token: $ORBIT_TOKEN

@worker.task("payments.charge", version="1.0.0")
def charge(input, ctx):
    if input["amount"] > 5000:
        raise TaskError("CARD_DECLINED", "limit exceeded", retryable=False)
    # ctx.idempotency_key is stable across retries and redeliveries
    return {"transaction_id": f"tx-{input['order_id']}", "status": "captured"}

worker.run()   # Ctrl-C / SIGTERM drains in-flight tasks and unregisters
```

* Handlers receive `(input: dict, ctx: Context)` and return a `dict`. `async def`
  handlers are supported.
* `ctx` exposes `execution_id`, `step_id`, `attempt`, `idempotency_key`,
  `redelivery`, `deadline_ms`, `remaining_ms()`, `is_cancelled` / `cancelled`
  (a `threading.Event` set when the runtime cancels the attempt) and `log()`.
* Raise `TaskError(code, message, retryable=..., details=...)` for business
  errors. Any other exception is reported as a retryable `UNHANDLED_EXCEPTION`
  with its traceback.
* The SDK re-registers after a runtime restart and keeps retrying result
  delivery through outages, so results produced during a crash are not lost.

Starting workflows from Python:

```python
from orbit_sdk import Client
c = Client()
eid = c.start("OrderProcessing", {"order_id": "A-1", ...}, execution_id="order-A-1")
print(c.wait(eid)["status"])
```
