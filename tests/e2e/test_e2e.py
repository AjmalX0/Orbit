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
import shutil
import socket
import ssl
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
    def __init__(self, server_args=(), tls_ca=None, admin_token=None):
        self.dir = tempfile.mkdtemp(prefix="orbit-e2e-")
        self.port = free_port()
        self.url = f"{'https' if tls_ca else 'http'}://127.0.0.1:{self.port}"
        self.db = os.path.join(self.dir, "orbit.db")
        self.server = None
        self.workers = []
        self.logs = []
        self.server_args = list(server_args)
        self.ssl = ssl.create_default_context(cafile=tls_ca) if tls_ca else None
        self.headers = {"authorization": f"Bearer {admin_token}"} if admin_token else {}
        self.env = {**os.environ, "ORBIT_SERVER": self.url, "PYTHONPATH": os.path.join(ROOT, "sdks", "python"), "NO_COLOR": "1"}
        for k in ("ORBIT_TOKEN", "ORBIT_WORKER_TOKEN", "ORBIT_CA_CERT"):
            self.env.pop(k, None)
        if admin_token:
            self.env["ORBIT_TOKEN"] = admin_token
        if tls_ca:
            self.env["ORBIT_CA_CERT"] = tls_ca
            self.env["NODE_EXTRA_CA_CERTS"] = tls_ca

    def log(self, name):
        f = open(os.path.join(self.dir, f"{name}.log"), "ab")
        self.logs.append(f)
        return f

    def start_server(self):
        self.server = subprocess.Popen(
            [ORBIT, "server", "--db", self.db, "--addr", f"127.0.0.1:{self.port}", "--session-timeout", "3", *self.server_args],
            stdout=self.log("server"), stderr=subprocess.STDOUT, env=self.env,
        )
        deadline = time.time() + 15
        while time.time() < deadline:
            try:
                urllib.request.urlopen(self.url + "/v1/health", timeout=1, context=self.ssl).read()
                return
            except OSError:
                time.sleep(0.1)
        raise RuntimeError("server did not start: " + open(os.path.join(self.dir, "server.log")).read())

    def kill_server(self):
        self.server.send_signal(signal.SIGKILL)
        self.server.wait()

    def spawn(self, name, argv, **extra_env):
        self.workers.append(subprocess.Popen(argv, stdout=self.log(name), stderr=subprocess.STDOUT, env={**self.env, **extra_env}))

    def wait_online(self, count):
        deadline = time.time() + 30
        while time.time() < deadline:
            online = [w for w in self.get("/v1/workers")["workers"] if w["online"]]
            if len(online) >= count:
                return
            time.sleep(0.2)
        raise RuntimeError("workers did not register; logs in " + self.dir)

    def start_workers(self, **extra_env):
        self.spawn("python", [sys.executable, os.path.join(ROOT, "examples", "workers", "python", "order_workers.py")], **extra_env)
        self.spawn("typescript", ["node", os.path.join(ROOT, "examples", "workers", "typescript", "order_workers.ts")], **extra_env)
        self.wait_online(2)

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

    def get(self, path, headers=None):
        req = urllib.request.Request(self.url + path, headers=self.headers if headers is None else headers)
        with urllib.request.urlopen(req, timeout=40, context=self.ssl) as r:
            return json.loads(r.read())

    def post(self, path, body):
        req = urllib.request.Request(self.url + path, data=json.dumps(body).encode(), method="POST",
                                     headers={"content-type": "application/json", **self.headers})
        with urllib.request.urlopen(req, timeout=40, context=self.ssl) as r:
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
        cls.c.cli("deploy", os.path.join(ROOT, "examples", "fulfilment.orbit"))

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

    def test_fan_out_and_child_workflow(self):
        fulfil = os.path.join(ROOT, "examples", "fulfilment.orbit")
        items = [{"sku": f"SKU-{i}", "quantity": 1} for i in range(6)]
        r = self.c.cli("run", fulfil, "--input", json.dumps({"order_id": "F-1", "items": items}))
        out = json.loads(r.stdout)
        self.assertEqual(out["tracking"], "UPS-F-1")
        self.assertEqual(out["reservations"], [{"reservation_id": f"res-F-1-SKU-{i}"} for i in range(6)])
        ex = [l for l in r.stderr.splitlines() if "execution" in l][0].split()[-1]
        child = self.c.get(f"/v1/executions/{ex}.ship")
        self.assertEqual((child["workflow"], child["status"]), ("Shipping", "COMPLETED"))
        self.assertEqual(child["parent"], {"execution_id": ex, "step": "ship"})

    def test_child_failure_compensates_every_element(self):
        items = [{"sku": f"SKU-{i}", "quantity": 1} for i in range(3)]
        ex = self.c.post("/v1/executions", {"workflow": "Fulfilment", "input": {"order_id": "noship-1", "items": items}})["execution_id"]
        s = self.c.wait(ex)
        self.assertEqual(s["status"], "FAILED")
        self.assertEqual(s["error"]["code"], "NO_CARRIER")
        self.assertEqual(s["compensation"], "completed")
        released = sorted(k for k, u in s["units"].items() if k.startswith("compensate:") and u["status"] == "SUCCEEDED")
        self.assertEqual(released, [f"compensate:reserve[{i}]" for i in range(3)])

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


class Polyglot(unittest.TestCase):
    """One workflow across Python, TypeScript, Go and Rust workers."""

    @classmethod
    def setUpClass(cls):
        if not shutil.which("go"):
            raise unittest.SkipTest("go toolchain not installed")
        cls.c = Cluster()
        go_bin = os.path.join(cls.c.dir, "text-go")
        subprocess.run(["go", "build", "-o", go_bin, "."], cwd=os.path.join(ROOT, "examples", "workers", "polyglot", "go"), check=True)
        subprocess.run(["cargo", "build", "-q", "-p", "orbit-worker", "--example", "text_worker"], cwd=ROOT, check=True)
        cls.c.start_server()
        poly = os.path.join(ROOT, "examples", "workers", "polyglot")
        cls.c.spawn("python", [sys.executable, os.path.join(poly, "text_worker.py")])
        cls.c.spawn("typescript", ["node", os.path.join(poly, "text_worker.ts")])
        cls.c.spawn("go", [go_bin])
        cls.c.spawn("rust", [os.path.join(ROOT, "target", "debug", "examples", "text_worker")])
        cls.c.wait_online(4)

    @classmethod
    def tearDownClass(cls):
        cls.c.stop()

    def test_four_languages(self):
        text = "The quick brown fox jumps over the lazy dog. The dog sleeps!"
        r = self.c.cli("run", os.path.join(ROOT, "examples", "polyglot.orbit"), "--input", json.dumps({"text": text}))
        out = json.loads(r.stdout)
        self.assertEqual(out["words"], 12)
        self.assertEqual(out["unique"], 9)
        self.assertEqual(out["top_word"], "the")
        # Ties are broken alphabetically: brown is the first of the words seen once.
        self.assertEqual(out["top"], [{"word": "the", "count": 3}, {"word": "dog", "count": 2}, {"word": "brown", "count": 1}])
        normalized = "the quick brown fox jumps over the lazy dog the dog sleeps"
        h = 0xcbf29ce484222325
        for b in normalized.encode():
            h = ((h ^ b) * 0x100000001b3) & 0xFFFFFFFFFFFFFFFF
        self.assertEqual(out["fingerprint"], f"{h:016x}")
        ex_id = [l for l in r.stderr.splitlines() if "execution" in l][0].split()[-1]
        units = self.c.get(f"/v1/executions/{ex_id}")["units"]
        self.assertEqual(
            {k: u["worker_id"] for k, u in units.items()},
            {"normalize": "text-python", "tokenize": "text-typescript", "frequencies": "text-go", "fingerprint": "text-rust"},
        )


class Security(unittest.TestCase):
    """HTTPS with a private CA, an admin token and a restricted worker token."""

    @classmethod
    def setUpClass(cls):
        if not shutil.which("openssl"):
            raise unittest.SkipTest("openssl not installed")
        d = tempfile.mkdtemp(prefix="orbit-certs-")
        run = lambda *a: subprocess.run(a, cwd=d, check=True, capture_output=True)
        run("openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", "ca-key.pem", "-out", "ca.pem", "-days", "2",
            "-subj", "/CN=ORBIT test CA", "-addext", "basicConstraints=critical,CA:TRUE", "-addext", "keyUsage=critical,keyCertSign,cRLSign")
        run("openssl", "req", "-newkey", "rsa:2048", "-nodes", "-keyout", "key.pem", "-out", "srv.csr", "-subj", "/CN=localhost")
        with open(os.path.join(d, "ext.cnf"), "w") as f:
            f.write("subjectAltName=DNS:localhost,IP:127.0.0.1\nextendedKeyUsage=serverAuth\nkeyUsage=critical,digitalSignature,keyEncipherment\n")
        run("openssl", "x509", "-req", "-in", "srv.csr", "-CA", "ca.pem", "-CAkey", "ca-key.pem", "-CAcreateserial",
            "-out", "cert.pem", "-days", "2", "-extfile", "ext.cnf")
        ca = os.path.join(d, "ca.pem")
        cls.c = Cluster(
            server_args=["--token", "admin-secret", "--worker-token", "worker-secret",
                         "--tls-cert", os.path.join(d, "cert.pem"), "--tls-key", os.path.join(d, "key.pem")],
            tls_ca=ca, admin_token="admin-secret",
        )
        cls.c.start_server()
        # Workers get only the worker credential.
        worker_env = {"ORBIT_TOKEN": "", "ORBIT_WORKER_TOKEN": "worker-secret"}
        cls.c.start_workers(**worker_env)
        cls.c.cli("deploy", WORKFLOW)

    @classmethod
    def tearDownClass(cls):
        cls.c.stop()

    def test_workflow_over_tls_with_worker_credentials(self):
        r = self.c.cli("run", WORKFLOW, "--input", json.dumps(order("S-1")))
        self.assertEqual(json.loads(r.stdout)["status"], "completed")

    def test_credentials_are_enforced(self):
        with self.assertRaises(urllib.error.HTTPError) as e:
            self.c.get("/v1/executions", headers={})
        self.assertEqual(e.exception.code, 401)
        with self.assertRaises(urllib.error.HTTPError) as e:
            self.c.get("/v1/executions", headers={"authorization": "Bearer worker-secret"})
        self.assertEqual(e.exception.code, 403)
        r = self.c.cli("ps", "--token", "worker-secret", check=False)
        self.assertNotEqual(r.returncode, 0)
        self.assertIn("FORBIDDEN", r.stderr)
        self.assertEqual(self.c.get("/v1/health", headers={})["status"], "ok")

    def test_tls_is_required(self):
        plain = self.c.url.replace("https://", "http://")
        with self.assertRaises(Exception):
            urllib.request.urlopen(plain + "/v1/health", timeout=3).read()
        r = subprocess.run([ORBIT, "ps", "--server", self.c.url, "--token", "admin-secret"], capture_output=True, text=True,
                           env={k: v for k, v in self.c.env.items() if k != "ORBIT_CA_CERT"})
        self.assertNotEqual(r.returncode, 0, "an untrusted certificate must be rejected")


if __name__ == "__main__":
    unittest.main(verbosity=2)
