"""ORBIT worker SDK for Python.

A worker announces the tasks it implements, long-polls the ORBIT runtime for
work, runs your handler and reports the result. It has no dependencies
outside the standard library.

    from orbit_sdk import Worker, TaskError

    worker = Worker("payments-worker")

    @worker.task("payments.charge", version="1.0.0")
    def charge(input, ctx):
        if input["amount"] <= 0:
            raise TaskError("INVALID_AMOUNT", "amount must be positive", retryable=False)
        return {"transaction_id": f"tx-{ctx.idempotency_key}", "status": "ok"}

    worker.run()
"""

from .client import ApiError, Client
from .worker import Context, TaskError, Worker

__all__ = ["Worker", "Context", "TaskError", "Client", "ApiError"]
__version__ = "0.2.0"
