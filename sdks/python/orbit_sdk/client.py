"""Tiny JSON-over-HTTP client for the ORBIT runtime API (standard library only)."""

import json
import os
import urllib.error
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
    def __init__(self, server: Optional[str] = None, token: Optional[str] = None, timeout: float = 70.0):
        self.server = (server or os.environ.get("ORBIT_SERVER") or DEFAULT_SERVER).rstrip("/")
        self.token = token if token is not None else os.environ.get("ORBIT_TOKEN")
        self.timeout = timeout

    def request(self, method: str, path: str, body: Any = None) -> Tuple[int, Any]:
        data = None if body is None else json.dumps(body).encode()
        req = urllib.request.Request(self.server + path, data=data, method=method)
        req.add_header("content-type", "application/json")
        req.add_header("accept", "application/json")
        if self.token:
            req.add_header("authorization", f"Bearer {self.token}")
        try:
            with urllib.request.urlopen(req, timeout=self.timeout) as resp:
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
