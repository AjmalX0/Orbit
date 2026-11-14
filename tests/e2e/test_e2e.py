"""End-to-end tests: real `orbit` server, real Python and TypeScript workers.

Covers RFC-0001 §15.5 acceptance criteria that need separate processes:
polyglot execution, conditions, compensation after a failure, retries,
inspection through the CLI, and crash recovery (the runtime is killed with
SIGKILL while a task is in flight and restarted).

    cargo build && python3 -I tests/e2e/test_e2e.py
"""

import json
import os
import signal
import socket
import subprocess
import sys
import tempfile
import time
import unittest
import urllib.request

ROOT = os.path.abspath(os.path.join(os.path.dirname(__file__), "..", ".."))
ORBIT = os.environ.get("ORBIT_BIN", os.path.join(ROOT, "target", "debug", "orbit"))
WORKFLOW = os.path.join(ROOT, "examples", "order_processing.orbit")


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


class Cluster:
    def __init__(self):
        self.dir = tempfile.mkdtemp(prefix="orbit-e2e-")
        self.port = free_port()
        self.url = f"http://127.0.0.1:{self.port}"
        self.db = os.path.join(self.dir, "orbit.db")
        self.server = None
        self.workers = []
        self.logs = []
        self.env = {**os.environ, "ORBIT_SERVER": self.url, "PYTHONPATH": os.path.join(ROOT, "sdks", "python"), "NO_COLOR": "1"}

    def log(self, name):
        f = open(os.path.join(self.dir, f"{name}.log"), "ab")
        self.logs.append(f)
        return f

    def start_server(self):
        self.server = subprocess.Popen(
            [ORBIT, "server", "--db", self.db, "--addr", f"127.0.0.1:{self.port}", "--session-timeout", "3"],
            stdout=self.log("server"), stderr=subprocess.STDOUT, env=self.env,
        )
        deadline = time.time() + 15
        while time.time() < deadline:
            try:
                urllib.request.urlopen(self.url + "/v1/health", timeout=1).read()
                return
            except OSError:
                time.sleep(0.1)
        raise RuntimeError("server did not start: " + open(os.path.join(self.dir, "server.log")).read())

    def kill_server(self):
        self.server.send_signal(signal.SIGKILL)
        self.server.wait()

    def start_workers(self, **extra_env):
        env = {**self.env, **extra_env}
        self.workers.append(subprocess.Popen(
            [sys.executable, os.path.join(ROOT, "examples", "workers", "python", "order_workers.py")],
            stdout=self.log("python"), stderr=subprocess.STDOUT, env=env,
        ))
        self.workers.append(subprocess.Popen(
            ["node", os.path.join(ROOT, "examples", "workers", "typescript", "order_workers.ts")],
            stdout=self.log("typescript"), stderr=subprocess.STDOUT, env=env,
        ))
        # Wait until both workers are online.
        deadline = time.time() + 20
        while time.time() < deadline:
            online = [w for w in self.get("/v1/workers")["workers"] if w["online"]]
            if len(online) >= 2:
                return
            time.sleep(0.2)
        raise RuntimeError("workers did not register")

    def stop(self):
        for p in self.workers:
            p.terminate()
        for p in self.workers:
            try:
                p.wait(timeout=10)
            except subprocess.TimeoutExpired:
                p.kill()
        if self.server and self.server.poll() is None:
            self.server.terminate()
            self.server.wait(timeout=10)
        for f in self.logs:
            f.close()

    def cli(self, *args, check=True):
        r = subprocess.run([ORBIT, *args, "--server", self.url], capture_output=True, text=True, env=self.env, timeout=120)
        if check and r.returncode != 0:
            raise AssertionError(f"orbit {' '.join(args)} failed ({r.returncode}):\n{r.stdout}\n{r.stderr}")
        return r

    def get(self, path):
        with urllib.request.urlopen(self.url + path, timeout=40) as r:
            return json.loads(r.read())

    def post(self, path, body):
        req = urllib.request.Request(self.url + path, data=json.dumps(body).encode(), method="POST", headers={"content-type": "application/json"})
        with urllib.request.urlopen(req, timeout=40) as r:
            return json.loads(r.read())

    def wait(self, execution_id, timeout=40):
        deadline = time.time() + timeout
        while time.time() < deadline:
            s = self.get(f"/v1/executions/{execution_id}/wait?timeout_ms=5000")
            if s["status"] in ("COMPLETED", "FAILED", "CANCELLED", "TIMED_OUT"):
                return s
        raise AssertionError(f"execution {execution_id} did not finish: {s['status']}")


def order(order_id, customer="cust-1", amount=120.0):
    return {"order_id": order_id, "customer_id": customer, "amount": amount, "items": [{"sku": "BOOK-1", "quantity": 2}]}


class EndToEnd(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.c = Cluster()
        cls.c.start_server()
        cls.c.start_workers()
        cls.c.cli("deploy", WORKFLOW)

    @classmethod
    def tearDownClass(cls):
        cls.c.stop()
        print(f"\nlogs: {cls.c.dir}")

    def start(self, input, execution_id=None):
        return self.c.post("/v1/executions", {"workflow": "OrderProcessing", "input": input, "execution_id": execution_id})["execution_id"]

    def test_polyglot_happy_path_via_cli(self):
        r = self.c.cli("run", WORKFLOW, "--input", json.dumps(order("A-1")))
        out = json.loads(r.stdout)
        self.assertEqual(out["status"], "completed")
        self.assertEqual(out["transaction_id"], "tx-A-1")
        # Steps ran on both runtimes.
        ex_id = [l for l in r.stderr.splitlines() if "execution" in l][0].split()[-1]
        state = self.c.get(f"/v1/executions/{ex_id}")
        workers = {u.get("worker_id") for u in state["units"].values()}
        self.assertEqual(workers, {"orders-python", "orders-typescript"})
        # History is inspectable through the CLI.
        logs = self.c.cli("logs", ex_id).stdout
        self.assertIn("WorkflowCompleted", logs)
        self.assertIn("StepDispatched", logs)
        status = self.c.cli("status", ex_id).stdout
        self.assertIn("COMPLETED", status)

    def test_condition_skips_payment(self):
        s = self.c.wait(self.start(order("A-2", customer="risky-7")))
        self.assertEqual(s["status"], "COMPLETED")
        self.assertEqual(s["output"]["status"], "held_for_review")
        self.assertIsNone(s["output"]["transaction_id"])
        self.assertEqual(s["units"]["process_payment"]["status"], "SKIPPED")

    def test_declined_payment_compensates_inventory(self):
        s = self.c.wait(self.start(order("A-3", amount=9000)))
        self.assertEqual(s["status"], "FAILED")
        self.assertEqual(s["error"]["code"], "CARD_DECLINED")
        self.assertEqual(s["compensation"], "completed")
        self.assertEqual(s["units"]["compensate:reserve_inventory"]["status"], "SUCCEEDED")
        self.assertNotIn("compensate:process_payment", s["units"], "a step that never succeeded is not compensated")

    def test_idempotent_start(self):
        a = self.start(order("A-4"), execution_id="order-A-4")
        b = self.start(order("A-4"), execution_id="order-A-4")
        self.assertEqual(a, b)
        self.assertEqual(self.c.wait(a)["status"], "COMPLETED")

    def test_invalid_input_rejected(self):
        with self.assertRaises(urllib.error.HTTPError) as e:
            self.start({"order_id": "x"})
        self.assertEqual(e.exception.code, 400)

    def test_z_crash_recovery(self):
        # amount 4242 makes the charge take 3s; kill the runtime while it runs.
        ex = self.start(order("A-5", amount=4242))
        deadline = time.time() + 20
        while self.c.get(f"/v1/executions/{ex}")["units"].get("process_payment", {}).get("status") != "DISPATCHED":
            self.assertLess(time.time(), deadline, "payment never dispatched")
            time.sleep(0.05)
        self.c.kill_server()
        time.sleep(1)
        self.c.start_server()
        s = self.c.wait(ex, timeout=60)
        self.assertEqual(s["status"], "COMPLETED", json.dumps(s, indent=2))
        self.assertEqual(s["output"]["transaction_id"], "tx-A-5")
        history = self.c.get(f"/v1/executions/{ex}/history")["events"]
        charges = [e for e in history if e["type"] == "StepSucceeded" and e["step"] == "process_payment"]
        self.assertEqual(len(charges), 1, "exactly one recorded success for the charge")


if __name__ == "__main__":
    unittest.main(verbosity=2)
