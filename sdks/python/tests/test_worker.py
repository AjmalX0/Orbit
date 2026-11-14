"""Protocol-level tests for the Python SDK against a scripted fake runtime.

    PYTHONPATH=sdks/python python3 -m unittest discover -s sdks/python/tests
"""

import json
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from orbit_sdk import TaskError, Worker


class FakeRuntime:
    def __init__(self, tasks):
        self.lock = threading.Lock()
        self.tasks = list(tasks)
        self.registrations = 0
        self.forget_once = True      # first poll answers UNKNOWN_SESSION
        self.fail_complete = 2       # first two /complete calls answer 503
        self.completed = {}
        self.failed = {}
        self.cancel = []
        self.auth = None
        fake = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, *args):
                pass

            def reply(self, status, body=None):
                data = b"" if body is None else json.dumps(body).encode()
                self.send_response(status)
                self.send_header("content-type", "application/json")
                self.send_header("content-length", str(len(data)))
                self.end_headers()
                self.wfile.write(data)

            def do_POST(self):
                body = json.loads(self.rfile.read(int(self.headers.get("content-length", 0))) or b"{}")
                with fake.lock:
                    fake.auth = self.headers.get("authorization")
                    path = self.path
                    if path == "/v1/workers/register":
                        fake.registrations += 1
                        return self.reply(200, {"accepted": True, "worker_session_id": "s", "heartbeat_interval_ms": 500})
                    if path == "/v1/workers/poll":
                        if fake.forget_once:
                            fake.forget_once = False
                            return self.reply(404, {"error": {"code": "UNKNOWN_SESSION", "message": "gone"}})
                        if not fake.tasks:
                            task = None
                        else:
                            task = fake.tasks.pop(0)
                    elif path == "/v1/workers/heartbeat":
                        return self.reply(200, {"accepted": True, "cancel": fake.cancel})
                    elif path == "/v1/tasks/complete":
                        if fake.fail_complete:
                            fake.fail_complete -= 1
                            return self.reply(503, {"error": {"code": "UNAVAILABLE", "message": "later"}})
                        fake.completed[body["token"]] = body["output"]
                        return self.reply(200, {"accepted": True})
                    elif path == "/v1/tasks/fail":
                        fake.failed[body["token"]] = body
                        return self.reply(200, {"accepted": True})
                    else:
                        return self.reply(200, {"accepted": True})
                if task is None:
                    time.sleep(0.02)
                    return self.reply(204)
                return self.reply(200, task)

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.url = f"http://127.0.0.1:{self.server.server_port}"
        threading.Thread(target=self.server.serve_forever, daemon=True).start()


def task(token, name, input=None):
    return {"token": token, "execution_id": "e", "step_id": token, "task_name": name, "contract_version": "1.0.0",
            "input": input or {}, "idempotency_key": f"e/{token}", "attempt": 1}


class WorkerProtocolTest(unittest.TestCase):
    def test_protocol(self):
        fake = FakeRuntime([
            task("ok", "echo", {"n": 1}),
            task("biz", "fail"),
            task("boom", "crash"),
            task("slow", "slow"),
            task("async", "async_echo", {"n": 5}),
            task("bad", "not_a_dict"),
        ])
        w = Worker("py-test", server=fake.url, token="wt", concurrency=2, poll_wait_ms=100)

        @w.task("echo")
        def echo(input, ctx):
            self.assertEqual(ctx.idempotency_key, "e/ok")
            return {"n": input["n"] + 1}

        @w.task("fail")
        def fail(input, ctx):
            raise TaskError("CARD_DECLINED", "no funds", retryable=False)

        @w.task("crash")
        def crash(input, ctx):
            raise ValueError("kaboom")

        @w.task("slow")
        def slow(input, ctx):
            with fake.lock:
                fake.cancel = [ctx.token]
            if not ctx.cancelled.wait(10):
                return {"finished": True}
            raise TaskError("CANCELLED", "stopped", retryable=False)

        @w.task("async_echo")
        async def async_echo(input, ctx):
            return {"n": input["n"] * 2}

        @w.task("not_a_dict")
        def not_a_dict(input, ctx):
            return [1, 2]

        thread = w.start()
        deadline = time.time() + 15
        while time.time() < deadline:
            with fake.lock:
                if len(fake.completed) == 2 and len(fake.failed) == 4:
                    break
            time.sleep(0.02)
        w.stop()
        thread.join(5)

        self.assertEqual(fake.completed["ok"], {"n": 2})
        self.assertEqual(fake.completed["async"], {"n": 10})
        self.assertGreaterEqual(fake.registrations, 2, "re-registers after UNKNOWN_SESSION")
        self.assertEqual(fake.failed["biz"]["error_code"], "CARD_DECLINED")
        self.assertFalse(fake.failed["biz"]["retryable"])
        self.assertEqual(fake.failed["boom"]["error_code"], "UNHANDLED_EXCEPTION")
        self.assertTrue(fake.failed["boom"]["retryable"])
        self.assertIn("kaboom", fake.failed["boom"]["error_message"])
        self.assertEqual(fake.failed["slow"]["error_code"], "CANCELLED")
        self.assertEqual(fake.failed["bad"]["error_code"], "INVALID_RESULT")
        self.assertEqual(fake.auth, "Bearer wt")


if __name__ == "__main__":
    unittest.main()
