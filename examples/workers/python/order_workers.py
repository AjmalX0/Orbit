"""Python workers for examples/order_processing.orbit: risk scoring and payments.

Run:  PYTHONPATH=sdks/python python examples/workers/python/order_workers.py

Demo behaviour (so every path of the workflow can be exercised):
  * customers whose id starts with "risky" score 0.95 -> payment is skipped
  * amounts above 5000 are declined (non-retryable) -> compensation runs
  * an amount of exactly 4242 makes the charge take 3 seconds (crash-test hook)
  * ORBIT_FLAKY=1 makes the first risk attempt fail to show retries
"""

import os
import time

from orbit_sdk import TaskError, Worker

worker = Worker(os.environ.get("ORBIT_WORKER_ID", "orders-python"), concurrency=8)

# Idempotency: results are remembered by key, so a redelivered charge never
# charges twice. A real service would store this in its database.
_charges = {}
_attempts = {}


@worker.task("risk.analyze", version="1.0.0")
def analyze_risk(input, ctx):
    if os.environ.get("ORBIT_FLAKY") and _attempts.setdefault(ctx.execution_id, 0) == 0:
        _attempts[ctx.execution_id] += 1
        raise TaskError("RISK_SERVICE_UNAVAILABLE", "upstream timed out", retryable=True)
    score = 0.95 if input["customer_id"].startswith("risky") else min(0.1 + input["amount"] / 100_000, 0.7)
    reasons = ["flagged customer"] if score > 0.8 else []
    return {"score": round(score, 3), "reasons": reasons}


@worker.task("payments.charge", version="1.0.0")
def charge(input, ctx):
    if ctx.idempotency_key in _charges:
        ctx.log("duplicate charge request; returning the original result")
        return _charges[ctx.idempotency_key]
    if input["amount"] == 4242:
        time.sleep(3)
    if input["amount"] > 5000:
        raise TaskError("CARD_DECLINED", f"charge of {input['amount']} {input['currency']} declined", retryable=False)
    result = {"transaction_id": f"tx-{input['order_id']}", "status": "captured"}
    _charges[ctx.idempotency_key] = result
    return result


@worker.task("payments.refund", version="1.0.0")
def refund(input, ctx):
    ctx.log("refunding", transaction_id=input["transaction_id"])
    return {"refunded": True}


if __name__ == "__main__":
    worker.run()
