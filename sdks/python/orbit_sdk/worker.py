"""ORBIT worker runtime for Python.

Delivery is at-least-once: a task can be delivered again after a worker or
runtime crash (``ctx.redelivery`` is then True). Make side effects idempotent
using ``ctx.idempotency_key``, which is stable across retries and redeliveries.
"""

import asyncio
import inspect
import json
import logging
import os
import random
import signal
import socket
import threading
import time
import traceback
from typing import Any, Callable, Dict, Optional, Set

from .client import ApiError, Client

log = logging.getLogger("orbit")


class TaskError(Exception):
    """Raise from a handler to fail the attempt with a structured error.

    ``retryable=False`` fails the step immediately regardless of its retry policy.
    """

    def __init__(self, code: str, message: str = "", retryable: bool = True, details: Any = None):
        super().__init__(f"{code}: {message}")
        self.code = code
        self.message = message
        self.retryable = retryable
        self.details = details


class Context:
    """Information about the attempt being executed."""

    def __init__(self, task: Dict[str, Any], worker: "Worker"):
        self.token: str = task["token"]
        self.execution_id: str = task["execution_id"]
        self.workflow: str = task.get("workflow", "")
        self.workflow_version: str = task.get("workflow_version", "")
        self.step_id: str = task["step_id"]
        self.task_name: str = task["task_name"]
        self.attempt: int = task.get("attempt", 1)
        self.idempotency_key: str = task.get("idempotency_key", "")
        self.redelivery: bool = task.get("redelivery", False)
        self.deadline_ms: Optional[int] = task.get("deadline_ms")
        self.cancelled = threading.Event()
        self._worker = worker

    @property
    def is_cancelled(self) -> bool:
        """True once the runtime asked this attempt to stop (timeout, cancellation)."""
        return self.cancelled.is_set()

    def remaining_ms(self) -> Optional[int]:
        if self.deadline_ms is None:
            return None
        return max(0, self.deadline_ms - int(time.time() * 1000))

    def log(self, message: str, **fields: Any) -> None:
        record = {"execution_id": self.execution_id, "step": self.step_id, "attempt": self.attempt, **fields}
        log.info("%s %s", message, json.dumps(record, default=str))


Handler = Callable[[Dict[str, Any], Context], Any]


class _Task:
    def __init__(self, name: str, version: str, fn: Handler):
        self.name = name
        self.version = version
        self.fn = fn


class Worker:
    def __init__(
        self,
        worker_id: Optional[str] = None,
        server: Optional[str] = None,
        token: Optional[str] = None,
        concurrency: int = 4,
        runtime: str = "python",
        poll_wait_ms: int = 20000,
    ):
        self.worker_id = worker_id or f"python-{socket.gethostname()}-{os.getpid()}"
        self.client = Client(server, token, timeout=poll_wait_ms / 1000 + 15)
        self.concurrency = max(1, concurrency)
        self.runtime = runtime
        self.poll_wait_ms = poll_wait_ms
        self._tasks: Dict[str, _Task] = {}
        self._session: Optional[str] = None
        self._session_lock = threading.Lock()
        self._heartbeat_interval = 5.0
        self._stop = threading.Event()
        self._active: Dict[str, Context] = {}
        self._active_lock = threading.Lock()

    # ------------------------------------------------------------ registration

    def task(self, name: str, version: str = "1.0.0") -> Callable[[Handler], Handler]:
        """Decorator registering ``fn(input, ctx)`` as the implementation of ``name@version``."""

        def wrap(fn: Handler) -> Handler:
            self.register(name, fn, version)
            return fn

        return wrap

    def register(self, name: str, fn: Handler, version: str = "1.0.0") -> None:
        if name in self._tasks:
            raise ValueError(f"task {name!r} is already registered")
        self._tasks[name] = _Task(name, version, fn)

    def _register_session(self) -> str:
        with self._session_lock:
            body = {
                "worker_id": self.worker_id,
                "runtime": self.runtime,
                "sdk": "orbit-python/0.3.0",
                "contracts": [{"task_name": t.name, "contract_version": t.version} for t in self._tasks.values()],
            }
            _, r = self.client.post("/v1/workers/register", body)
            self._session = r["worker_session_id"]
            self._heartbeat_interval = max(0.5, r.get("heartbeat_interval_ms", 5000) / 1000)
            for w in r.get("warnings", []):
                log.warning("runtime: %s", w)
            log.info("worker %s registered (%d tasks)", self.worker_id, len(self._tasks))
            return self._session

    def _session_id(self) -> str:
        return self._session or self._register_session()

    def _with_session(self, fn: Callable[[str], Any]) -> Any:
        """Call ``fn(session)``, re-registering if the runtime forgot the session (e.g. after a restart)."""
        session = self._session_id()
        try:
            return fn(session)
        except ApiError as e:
            if e.status == 404 and e.code == "UNKNOWN_SESSION":
                with self._session_lock:
                    if self._session == session:
                        self._session = None
                return fn(self._session_id())
            raise

    # ----------------------------------------------------------------- running

    def run(self, install_signal_handlers: bool = True) -> None:
        """Run until stopped (SIGINT/SIGTERM or :meth:`stop`)."""
        if not self._tasks:
            raise RuntimeError("no tasks registered")
        logging.basicConfig(level=os.environ.get("ORBIT_LOG_LEVEL", "INFO"), format="%(asctime)s %(levelname)s %(name)s %(message)s")
        if install_signal_handlers and threading.current_thread() is threading.main_thread():
            for sig in (signal.SIGINT, signal.SIGTERM):
                signal.signal(sig, lambda *_: self.stop())
        self._retrying(self._register_session)
        threads = [threading.Thread(target=self._heartbeat_loop, name="orbit-heartbeat", daemon=True)]
        threads += [threading.Thread(target=self._poll_loop, name=f"orbit-poll-{i}", daemon=True) for i in range(self.concurrency)]
        for t in threads:
            t.start()
        try:
            while not self._stop.is_set():
                self._stop.wait(0.5)
        finally:
            for t in threads[1:]:
                t.join(timeout=self.poll_wait_ms / 1000 + 5)
            try:
                if self._session:
                    self.client.post("/v1/workers/unregister", {"worker_session_id": self._session})
            except ApiError:
                pass
            log.info("worker %s stopped", self.worker_id)

    def start(self) -> threading.Thread:
        """Run in a background thread (useful in tests and embedded use)."""
        t = threading.Thread(target=self.run, kwargs={"install_signal_handlers": False}, daemon=True)
        t.start()
        return t

    def stop(self) -> None:
        self._stop.set()

    def _retrying(self, fn: Callable[[], Any]) -> Any:
        delay = 0.2
        while True:
            try:
                return fn()
            except ApiError as e:
                if not e.is_connection_error and e.status < 500:
                    raise
                if self._stop.is_set():
                    log.warning("%s; giving up because the worker is stopping", e.message)
                    return None
                log.warning("%s; retrying in %.1fs", e.message, delay)
                self._stop.wait(delay)
                delay = min(delay * 2, 5.0) * (0.8 + random.random() * 0.4)

    def _heartbeat_loop(self) -> None:
        while not self._stop.wait(self._heartbeat_interval):
            try:
                r = self._with_session(lambda s: self.client.post("/v1/workers/heartbeat", {"worker_session_id": s})[1])
                for token in r.get("cancel", []):
                    with self._active_lock:
                        ctx = self._active.get(token)
                    if ctx:
                        ctx.log("cancellation requested by runtime")
                        ctx.cancelled.set()
            except ApiError as e:
                log.debug("heartbeat failed: %s", e)

    def _poll_loop(self) -> None:
        backoff = 0.2
        while not self._stop.is_set():
            try:
                status, task = self._with_session(
                    lambda s: self.client.post("/v1/workers/poll", {"worker_session_id": s, "wait_ms": self.poll_wait_ms})
                )
                backoff = 0.2
            except ApiError as e:
                if self._stop.is_set():
                    return
                log.warning("poll failed: %s; retrying in %.1fs", e.message, backoff)
                self._stop.wait(backoff)
                backoff = min(backoff * 2, 5.0)
                continue
            if status == 204 or not task:
                continue
            self._execute(task)

    def _execute(self, task: Dict[str, Any]) -> None:
        ctx = Context(task, self)
        impl = self._tasks.get(task["task_name"])
        with self._active_lock:
            self._active[ctx.token] = ctx
        try:
            if impl is None:
                raise TaskError("UNKNOWN_TASK", f"worker does not implement {task['task_name']}", retryable=True)
            ctx.log("executing", task=ctx.task_name, redelivery=ctx.redelivery)
            result = impl.fn(task.get("input") or {}, ctx)
            if inspect.isawaitable(result):
                result = asyncio.run(_await(result))
            if result is None:
                result = {}
            if not isinstance(result, dict):
                raise TaskError("INVALID_RESULT", f"handler returned {type(result).__name__}; tasks must return a dict", retryable=False)
            self._report("/v1/tasks/complete", {"token": ctx.token, "output": result})
        except TaskError as e:
            ctx.log("task failed", code=e.code)
            self._report("/v1/tasks/fail", {"token": ctx.token, "error_code": e.code, "error_message": e.message, "retryable": e.retryable, "details": e.details})
        except Exception as e:  # noqa: BLE001 - report any handler crash to the runtime
            tb = traceback.format_exc(limit=8)
            log.error("task %s raised %s", task["task_name"], tb)
            self._report(
                "/v1/tasks/fail",
                {"token": ctx.token, "error_code": "UNHANDLED_EXCEPTION", "error_message": f"{type(e).__name__}: {e}", "retryable": True, "details": {"traceback": tb}},
            )
        finally:
            with self._active_lock:
                self._active.pop(ctx.token, None)

    def _report(self, path: str, body: Dict[str, Any]) -> None:
        """Deliver a result, retrying through connection failures so that a
        result survives a runtime restart (the runtime reconciles it)."""
        r = self._retrying(lambda: self.client.post(path, body)[1])
        if r and not r.get("accepted", True):
            log.info("runtime ignored result for %s: %s", body["token"][:8], r.get("reason"))


async def _await(aw: Any) -> Any:
    return await aw
