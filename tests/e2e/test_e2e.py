"""End-to-end tests: real `orbit` server, real Python and TypeScript workers.

Covers RFC-0001 §15.5 acceptance criteria that need separate processes:
polyglot execution, conditions, compensation after a failure, retries,
inspection through the CLI, and crash recovery (the runtime is killed with
SIGKILL while a task is in flight and restarted), and failover between two
servers in cluster mode (with ORBIT_E2E_POSTGRES_URL).

    cargo build && python3 -I tests/e2e/test_e2e.py
"""

import calendar
import http.client
import json
import os
import re
import signal
import shutil
import socket
import ssl
import subprocess
import sys
import tempfile
import threading
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
        # With ORBIT_E2E_POSTGRES_URL (an admin URL such as
        # postgres://orbit@127.0.0.1:5432/postgres) every cluster runs on its own
        # freshly created PostgreSQL database instead of SQLite.
        pg = os.environ.get("ORBIT_E2E_POSTGRES_URL")
        if pg:
            name = "orbit_e2e_" + os.path.basename(self.dir).replace("-", "_").lower()
            subprocess.run(["psql", pg, "-qc", f"CREATE DATABASE {name}"], check=True)
            self.db = pg.rsplit("/", 1)[0] + "/" + name
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

    def spawn(self, name, argv, cwd=None, **extra_env):
        self.workers.append(subprocess.Popen(argv, cwd=cwd, stdout=self.log(name), stderr=subprocess.STDOUT, env={**self.env, **extra_env}))

    def wait_named(self, worker_id, timeout=60):
        deadline = time.time() + timeout
        while time.time() < deadline:
            if any(w["worker_id"] == worker_id and w["online"] for w in self.get("/v1/workers")["workers"]):
                return
            time.sleep(0.5)
        raise RuntimeError(f"worker {worker_id} did not register; logs in {self.dir}")

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

    def test_signals_queries_and_compute_steps(self):
        approval = os.path.join(ROOT, "examples", "approval.orbit")
        self.c.cli("deploy", approval)
        start = lambda oid, amount: self.c.post("/v1/executions", {"workflow": "OrderApproval", "input": {"order_id": oid, "customer_id": "c-1", "amount": amount}})["execution_id"]
        # Small orders need no approval: the await step is skipped.
        s = self.c.wait(start("S-1", 50.0))
        self.assertEqual(s["status"], "COMPLETED")
        self.assertEqual(s["units"]["decision"]["status"], "SKIPPED")
        self.assertEqual(s["output"], {"status": "charged", "transaction_id": "tx-S-1"})
        # Large orders wait for a typed signal; the query shows where they stand.
        big = start("S-2", 2500.0)
        deadline = time.time() + 10
        while self.c.get(f"/v1/executions/{big}")["units"].get("decision", {}).get("status") != "WAITING":
            self.assertLess(time.time(), deadline, "await step did not start")
            time.sleep(0.1)
        q = json.loads(self.c.cli("query", big, "status").stdout)
        self.assertEqual(q, {"needs_approval": True, "approved": None, "reviewer": None, "charged": False})
        bad = self.c.cli("signal", big, "review", "--data", '{"approved": "yes"}', check=False)
        self.assertNotEqual(bad.returncode, 0)
        self.assertIn("SIGNAL_PAYLOAD_INVALID", bad.stderr)
        sys.path.insert(0, os.path.join(ROOT, "sdks", "python"))
        from orbit_sdk import Client
        Client(self.c.url).signal(big, "review", {"approved": True, "reviewer": "dana"})
        s = self.c.wait(big)
        self.assertEqual(s["status"], "COMPLETED")
        self.assertEqual(s["output"]["transaction_id"], "tx-S-2")
        self.assertEqual(Client(self.c.url).query(big, "status")["reviewer"], "dana")
        self.assertIn("SignalDelivered", self.c.cli("logs", big).stdout)

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


def make_pki(clients=()):
    """A throwaway CA, a server certificate for 127.0.0.1, and one client
    certificate per name (CN and DNS SAN), in a new temp directory."""
    d = tempfile.mkdtemp(prefix="orbit-certs-")
    run = lambda *a: subprocess.run(a, cwd=d, check=True, capture_output=True)
    run("openssl", "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-keyout", "ca-key.pem", "-out", "ca.pem", "-days", "2",
        "-subj", "/CN=ORBIT test CA", "-addext", "basicConstraints=critical,CA:TRUE", "-addext", "keyUsage=critical,keyCertSign,cRLSign")

    def issue(name, ext, out):
        run("openssl", "req", "-newkey", "rsa:2048", "-nodes", "-keyout", f"{out}-key.pem", "-out", f"{out}.csr", "-subj", f"/CN={name}")
        with open(os.path.join(d, f"{out}.cnf"), "w") as f:
            f.write(ext)
        run("openssl", "x509", "-req", "-in", f"{out}.csr", "-CA", "ca.pem", "-CAkey", "ca-key.pem", "-CAcreateserial",
            "-out", f"{out}.pem", "-days", "2", "-extfile", f"{out}.cnf")

    issue("localhost", "subjectAltName=DNS:localhost,IP:127.0.0.1\nextendedKeyUsage=serverAuth\nkeyUsage=critical,digitalSignature,keyEncipherment\n", "server")
    os.rename(os.path.join(d, "server.pem"), os.path.join(d, "cert.pem"))
    os.rename(os.path.join(d, "server-key.pem"), os.path.join(d, "key.pem"))
    for name in clients:
        issue(name, f"subjectAltName=DNS:{name}\nextendedKeyUsage=clientAuth\nkeyUsage=critical,digitalSignature,keyEncipherment\n", name)
    return d


class Security(unittest.TestCase):
    """HTTPS with a private CA, an admin token and a restricted worker token."""

    @classmethod
    def setUpClass(cls):
        if not shutil.which("openssl"):
            raise unittest.SkipTest("openssl not installed")
        d = make_pki()
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


class ClientsAndLimits(unittest.TestCase):
    """Client identities limited to some workflows and actions, and server limits."""

    @classmethod
    def setUpClass(cls):
        import hashlib
        d = tempfile.mkdtemp(prefix="orbit-clients-")
        h = hashlib.sha256(b"billing-token").hexdigest()
        workers = os.path.join(d, "workers.toml")
        with open(workers, "w") as f:
            f.write(f'[[client]]\nid = "billing"\ntoken_sha256 = "{h}"\nworkflows = ["OrderApproval"]\nactions = ["start", "signal", "query", "read"]\n')
            s = hashlib.sha256(b"scheduler-token").hexdigest()
            f.write(f'\n[[client]]\nid = "scheduler"\ntoken_sha256 = "{s}"\nworkflows = ["OrderApproval"]\nactions = ["schedule"]\n')
        cls.c = Cluster(server_args=["--token", "admin-secret", "--workers-file", workers,
                                     "--max-active-executions", "2", "--max-payload-bytes", "4096"],
                        admin_token="admin-secret")
        cls.c.start_server()
        cls.c.cli("deploy", os.path.join(ROOT, "examples", "approval.orbit"))
        cls.c.cli("deploy", WORKFLOW)
        cls.client = {"authorization": "Bearer billing-token"}

    @classmethod
    def tearDownClass(cls):
        cls.c.stop()

    def as_client(self, method, path, body=None):
        req = urllib.request.Request(self.c.url + path, method=method,
                                     data=None if body is None else json.dumps(body).encode(),
                                     headers={"content-type": "application/json", **self.client})
        with urllib.request.urlopen(req, timeout=10) as r:
            return json.loads(r.read())

    def expect_status(self, code, *args):
        with self.assertRaises(urllib.error.HTTPError) as e:
            self.as_client(*args)
        self.assertEqual(e.exception.code, code, e.exception.read().decode())

    def test_clients_are_limited_to_their_workflows_and_actions(self):
        input = {"order_id": "C-1", "customer_id": "c", "amount": 5000.0}
        ex = self.as_client("POST", "/v1/executions", {"workflow": "OrderApproval", "input": input})["execution_id"]
        self.assertEqual(self.as_client("GET", f"/v1/executions/{ex}/queries/status")["value"]["needs_approval"], True)
        self.as_client("POST", f"/v1/executions/{ex}/signals/review", {"approved": False, "reviewer": "kim"})
        # Another workflow, an action not granted, admin and worker endpoints: all refused.
        self.expect_status(403, "POST", "/v1/executions", {"workflow": "OrderProcessing", "input": order("C-2")})
        self.expect_status(403, "POST", f"/v1/executions/{ex}/cancel", {})
        self.expect_status(403, "GET", "/v1/workers")
        self.expect_status(403, "POST", "/v1/workers/register", {"worker_id": "x", "runtime": "python", "contracts": []})
        listed = {e["workflow"] for e in self.as_client("GET", "/v1/executions")["executions"]}
        self.assertEqual(listed, {"OrderApproval"})

    def test_schedules_need_the_schedule_action(self):
        spec = {"schedule_id": "approvals", "workflow": "OrderApproval", "cron": "0 6 * * *", "paused": True,
                "input": {"order_id": "SCH-1", "customer_id": "c", "amount": 10.0}}
        # `billing` may start executions but not manage schedules.
        self.expect_status(403, "POST", "/v1/schedules", spec)
        self.client = {"authorization": "Bearer scheduler-token"}
        try:
            self.assertEqual(self.as_client("POST", "/v1/schedules", spec)["paused"], True)
            self.expect_status(403, "POST", "/v1/schedules", {**spec, "schedule_id": "other", "workflow": "OrderProcessing"})
            self.expect_status(403, "PATCH", "/v1/schedules/approvals", {"workflow": "OrderProcessing"})
            self.c.post("/v1/schedules", {"schedule_id": "admin-only", "workflow": "OrderProcessing", "cron": "0 6 * * *",
                                          "paused": True, "input": order("SCH-2")})
            self.expect_status(403, "GET", "/v1/schedules/admin-only")
            listed = [s["schedule_id"] for s in self.as_client("GET", "/v1/schedules")["schedules"]]
            self.assertEqual(listed, ["approvals"])
            self.assertEqual(self.as_client("DELETE", "/v1/schedules/approvals")["deleted"], True)
            # Schedule access is not read access to executions.
            self.assertEqual(self.as_client("GET", "/v1/executions")["executions"], [])
        finally:
            self.client = {"authorization": "Bearer billing-token"}

    def test_limits(self):
        self.expect_status(413, "POST", "/v1/executions", {"workflow": "OrderApproval",
                           "input": {"order_id": "x" * 5000, "customer_id": "c", "amount": 1.0}})
        # Large orders wait for approval, so they stay active: the third is refused.
        started = []
        for i in range(3):
            try:
                started.append(self.c.post("/v1/executions", {"workflow": "OrderApproval",
                               "input": {"order_id": f"L-{i}", "customer_id": "c", "amount": 2000.0}})["execution_id"])
            except urllib.error.HTTPError as e:
                self.assertEqual(e.code, 429)
                self.assertIn("RESOURCE_EXHAUSTED", e.read().decode())
                break
        else:
            self.fail("the active-execution limit was not enforced")
        for ex in started:
            self.c.post(f"/v1/executions/{ex}/cancel", {})


class Identities(unittest.TestCase):
    """Per-worker identities: mutual TLS for real workers, a token identity for
    a rogue worker whose every overreach must be refused."""

    @classmethod
    def setUpClass(cls):
        if not shutil.which("openssl"):
            raise unittest.SkipTest("openssl not installed")
        d = make_pki(clients=["orders-python", "orders-typescript", "stranger", "text-go", "text-rust", "hello-java", "hello-csharp"])
        cls.pki = d
        rogue_hash = subprocess.run([sys.executable, "-c", "import hashlib;print(hashlib.sha256(b'rogue-token').hexdigest())"],
                                    capture_output=True, text=True).stdout.strip()
        workers = os.path.join(d, "workers.toml")
        with open(workers, "w") as f:
            f.write(f"""
[[worker]]
id = "orders-python"
client_cert = "orders-python"
runtime = "python"
tasks = ["risk.*", "payments.*", "shipping.*"]

[[worker]]
id = "orders-typescript"
client_cert = "orders-typescript"
tasks = ["orders.*", "inventory.*", "notifications.*"]

[[worker]]
id = "text-go"
client_cert = "text-go"
tasks = ["text.*"]

[[worker]]
id = "text-rust"
client_cert = "text-rust"
tasks = ["text.*"]

[[worker]]
id = "hello-java"
client_cert = "hello-java"
runtime = "java"
tasks = ["greeting.java"]

[[worker]]
id = "hello-csharp"
client_cert = "hello-csharp"
runtime = "csharp"
tasks = ["greeting.csharp"]

[[worker]]
id = "rogue"
token_sha256 = "{rogue_hash}"
tasks = ["risk.analyze"]
""")
        cls.c = Cluster(
            server_args=["--token", "admin-secret", "--workers-file", workers, "--tls-cert", os.path.join(d, "cert.pem"),
                         "--tls-key", os.path.join(d, "key.pem"), "--tls-client-ca", os.path.join(d, "ca.pem")],
            tls_ca=os.path.join(d, "ca.pem"), admin_token="admin-secret",
        )
        cls.c.start_server()
        for name, argv in [
            ("orders-python", [sys.executable, os.path.join(ROOT, "examples", "workers", "python", "order_workers.py")]),
            ("orders-typescript", ["node", os.path.join(ROOT, "examples", "workers", "typescript", "order_workers.ts")]),
        ]:
            # No token at all: the certificate is the credential.
            cls.c.spawn(name, argv, ORBIT_TOKEN="", ORBIT_WORKER_ID=name,
                        ORBIT_CLIENT_CERT=os.path.join(d, f"{name}.pem"), ORBIT_CLIENT_KEY=os.path.join(d, f"{name}-key.pem"))
        cls.c.wait_online(2)
        cls.c.cli("deploy", WORKFLOW)

    @classmethod
    def tearDownClass(cls):
        cls.c.stop()

    def rogue(self, path, body, cert=None):
        ctx = ssl.create_default_context(cafile=os.path.join(self.pki, "ca.pem"))
        headers = {"content-type": "application/json"}
        if cert:
            ctx.load_cert_chain(os.path.join(self.pki, f"{cert}.pem"), os.path.join(self.pki, f"{cert}-key.pem"))
        else:
            headers["authorization"] = "Bearer rogue-token"
        req = urllib.request.Request(self.c.url + path, data=json.dumps(body).encode(), method="POST", headers=headers)
        try:
            with urllib.request.urlopen(req, timeout=20, context=ctx) as r:
                return r.status, json.loads(r.read() or b"null")
        except urllib.error.HTTPError as e:
            return e.code, json.loads(e.read() or b"null")

    def test_workers_authenticated_by_certificate_run_the_workflow(self):
        r = self.c.cli("run", WORKFLOW, "--input", json.dumps(order("I-1")))
        self.assertEqual(json.loads(r.stdout)["status"], "completed")
        online = {w["worker_id"] for w in self.c.get("/v1/workers")["workers"] if w["online"]}
        self.assertTrue({"orders-python", "orders-typescript"} <= online, online)

    def test_go_and_rust_sdks_authenticate_with_certificates(self):
        if not shutil.which("go"):
            self.skipTest("go toolchain not installed")
        go_bin = os.path.join(self.c.dir, "text-go")
        subprocess.run(["go", "build", "-o", go_bin, "."], cwd=os.path.join(ROOT, "examples", "workers", "polyglot", "go"), check=True)
        subprocess.run(["cargo", "build", "-q", "-p", "orbit-worker", "--example", "text_worker"], cwd=ROOT, check=True)
        for name, argv in [("text-go", [go_bin]), ("text-rust", [os.path.join(ROOT, "target", "debug", "examples", "text_worker")])]:
            self.c.spawn(name, argv, ORBIT_TOKEN="", ORBIT_CLIENT_CERT=os.path.join(self.pki, f"{name}.pem"),
                         ORBIT_CLIENT_KEY=os.path.join(self.pki, f"{name}-key.pem"))
        deadline = time.time() + 30
        while time.time() < deadline:
            online = {w["worker_id"] for w in self.c.get("/v1/workers")["workers"] if w["online"]}
            if {"text-go", "text-rust"} <= online:
                return
            time.sleep(0.2)
        self.fail(f"Go/Rust workers did not register over mTLS; logs in {self.c.dir}")

    def certs(self, name):
        return {"ORBIT_TOKEN": "", "ORBIT_CLIENT_CERT": os.path.join(self.pki, f"{name}.pem"),
                "ORBIT_CLIENT_KEY": os.path.join(self.pki, f"{name}-key.pem")}

    def test_java_sdk_over_mutual_tls(self):
        mvn = shutil.which("mvn") or ("/opt/maven/bin/mvn" if os.path.exists("/opt/maven/bin/mvn") else None)
        if not mvn:
            self.skipTest("maven not installed")
        subprocess.run([mvn, "-B", "-q", "install", "-DskipTests", "-Dgpg.skip=true"], cwd=os.path.join(ROOT, "sdks", "java"), check=True)
        self.c.spawn("java", [mvn, "-B", "-q", "compile", "exec:java"], cwd=os.path.join(ROOT, "examples", "workers", "java"),
                     **self.certs("hello-java"))
        self.c.wait_named("hello-java", timeout=180)
        r = self.c.cli("run", os.path.join(ROOT, "examples", "java_hello.orbit"), "--input", json.dumps({"name": "Ada", "excited": True}))
        self.assertEqual(json.loads(r.stdout)["message"], "Hello, Ada, from Java!")

    def test_csharp_sdk_over_mutual_tls(self):
        if not shutil.which("dotnet"):
            self.skipTest(".NET SDK not installed")
        proj = os.path.join(ROOT, "examples", "workers", "csharp")
        subprocess.run(["dotnet", "build", "-v", "q", "-nologo"], cwd=proj, check=True, capture_output=True)
        self.c.spawn("csharp", ["dotnet", "run", "--no-build"], cwd=proj, **self.certs("hello-csharp"))
        self.c.wait_named("hello-csharp", timeout=120)
        r = self.c.cli("run", os.path.join(ROOT, "examples", "csharp_hello.orbit"), "--input", json.dumps({"name": "Ada", "excitement": 2}))
        self.assertEqual(json.loads(r.stdout)["message"], "Hello, Ada!!")

    def test_identities_cannot_overreach(self):
        contract = lambda t: [{"task_name": t, "contract_version": "1.0.0"}]
        # Impersonating another worker.
        status, body = self.rogue("/v1/workers/register", {"worker_id": "orders-python", "runtime": "python", "contracts": contract("risk.analyze")})
        self.assertEqual((status, body["error"]["code"]), (403, "FORBIDDEN"))
        # Announcing a task it is not allowed to serve.
        status, body = self.rogue("/v1/workers/register", {"worker_id": "rogue", "runtime": "go", "contracts": contract("payments.charge")})
        self.assertEqual(status, 403)
        self.assertIn("may not serve: payments.charge", body["error"]["message"])
        # Admin endpoints.
        req = urllib.request.Request(self.c.url + "/v1/executions", headers={"authorization": "Bearer rogue-token"})
        with self.assertRaises(urllib.error.HTTPError) as e:
            urllib.request.urlopen(req, timeout=10, context=self.c.ssl)
        self.assertEqual(e.exception.code, 403)
        # Completing a task token issued to another worker: amount 4242 keeps the
        # Python worker busy with the payment for 3 seconds.
        ex = self.c.post("/v1/executions", {"workflow": "OrderProcessing", "input": order("I-2", amount=4242)})["execution_id"]
        deadline = time.time() + 20
        while True:
            unit = self.c.get(f"/v1/executions/{ex}")["units"].get("process_payment", {})
            if unit.get("status") == "DISPATCHED":
                break
            self.assertLess(time.time(), deadline)
            time.sleep(0.05)
        status, body = self.rogue("/v1/tasks/complete", {"token": unit["token"], "output": {"transaction_id": "forged", "status": "x"}})
        self.assertEqual(status, 403)
        self.assertIn("issued to worker `orders-python`", body["error"]["message"])
        s = self.c.wait(ex)
        self.assertEqual(s["output"]["transaction_id"], "tx-I-2", "the forged result was not recorded")
        # A certificate from the trusted CA that is not in the workers file.
        status, body = self.rogue("/v1/workers/register", {"worker_id": "stranger", "runtime": "go", "contracts": contract("risk.analyze")}, cert="stranger")
        self.assertEqual(status, 403)
        self.assertIn("matches no worker", body["error"]["message"])
        # Allowed registration still works for the rogue's own identity.
        status, body = self.rogue("/v1/workers/register", {"worker_id": "rogue", "runtime": "go", "contracts": contract("risk.analyze")})
        self.assertEqual(status, 200, body)


class ReadyProxy:
    """A minimal TCP load balancer: every new connection goes to the backend
    whose /v1/ready answers 200, probed every 200ms. When the ready backend
    changes, connections to the previous one are closed (like HAProxy's
    `on-marked-down shutdown-sessions`)."""

    def __init__(self, backends):
        self.backends = backends
        self.active = None
        self.conns = {b: set() for b in backends}
        self.lock = threading.Lock()
        self.stopped = threading.Event()
        self.sock = socket.socket()
        self.sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.sock.bind(("127.0.0.1", 0))
        self.sock.listen(128)
        self.port = self.sock.getsockname()[1]
        threading.Thread(target=self._probe, daemon=True).start()
        threading.Thread(target=self._accept, daemon=True).start()

    def _ready(self, backend):
        try:
            with urllib.request.urlopen(f"http://{backend[0]}:{backend[1]}/v1/ready", timeout=0.5) as r:
                return r.status == 200
        except OSError:  # 503 (HTTPError), refused, timeout
            return False

    def _probe(self):
        while not self.stopped.is_set():
            ready = next((b for b in self.backends if self._ready(b)), None)
            with self.lock:
                previous, self.active = self.active, ready
                stale = list(self.conns[previous]) if previous and previous != ready else []
            for s in stale:
                self._close(s)
            time.sleep(0.2)

    @staticmethod
    def _close(s):
        try:
            s.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        s.close()

    def _accept(self):
        while not self.stopped.is_set():
            try:
                conn, _ = self.sock.accept()
            except OSError:
                return
            threading.Thread(target=self._serve, args=(conn,), daemon=True).start()

    def _serve(self, conn):
        with self.lock:
            backend = self.active
        if backend is None:
            return self._close(conn)
        try:
            up = socket.create_connection(backend, timeout=2)
        except OSError:
            return self._close(conn)
        up.settimeout(None)
        with self.lock:
            self.conns[backend].update((conn, up))

        def pipe(src, dst):
            try:
                while True:
                    data = src.recv(65536)
                    if not data:
                        break
                    dst.sendall(data)
            except OSError:
                pass
            finally:
                self._close(src)
                self._close(dst)
                with self.lock:
                    self.conns[backend].discard(src)
                    self.conns[backend].discard(dst)

        threading.Thread(target=pipe, args=(up, conn), daemon=True).start()
        pipe(conn, up)

    def stop(self):
        self.stopped.set()
        self.sock.close()


@unittest.skipUnless(os.environ.get("ORBIT_E2E_POSTGRES_URL"), "set ORBIT_E2E_POSTGRES_URL to run the multi-server tests")
class HighAvailability(unittest.TestCase):
    """Two `orbit server --cluster` nodes on one PostgreSQL database behind a
    readiness-checking proxy; the leader is killed with SIGKILL mid-execution."""

    @classmethod
    def setUpClass(cls):
        cls.c = Cluster()
        cls.ports = {"node-1": free_port(), "node-2": free_port()}
        cls.nodes = {}
        cls.start_node("node-1")
        cls.wait_leader()
        cls.start_node("node-2")
        cls.proxy = ReadyProxy([("127.0.0.1", p) for p in cls.ports.values()])
        cls.c.url = f"http://127.0.0.1:{cls.proxy.port}"
        cls.c.env["ORBIT_SERVER"] = cls.c.url
        cls.c.start_workers()
        cls.c.cli("deploy", WORKFLOW)

    @classmethod
    def tearDownClass(cls):
        cls.c.stop()
        for p in cls.nodes.values():
            if p.poll() is None:
                p.terminate()
                p.wait(timeout=10)
        cls.proxy.stop()
        print(f"\nlogs: {cls.c.dir}")

    @classmethod
    def node_url(cls, name):
        return f"http://127.0.0.1:{cls.ports[name]}"

    @classmethod
    def start_node(cls, name):
        cls.nodes[name] = subprocess.Popen(
            [ORBIT, "server", "--cluster", "--db", cls.c.db, "--addr", f"127.0.0.1:{cls.ports[name]}",
             "--node-id", name, "--session-timeout", "3"],
            stdout=cls.c.log(name), stderr=subprocess.STDOUT, env=cls.c.env,
        )
        deadline = time.time() + 15
        while time.time() < deadline:
            try:
                urllib.request.urlopen(cls.node_url(name) + "/v1/health", timeout=1).read()
                return
            except OSError:
                time.sleep(0.1)
        raise RuntimeError(f"{name} did not start; logs in {cls.c.dir}")

    @classmethod
    def health(cls, name):
        with urllib.request.urlopen(cls.node_url(name) + "/v1/health", timeout=2) as r:
            return json.loads(r.read())

    @classmethod
    def ready(cls, name):
        try:
            with urllib.request.urlopen(cls.node_url(name) + "/v1/ready", timeout=1) as r:
                return r.status == 200
        except OSError:
            return False

    @classmethod
    def wait_leader(cls, timeout=15):
        deadline = time.time() + timeout
        while time.time() < deadline:
            ready = [n for n in cls.nodes if cls.nodes[n].poll() is None and cls.ready(n)]
            assert len(ready) < 2, "two leaders"
            if ready:
                return ready[0]
            time.sleep(0.05)
        raise AssertionError(f"no leader within {timeout}s; logs in {cls.c.dir}")

    def start(self, input, execution_id=None):
        return self.c.post("/v1/executions", {"workflow": "OrderProcessing", "input": input, "execution_id": execution_id})["execution_id"]

    def await_dispatched(self, ex, step="process_payment"):
        deadline = time.time() + 20
        while self.c.get(f"/v1/executions/{ex}")["units"].get(step, {}).get("status") != "DISPATCHED":
            self.assertLess(time.time(), deadline, f"{step} never dispatched")
            time.sleep(0.05)

    def kill_leader(self):
        leader = self.wait_leader()
        self.nodes[leader].send_signal(signal.SIGKILL)
        self.nodes[leader].wait()
        return leader, time.time()

    def wait_failover(self, killed, at):
        """Wait for the new leader, and for the proxy to route to it."""
        leader = self.wait_leader()
        took = time.time() - at
        self.assertNotEqual(leader, killed)
        self.assertLess(took, 5, f"failover took {took:.1f}s")
        deadline = time.time() + 5
        while self.proxy.active != ("127.0.0.1", self.ports[leader]):
            self.assertLess(time.time(), deadline, "the proxy did not switch to the new leader")
            time.sleep(0.05)
        return leader

    def test_a_standby_serves_reads_and_points_at_the_leader(self):
        leader = self.wait_leader()
        standby = next(n for n in self.nodes if n != leader)
        h = self.health(standby)
        self.assertEqual((h["role"], h["node_id"], h["leader"]["node_id"]), ("standby", standby, leader))
        self.assertEqual(self.health(leader)["role"], "leader")
        req = urllib.request.Request(self.node_url(standby) + "/v1/executions", method="POST",
                                     data=json.dumps({"workflow": "OrderProcessing", "input": order("HA-0")}).encode(),
                                     headers={"content-type": "application/json"})
        with self.assertRaises(urllib.error.HTTPError) as e:
            urllib.request.urlopen(req, timeout=5)
        self.assertEqual(e.exception.code, 503)
        self.assertEqual(e.exception.headers["x-orbit-leader"], self.node_url(leader))
        self.assertEqual(json.loads(e.exception.read())["error"]["code"], "NOT_LEADER")
        # The CLI follows NOT_LEADER to the leader; reads are served by the standby.
        r = subprocess.run([ORBIT, "run", WORKFLOW, "--input", json.dumps(order("HA-1")), "--server", self.node_url(standby)],
                           capture_output=True, text=True, env=self.c.env, timeout=120)
        self.assertEqual(r.returncode, 0, r.stderr)
        self.assertEqual(json.loads(r.stdout)["transaction_id"], "tx-HA-1")
        ps = subprocess.run([ORBIT, "ps", "--server", self.node_url(standby)], capture_output=True, text=True, env=self.c.env, timeout=30)
        self.assertIn("COMPLETED", ps.stdout)

    def test_b_failover_mid_execution(self):
        term = self.health(self.wait_leader())["term"]
        quick = [self.start(order(f"HA-B{i}")) for i in range(3)]
        slow = self.start(order("HA-B9", amount=4242))  # the charge takes 3s
        self.await_dispatched(slow)
        killed, at = self.kill_leader()
        leader = self.wait_failover(killed, at)
        self.assertEqual(self.health(leader)["term"], term + 1)
        for ex in quick + [slow]:
            s = self.c.wait(ex, timeout=60)
            self.assertEqual(s["status"], "COMPLETED", json.dumps(s, indent=2))
        self.assertEqual(self.c.get(f"/v1/executions/{slow}")["output"]["transaction_id"], "tx-HA-B9")
        history = self.c.get(f"/v1/executions/{slow}/history")["events"]
        charges = [e for e in history if e["type"] == "StepSucceeded" and e["step"] == "process_payment"]
        self.assertEqual(len(charges), 1, "exactly one recorded success for the charge")
        # The killed node rejoins as a standby.
        self.start_node(killed)
        deadline = time.time() + 10
        while self.health(killed)["role"] != "standby" or self.health(killed)["term"] != term + 1:
            self.assertLess(time.time(), deadline, "restarted node did not rejoin")
            time.sleep(0.2)

    def test_c_failover_redelivers_a_lost_attempt(self):
        # The leader and the worker running the charge die together: the
        # attempt's outcome is unknown and the new leader redelivers it.
        slow = self.start(order("HA-C1", amount=4242))
        self.await_dispatched(slow)
        python = self.c.workers[0]
        python.send_signal(signal.SIGKILL)
        killed, at = self.kill_leader()
        self.wait_failover(killed, at)
        self.c.spawn("python-restarted", [sys.executable, os.path.join(ROOT, "examples", "workers", "python", "order_workers.py")])
        s = self.c.wait(slow, timeout=60)
        self.assertEqual(s["status"], "COMPLETED", json.dumps(s, indent=2))
        self.assertEqual(s["output"]["transaction_id"], "tx-HA-C1")
        history = self.c.get(f"/v1/executions/{slow}/history")["events"]
        of = lambda t: [e for e in history if e["type"] == t and e.get("step") == "process_payment"]
        self.assertEqual(len(of("StepAttemptUncertain")), 1, json.dumps(history, indent=1))
        self.assertEqual(len(of("StepDispatched")), 2, "the charge was redelivered")
        self.assertEqual(len(of("StepSucceeded")), 1, "exactly one recorded success for the charge")


class RoundRobinProxy:
    """A minimal TCP load balancer: every new connection goes to the next
    backend whose /v1/ready answers 200 (probed every 200ms), in turn."""

    def __init__(self, backends):
        self.backends = backends
        self.ready = []
        self.next = 0
        self.lock = threading.Lock()
        self.stopped = threading.Event()
        self.sock = socket.socket()
        self.sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.sock.bind(("127.0.0.1", 0))
        self.sock.listen(256)
        self.port = self.sock.getsockname()[1]
        threading.Thread(target=self._probe, daemon=True).start()
        threading.Thread(target=self._accept, daemon=True).start()

    def _probe(self):
        while not self.stopped.is_set():
            ready = [b for b in self.backends if ReadyProxy._ready(None, b)]
            with self.lock:
                self.ready = ready
            time.sleep(0.2)

    def _accept(self):
        while not self.stopped.is_set():
            try:
                conn, _ = self.sock.accept()
            except OSError:
                return
            threading.Thread(target=self._serve, args=(conn,), daemon=True).start()

    def _serve(self, conn):
        # Like a load balancer's retry on connection errors: a backend that
        # just died (refused) is skipped in favour of the next ready one.
        up = None
        for _ in range(len(self.backends)):
            with self.lock:
                if not self.ready:
                    break
                backend = self.ready[self.next % len(self.ready)]
                self.next += 1
            try:
                up = socket.create_connection(backend, timeout=2)
                break
            except OSError:
                continue
        if up is None:
            return ReadyProxy._close(conn)
        up.settimeout(None)

        def pipe(src, dst):
            try:
                while True:
                    data = src.recv(65536)
                    if not data:
                        break
                    dst.sendall(data)
            except OSError:
                pass
            finally:
                ReadyProxy._close(src)
                ReadyProxy._close(dst)

        threading.Thread(target=pipe, args=(up, conn), daemon=True).start()
        pipe(conn, up)

    def stop(self):
        self.stopped.set()
        self.sock.close()


@unittest.skipUnless(os.environ.get("ORBIT_E2E_POSTGRES_URL"), "set ORBIT_E2E_POSTGRES_URL to run the multi-server tests")
class SharedCluster(unittest.TestCase):
    """Three `orbit server --cluster --cluster-mode shared` nodes on one
    PostgreSQL database behind a round-robin proxy; every node is active. One
    node is killed with SIGKILL mid-run."""

    NODES = ("node-1", "node-2", "node-3")

    @classmethod
    def setUpClass(cls):
        cls.c = Cluster()
        cls.ports = {n: free_port() for n in cls.NODES}
        cls.nodes = {}
        for n in cls.NODES:
            cls.start_node(n)
        cls.proxy = RoundRobinProxy([("127.0.0.1", p) for p in cls.ports.values()])
        deadline = time.time() + 10
        while len(cls.proxy.ready) < len(cls.NODES):
            assert time.time() < deadline, "not every node became ready"
            time.sleep(0.05)
        cls.c.url = f"http://127.0.0.1:{cls.proxy.port}"
        cls.c.env["ORBIT_SERVER"] = cls.c.url
        cls.c.start_workers()
        cls.c.cli("deploy", WORKFLOW)

    @classmethod
    def tearDownClass(cls):
        cls.c.stop()
        for p in cls.nodes.values():
            if p.poll() is None:
                p.terminate()
                p.wait(timeout=10)
        cls.proxy.stop()
        print(f"\nlogs: {cls.c.dir}")

    @classmethod
    def node_url(cls, name):
        return f"http://127.0.0.1:{cls.ports[name]}"

    @classmethod
    def start_node(cls, name):
        cls.nodes[name] = subprocess.Popen(
            [ORBIT, "server", "--cluster", "--cluster-mode", "shared", "--db", cls.c.db,
             "--addr", f"127.0.0.1:{cls.ports[name]}", "--node-id", name,
             "--session-timeout", "3", "--node-timeout", "3"],
            stdout=cls.c.log(name), stderr=subprocess.STDOUT, env=cls.c.env,
        )
        deadline = time.time() + 15
        while time.time() < deadline:
            try:
                urllib.request.urlopen(cls.node_url(name) + "/v1/ready", timeout=1).read()
                return
            except OSError:
                time.sleep(0.1)
        raise RuntimeError(f"{name} did not become ready; logs in {cls.c.dir}")

    @classmethod
    def health(cls, name):
        with urllib.request.urlopen(cls.node_url(name) + "/v1/health", timeout=2) as r:
            return json.loads(r.read())

    @classmethod
    def node_log(cls, name):
        with open(os.path.join(cls.c.dir, f"{name}.log"), errors="replace") as f:
            return f.read()

    def retry(self, call):
        """A request through the proxy, repeated when the node serving it
        dies mid-request (as a client of a load balancer would)."""
        deadline = time.time() + 30
        while True:
            try:
                return call()
            except (OSError, http.client.HTTPException) as e:
                if isinstance(e, urllib.error.HTTPError) or time.time() > deadline:
                    raise
                time.sleep(0.2)

    def get(self, path):
        return self.retry(lambda: self.c.get(path))

    def wait(self, ex, timeout):
        deadline = time.time() + timeout
        while time.time() < deadline:
            s = self.get(f"/v1/executions/{ex}/wait?timeout_ms=5000")
            if s["status"] in ("COMPLETED", "FAILED", "CANCELLED", "TIMED_OUT"):
                return s
        raise AssertionError(f"execution {ex} did not finish: {s['status']}")

    def start(self, input):
        # An explicit execution id makes a repeated start safe.
        ex = "sh-" + input["order_id"]
        body = {"workflow": "OrderProcessing", "input": input, "execution_id": ex}
        return self.retry(lambda: self.c.post("/v1/executions", body))["execution_id"]

    def start_on(self, node, input):
        """Start an execution on one node directly: that node owns it."""
        ex = "sh-" + input["order_id"]
        req = urllib.request.Request(self.node_url(node) + "/v1/executions", method="POST",
                                     data=json.dumps({"workflow": "OrderProcessing", "input": input, "execution_id": ex}).encode(),
                                     headers={"content-type": "application/json"})
        with urllib.request.urlopen(req, timeout=10) as r:
            return json.loads(r.read())["execution_id"]

    def await_dead(self, victim):
        """Wait until the other nodes no longer list `victim` as live."""
        deadline = time.time() + 15
        for n in self.NODES:
            if n == victim:
                continue
            while victim in [x["node_id"] for x in self.health(n)["nodes"] if x["live"]]:
                self.assertLess(time.time(), deadline, f"{n} still lists {victim} as live")
                time.sleep(0.1)

    def started_by(self):
        """Executions each node started (and therefore owned first), from its log."""
        return {n: set(re.findall(r"execution started execution_id=(\S+)", self.node_log(n))) for n in self.NODES}

    def payment(self, ex):
        history = self.get(f"/v1/executions/{ex}/history")["events"]
        return {t: [e for e in history if e["type"] == t and e.get("step") == "process_payment"]
                for t in ("StepDispatched", "StepAttemptUncertain", "StepSucceeded")}

    def await_dispatched(self, executions, step="process_payment"):
        deadline = time.time() + 30
        while not all(self.get(f"/v1/executions/{ex}")["units"].get(step, {}).get("status") == "DISPATCHED" for ex in executions):
            self.assertLess(time.time(), deadline, f"{step} never dispatched")
            time.sleep(0.05)

    def test_a_work_is_spread_over_every_node(self):
        claimed = {n: self.health(n)["tasks_claimed"] for n in self.NODES}
        executions = [self.start(order(f"SH-A{i}", customer=f"cust-{i}")) for i in range(20)]
        for ex in executions:
            s = self.wait(ex, timeout=60)
            self.assertEqual(s["status"], "COMPLETED", json.dumps(s, indent=2))
            self.assertEqual(len(self.payment(ex)["StepSucceeded"]), 1)
        owners = self.started_by()
        spread = [n for n in self.NODES if owners[n] & set(executions)]
        self.assertGreater(len(spread), 1, f"executions started on one node only: {owners}")
        dispatched = {n: self.health(n)["tasks_claimed"] - claimed[n] for n in self.NODES}
        self.assertGreater(len([n for n in dispatched if dispatched[n] > 0]), 1, f"tasks dispatched by one node only: {dispatched}")
        # Every node reports the others as live and is ready.
        for n in self.NODES:
            h = self.health(n)
            self.assertEqual((h["role"], h["node_id"]), ("shared", n))
            self.assertEqual(sorted(x["node_id"] for x in h["nodes"] if x["live"]), sorted(self.NODES))
        print(f"\nstarted per node: { {n: len(owners[n] & set(executions)) for n in self.NODES} }, tasks claimed per node: {dispatched}")

    def test_b_kill_a_node_mid_run(self):
        victim = "node-2"
        quick = [self.start(order(f"SH-B{i}", customer=f"cust-{i}")) for i in range(16)]
        # Charges of 4242 take 3s; two of them are owned by the victim.
        slow = [self.start_on(victim, order(f"SH-B9{i}", amount=4242)) for i in range(2)]
        slow += [self.start(order(f"SH-B9{i}", amount=4242)) for i in range(2, 4)]
        self.await_dispatched(slow)
        self.assertGreaterEqual(self.health(victim)["owned_executions"], 2)
        self.nodes[victim].send_signal(signal.SIGKILL)
        self.nodes[victim].wait()
        for ex in quick + slow:
            s = self.wait(ex, timeout=90)
            self.assertEqual(s["status"], "COMPLETED", json.dumps(s, indent=2))
            self.assertEqual(len(self.payment(ex)["StepSucceeded"]), 1, "exactly one recorded success for the charge")
        took = "".join(self.node_log(n) for n in self.NODES if n != victim)
        self.assertIn(f"taking over executions of a node that stopped heartbeating from={victim}", took)
        self.await_dead(victim)
        self.start_node(victim)

    def test_c_kill_a_node_and_a_worker_mid_run(self):
        # The node and the worker running the charges die together: the
        # attempts' outcome is unknown and their new owners redeliver them.
        victim = "node-3"
        quick = [self.start(order(f"SH-C{i}", customer=f"cust-{i}")) for i in range(16)]
        slow = [self.start_on(victim, order(f"SH-C9{i}", amount=4242)) for i in range(2)]
        slow += [self.start(order(f"SH-C9{i}", amount=4242)) for i in range(2, 4)]
        self.await_dispatched(slow)
        python = self.c.workers[0]
        python.send_signal(signal.SIGKILL)
        self.nodes[victim].send_signal(signal.SIGKILL)
        self.nodes[victim].wait()
        self.c.spawn("python-restarted", [sys.executable, os.path.join(ROOT, "examples", "workers", "python", "order_workers.py")])
        for ex in quick + slow:
            s = self.wait(ex, timeout=90)
            self.assertEqual(s["status"], "COMPLETED", json.dumps(s, indent=2))
        for ex in slow:
            p = self.payment(ex)
            self.assertEqual(len(p["StepAttemptUncertain"]), 1, json.dumps(p, indent=1))
            self.assertEqual(len(p["StepDispatched"]), 2, "the charge was redelivered")
            self.assertTrue(p["StepDispatched"][0]["worker_session"] != p["StepDispatched"][1]["worker_session"])
            self.assertEqual(len(p["StepSucceeded"]), 1, "exactly one recorded success for the charge")
        self.start_node(victim)

    def test_d_each_schedule_occurrence_fires_once_across_nodes(self):
        self.c.post("/v1/schedules", {"schedule_id": "sh-tick", "workflow": "OrderProcessing", "interval": "1s",
                                      "overlap": "allow", "input": order("SH-T")})
        time.sleep(5)
        self.c.post("/v1/schedules/sh-tick/pause", {})
        time.sleep(1)
        s = self.c.get("/v1/schedules/sh-tick")
        ids = sorted(e["execution_id"] for e in self.c.get("/v1/executions?limit=1000")["executions"]
                     if e["execution_id"].startswith("sh-tick:"))
        self.assertGreaterEqual(s["fired"], 3, s)
        self.assertEqual(s["fired"], len(ids), (s, ids))
        fired = 0
        for n in self.NODES:
            with urllib.request.urlopen(self.node_url(n) + "/v1/metrics", timeout=5) as r:
                m = re.search(r"^orbit_schedules_fired_total (\d+)$", r.read().decode(), re.M)
                fired += int(m.group(1))
        self.assertEqual(fired, s["fired"], "each start was made by exactly one node")
        for ex in ids:
            self.assertEqual(self.wait(ex, timeout=60)["status"], "COMPLETED")


class Schedules(unittest.TestCase):
    """Schedules on a real server with real workers: occurrences started on
    time with deterministic ids, the `orbit schedule` CLI, pause, resume,
    trigger and the overlap policy, and a server killed with SIGKILL."""

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

    def scheduled(self, sid):
        return sorted(e["execution_id"] for e in self.c.get("/v1/executions?limit=1000")["executions"]
                      if e["execution_id"].startswith(sid + ":"))

    def await_count(self, sid, n, timeout=30):
        deadline = time.time() + timeout
        while len(self.scheduled(sid)) < n:
            self.assertLess(time.time(), deadline, f"{sid} did not start {n} executions")
            time.sleep(0.2)

    def test_a_interval_schedule_with_the_cli(self):
        out = self.c.cli("schedule", "create", "every2", "--workflow", "OrderProcessing", "--every", "2s",
                         "--input", json.dumps(order("SC-1"))).stdout
        self.assertIn("active", out)
        self.assertIn("next fires:", out)
        self.await_count("every2", 2)
        for ex in self.scheduled("every2")[:2]:
            # `<schedule-id>:<occurrence>`; the occurrence is on the 2s grid.
            self.assertRegex(ex, r"^every2:\d{4}-\d\d-\d\dT\d\d:\d\d:\d[02468]Z$")
            s = self.c.wait(ex)
            self.assertEqual(s["status"], "COMPLETED", json.dumps(s, indent=2))
            self.assertEqual(s["schedule_id"], "every2")
            self.assertEqual(s["output"]["transaction_id"], "tx-SC-1")
        listing = self.c.cli("schedule", "list").stdout
        self.assertRegex(listing, r"every2\s+OrderProcessing\s+every 2s\s+active")
        self.c.cli("schedule", "pause", "every2")
        time.sleep(0.5)
        paused = len(self.scheduled("every2"))
        time.sleep(4.5)
        self.assertEqual(len(self.scheduled("every2")), paused, "a paused schedule does not fire")
        self.assertIn("paused", self.c.cli("schedule", "get", "every2").stdout)
        started = self.c.cli("schedule", "trigger", "every2").stdout
        self.assertRegex(started, r"started execution every2:manual:")
        info = json.loads(self.c.cli("schedule", "get", "every2", "--json").stdout)
        self.assertEqual(info["fired"], paused + 1)
        self.assertIn(":manual:", info["last_execution_id"])
        self.assertIn("resumed; next fire", self.c.cli("schedule", "resume", "every2").stdout)
        self.await_count("every2", paused + 2)
        self.c.cli("schedule", "delete", "every2")
        missing = self.c.cli("schedule", "get", "every2", check=False)
        self.assertNotEqual(missing.returncode, 0)
        self.assertIn("SCHEDULE_NOT_FOUND", missing.stderr)

    def test_b_overlap_skip_waits_for_the_running_execution(self):
        # amount 4242 makes the charge take 3s: occurrences every second are
        # skipped while one execution runs.
        self.c.post("/v1/schedules", {"schedule_id": "slow", "workflow": "OrderProcessing", "interval": "1s",
                                      "input": order("SC-2", amount=4242)})
        time.sleep(5)
        self.c.post("/v1/schedules/slow/pause", {})
        time.sleep(0.5)
        s = self.c.get("/v1/schedules/slow")
        ids = self.scheduled("slow")
        self.assertGreaterEqual(s["skipped"], 2, s)
        self.assertEqual(s["fired"], len(ids))
        runs = [self.c.wait(ex, timeout=60) for ex in ids]
        for a, b in zip(runs, runs[1:]):
            self.assertLessEqual(a["finished_at_ms"], b["created_at_ms"], "executions never overlapped")

    def test_c_a_killed_server_fires_each_occurrence_once_after_restart(self):
        self.c.post("/v1/schedules", {"schedule_id": "crash", "workflow": "OrderProcessing", "interval": "1s",
                                      "overlap": "allow", "input": order("SC-3")})
        self.await_count("crash", 2)
        self.c.kill_server()
        self.c.start_server()
        before = len(self.scheduled("crash"))
        self.await_count("crash", before + 2)
        self.c.post("/v1/schedules/crash/pause", {})
        time.sleep(0.5)
        s = self.c.get("/v1/schedules/crash")
        ids = self.scheduled("crash")
        self.assertEqual(s["fired"], len(ids), (s, ids))
        first, last = (time.strptime(i.split(":", 1)[1], "%Y-%m-%dT%H:%M:%SZ") for i in (ids[0], ids[-1]))
        # Every occurrence between the first and the last was started or counted as skipped.
        self.assertEqual(calendar.timegm(last) - calendar.timegm(first) + 1, s["fired"] + s["skipped"], (s, ids))
        for ex in ids:
            self.assertEqual(self.c.wait(ex, timeout=60)["status"], "COMPLETED")
        with urllib.request.urlopen(self.c.url + "/v1/metrics", timeout=5) as r:
            self.assertRegex(r.read().decode(), r"orbit_schedules_fired_total \d+")


if __name__ == "__main__":
    unittest.main(verbosity=2)
