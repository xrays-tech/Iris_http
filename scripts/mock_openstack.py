#!/usr/bin/env python3
"""Canonical mock of the OpenStack control plane (Keystone v3 + Nova 2.1).

Implements the P5/P7 contract from ``dev-docs/plan/12-mock-fixtures.md``:

* ``POST /v3/auth/tokens`` -> ``201`` + ``X-Subject-Token`` and a body carrying a
  Nova catalog (``type=compute``, public URL pointing at this mock's ``/v2.1``).
* ``GET /v2.1/servers?name=<name>&all_tenants=true`` -> ``200`` JSON from
  ``--servers-dir/<name>.json`` (``404`` when absent), or a forced status/timeout.
* ``GET /v2.1/servers/<uuid>`` (Nova show-by-id) -> ``200`` single-server
  envelope from ``--servers-dir/<uuid>.json`` (``404`` when absent), sharing the
  same forced-status/timeout/401-once gates as the name query.
* Every request's ``X-Auth-Token`` / ``OpenStack-API-Version`` (and method/path)
  is appended as one JSON line to ``--record`` so tests can prove token reuse
  and assert which endpoint (name query vs show-by-id) was hit.

Python standard library only. Prints ``LISTENING <port>`` on stdout once bound.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import threading
import time
from datetime import datetime, timedelta, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlparse

_OPTIONS: "Options" = None  # set in main()
_RECORD_LOCK = threading.Lock()
_AUTH_COUNTER = 0
_UNAUTHORIZED_SERVED = False
_STATE_LOCK = threading.Lock()


class Options:
    def __init__(self, args: argparse.Namespace) -> None:
        self.port: int = args.port
        self.record: str | None = args.record
        self.servers_dir: str | None = args.servers_dir
        self.region: str = args.region
        self.token_ttl: int = args.token_ttl
        self.auth_fail: bool = args.auth_fail
        self.auth_delay: float = args.auth_delay
        self.servers_status: int | None = args.servers_status
        self.servers_unauthorized_once: bool = args.servers_unauthorized_once
        self.delay: float = args.delay
        self.nova_url: str | None = args.nova_url


def _record(method: str, path: str, query: str, headers) -> None:
    if not _OPTIONS.record:
        return
    line = json.dumps(
        {
            "method": method,
            "path": path,
            "query": query,
            "x-auth-token": headers.get("X-Auth-Token"),
            "openstack-api-version": headers.get("OpenStack-API-Version"),
        }
    )
    with _RECORD_LOCK:
        with open(_OPTIONS.record, "a", encoding="utf-8") as handle:
            handle.write(line + "\n")
            handle.flush()


class Handler(BaseHTTPRequestHandler):
    server_version = "mock-openstack/1.0"

    # Silence the default stderr access log; tests rely on `--record` instead.
    def log_message(self, *args, **kwargs) -> None:  # noqa: D401
        pass

    # -- helpers ---------------------------------------------------------
    def _send_json(self, status: int, payload, extra_headers=None) -> None:
        body = json.dumps(payload).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        for key, value in (extra_headers or {}).items():
            self.send_header(key, value)
        self.end_headers()
        self.wfile.write(body)

    def _error(self, status: int, message: str) -> None:
        self._send_json(status, {"error": {"message": message, "code": status}})

    def _nova_base(self) -> str:
        if _OPTIONS.nova_url:
            return _OPTIONS.nova_url
        host = self.headers.get("Host") or f"127.0.0.1:{_OPTIONS.port}"
        return f"http://{host}/v2.1"

    # -- routes ----------------------------------------------------------
    def do_POST(self) -> None:  # noqa: N802
        parsed = urlparse(self.path)
        _record("POST", parsed.path, parsed.query, self.headers)
        if parsed.path.rstrip("/").endswith("/auth/tokens"):
            self._handle_auth()
        else:
            self._error(404, "unknown POST endpoint")

    def _handle_auth(self) -> None:
        global _AUTH_COUNTER
        if _OPTIONS.auth_fail:
            self._error(401, "authentication failed")
            return
        length = int(self.headers.get("Content-Length") or 0)
        if length:
            self.rfile.read(length)  # drain the body
        if _OPTIONS.auth_delay > 0:
            # Accept then stall: exercises the client-side auth timeout (FIX #6).
            time.sleep(_OPTIONS.auth_delay)
        with _STATE_LOCK:
            _AUTH_COUNTER += 1
            token_value = f"mock-token-{_AUTH_COUNTER}"
        expires_at = (datetime.now(timezone.utc) + timedelta(seconds=_OPTIONS.token_ttl))
        body = {
            "token": {
                "expires_at": expires_at.strftime("%Y-%m-%dT%H:%M:%SZ"),
                "catalog": [
                    {
                        "type": "compute",
                        "name": "nova",
                        "endpoints": [
                            {
                                "interface": "public",
                                "region": _OPTIONS.region,
                                "region_id": _OPTIONS.region,
                                "url": self._nova_base(),
                            }
                        ],
                    }
                ],
            }
        }
        self._send_json(201, body, {"X-Subject-Token": token_value})

    def do_GET(self) -> None:  # noqa: N802
        parsed = urlparse(self.path)
        _record("GET", parsed.path, parsed.query, self.headers)
        path = parsed.path.rstrip("/")
        if path.endswith("/servers"):
            self._handle_servers(parsed)
        elif "/servers/" in path:
            server_id = path.rsplit("/", 1)[1]
            self._handle_server_show(server_id)
        else:
            self._error(404, "unknown GET endpoint")

    def _nova_gate(self) -> bool:
        """Shared forced-status / delay / 401-once gates for Nova endpoints.

        Returns ``True`` when an error reply has already been sent, so the
        caller must stop; ``False`` when the request may proceed.
        """
        global _UNAUTHORIZED_SERVED
        if _OPTIONS.servers_unauthorized_once:
            with _STATE_LOCK:
                first = not _UNAUTHORIZED_SERVED
                _UNAUTHORIZED_SERVED = True
            if first:
                self._error(401, "token expired")
                return True
        if _OPTIONS.delay > 0:
            time.sleep(_OPTIONS.delay)
        if _OPTIONS.servers_status is not None:
            self._error(_OPTIONS.servers_status, "forced status")
            return True
        return False

    def _send_fixture(self, fixture: str) -> None:
        with open(fixture, "rb") as handle:
            body = handle.read()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _handle_servers(self, parsed) -> None:
        if self._nova_gate():
            return

        name = (parse_qs(parsed.query).get("name") or [""])[0]
        if not name or not _OPTIONS.servers_dir:
            self._send_json(200, {"servers": []})
            return

        fixture = os.path.join(_OPTIONS.servers_dir, f"{name}.json")
        if not os.path.isfile(fixture):
            self._error(404, "instance not found")
            return
        self._send_fixture(fixture)

    def _handle_server_show(self, server_id: str) -> None:
        if self._nova_gate():
            return
        if not _OPTIONS.servers_dir:
            self._error(404, "instance not found")
            return
        fixture = os.path.join(_OPTIONS.servers_dir, f"{server_id}.json")
        if not os.path.isfile(fixture):
            self._error(404, "instance not found")
            return
        self._send_fixture(fixture)


def _parse_args(argv) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Mock OpenStack control plane")
    parser.add_argument("--port", type=int, default=0, help="listen port (0 = ephemeral)")
    parser.add_argument("--record", default=None, help="append request records to this file")
    parser.add_argument("--servers-dir", default=None, help="dir of <name>.json Nova replies")
    parser.add_argument("--region", default="RegionOne")
    parser.add_argument("--token-ttl", type=int, default=3600)
    parser.add_argument("--auth-fail", action="store_true", help="return 401 from /v3/auth/tokens")
    parser.add_argument(
        "--auth-delay",
        type=float,
        default=0.0,
        help="accept then sleep before replying to /v3/auth/tokens (timeout test)",
    )
    parser.add_argument("--servers-status", type=int, default=None, help="force a status for /servers")
    parser.add_argument(
        "--servers-unauthorized-once",
        action="store_true",
        help="first /servers request returns 401",
    )
    parser.add_argument("--delay", type=float, default=0.0, help="sleep seconds before /servers reply")
    parser.add_argument("--nova-url", default=None, help="override the catalog Nova URL")
    return parser.parse_args(argv)


def main(argv) -> int:
    global _OPTIONS
    _OPTIONS = Options(_parse_args(argv))
    httpd = ThreadingHTTPServer(("127.0.0.1", _OPTIONS.port), Handler)
    port = httpd.server_address[1]
    sys.stdout.write(f"LISTENING {port}\n")
    sys.stdout.flush()
    try:
        httpd.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        httpd.server_close()
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
