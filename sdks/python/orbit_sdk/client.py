"""Tiny JSON-over-HTTP client for the ORBIT runtime API (standard library only)."""

import json
import os
import ssl
import urllib.error
import urllib.parse
import urllib.request
from typing import Any, Optional, Tuple

DEFAULT_SERVER = "http://127.0.0.1:7233"


class ApiError(Exception):
    """An error response from the runtime, or a connection failure (status 0)."""

    def __init__(self, status: int, code: str, message: str):
        super().__init__(f"{code} ({status}): {message}")
        self.status = status
        self.code = code
        self.message = message

    @property
    def is_connection_error(self) -> bool:
        return self.status == 0


class Client:
    def __init__(
        self,
        server: Optional[str] = None,
        token: Optional[str] = None,
        timeout: float = 70.0,
        ca_cert: Optional[str] = None,
        client_cert: Optional[str] = None,
        client_key: Optional[str] = None,
    ):
        self.server = (server or os.environ.get("ORBIT_SERVER") or DEFAULT_SERVER).rstrip("/")
        # Workers should be given the restricted worker credential when the runtime has one.
        self.token = token if token is not None else (os.environ.get("ORBIT_WORKER_TOKEN") or os.environ.get("ORBIT_TOKEN"))
        self.timeout = timeout
        ca = ca_cert or os.environ.get("ORBIT_CA_CERT")
        # Mutual TLS: a client certificate identifies the worker to the runtime.
        cert = client_cert or os.environ.get("ORBIT_CLIENT_CERT")
        key = client_key or os.environ.get("ORBIT_CLIENT_KEY")
        self._ssl = None
        if ca or cert:
            self._ssl = ssl.create_default_context(cafile=ca)
            if cert:
                self._ssl.load_cert_chain(cert, key)

    def request(self, method: str, path: str, body: Any = None) -> Tuple[int, Any]:
        data = None if body is None else json.dumps(body).encode()
        req = urllib.request.Request(self.server + path, data=data, method=method)
        req.add_header("content-type", "application/json")
        req.add_header("accept", "application/json")
        if self.token:
            req.add_header("authorization", f"Bearer {self.token}")
        try:
            with urllib.request.urlopen(req, timeout=self.timeout, context=self._ssl) as resp:
                raw = resp.read()
                return resp.status, (json.loads(raw) if raw else None)
        except urllib.error.HTTPError as e:
            raw = e.read()
            try:
                err = json.loads(raw).get("error", {})
            except (ValueError, AttributeError):
                err = {}
            raise ApiError(e.code, err.get("code", "HTTP_ERROR"), err.get("message", raw.decode(errors="replace"))) from None
        except (urllib.error.URLError, ConnectionError, TimeoutError, OSError) as e:
            raise ApiError(0, "CONNECTION", f"cannot reach ORBIT runtime at {self.server}: {e}") from None

    def get(self, path: str) -> Any:
        return self.request("GET", path)[1]

    def post(self, path: str, body: Any) -> Tuple[int, Any]:
        return self.request("POST", path, body)

    # Convenience helpers for applications that start workflows.
    def start(self, workflow: str, input: Any = None, version: Optional[str] = None, execution_id: Optional[str] = None) -> str:
        _, r = self.post("/v1/executions", {"workflow": workflow, "version": version, "input": input or {}, "execution_id": execution_id})
        return r["execution_id"]

    def wait(self, execution_id: str, timeout_ms: int = 30000) -> Any:
        return self.get(f"/v1/executions/{execution_id}/wait?timeout_ms={timeout_ms}")

    def signal(self, execution_id: str, name: str, payload: Any = None) -> int:
        """Send a typed signal declared with `signal <name> { ... }` in the workflow.

        The payload is validated by the runtime against the declaration.
        Returns the history sequence number of the recorded signal.
        """
        quote = urllib.parse.quote
        _, r = self.post(f"/v1/executions/{quote(execution_id, safe='')}/signals/{quote(name, safe='')}", payload if payload is not None else {})
        return r["seq"]

    def query(self, execution_id: str, name: str) -> Any:
        """Evaluate a query declared with `query <name> = ...` against the current state."""
        quote = urllib.parse.quote
        return self.get(f"/v1/executions/{quote(execution_id, safe='')}/queries/{quote(name, safe='')}")["value"]
