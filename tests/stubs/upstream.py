#!/usr/bin/env python3
"""Shared HTTP upstream stub for Iris end-to-end tests (plan/12 §2).

One process can serve several ports (repeatable ``--port``); every port serves
the same behaviour. It has **no third-party dependencies** — only the standard
library — so it runs anywhere Python 3 is available.

Each response advertises the listening port via the ``X-Upstream-Port`` header
so tests can assert load-balancing distribution.

Usage
-----
    python3 tests/stubs/upstream.py --port 18081 --port 18082
    python3 tests/stubs/upstream.py --port 18083 --status 503 --body down
    python3 tests/stubs/upstream.py --port 18084 --delay-ms 500
    python3 tests/stubs/upstream.py --port 18085 --drop

Flags
-----
--port N       Listen port; repeat for multiple ports.
--status C     HTTP status code to return (default 200).
--body S       Response body (default ``ok``).
--delay-ms D   Delay before responding, in milliseconds (default 0).
--drop         Accept the connection then close it without sending a response
               (simulates a hard disconnect).
--path P       Only answer this path; other paths get 404. Useful as an HTTP
               health-check endpoint (``--path /healthz``).
"""

from __future__ import annotations

import argparse
import socket
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class _Config:
    """Per-process response behaviour."""

    def __init__(self, args: argparse.Namespace) -> None:
        self.status: int = args.status
        self.body: bytes = args.body.encode("utf-8")
        self.delay_ms: int = args.delay_ms
        self.drop: bool = args.drop
        self.path: str | None = args.path


class _Handler(BaseHTTPRequestHandler):
    # HTTP/1.1 keeps connections alive; every response sets Content-Length.
    protocol_version = "HTTP/1.1"
    server_version = "iris-stub"

    def _respond(self, *, include_body: bool = True) -> None:
        cfg: _Config = self.server.cfg  # type: ignore[attr-defined]

        if cfg.drop:
            self.close_connection = True
            try:
                self.connection.shutdown(socket.SHUT_RDWR)
            except OSError:
                pass
            self.connection.close()
            return

        request_path = self.path.split("?", 1)[0]
        if cfg.path is not None and request_path != cfg.path:
            self.send_response(404)
            self.send_header("Content-Length", "0")
            self.end_headers()
            return

        if cfg.delay_ms:
            time.sleep(cfg.delay_ms / 1000.0)

        self.send_response(cfg.status)
        self.send_header("Content-Type", "text/plain; charset=utf-8")
        self.send_header("Content-Length", str(len(cfg.body)))
        self.send_header("X-Upstream-Port", str(self.server.server_port))
        self.end_headers()
        if include_body:
            self.wfile.write(cfg.body)

    def do_GET(self) -> None:  # noqa: N802 (stdlib naming)
        self._respond()

    def do_HEAD(self) -> None:  # noqa: N802
        self._respond(include_body=False)

    def log_message(self, *_args: object) -> None:
        """Silence the default stderr access log."""


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Iris upstream HTTP stub")
    parser.add_argument(
        "--port",
        type=int,
        action="append",
        required=True,
        dest="ports",
        help="Listening port (repeatable).",
    )
    parser.add_argument("--status", type=int, default=200, help="HTTP status code.")
    parser.add_argument("--body", default="ok", help="Response body.")
    parser.add_argument("--delay-ms", type=int, default=0, dest="delay_ms",
                        help="Response delay in milliseconds.")
    parser.add_argument("--drop", action="store_true",
                        help="Close the connection without responding.")
    parser.add_argument("--path", default=None,
                        help="Only answer this path (others 404).")
    return parser.parse_args(argv)


def build_server(port: int, cfg: _Config) -> ThreadingHTTPServer:
    server = ThreadingHTTPServer(("127.0.0.1", port), _Handler)
    server.daemon_threads = True
    server.cfg = cfg  # type: ignore[attr-defined]
    return server


def main(argv: list[str]) -> int:
    args = parse_args(argv)
    cfg = _Config(args)

    servers = []
    for port in args.ports:
        servers.append(build_server(port, cfg))

    threads = []
    for server in servers:
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        threads.append(thread)
        print(f"[stub] listening on 127.0.0.1:{server.server_port}", flush=True)

    try:
        for thread in threads:
            thread.join()
    except KeyboardInterrupt:
        pass
    finally:
        for server in servers:
            server.shutdown()
    return 0


if __name__ == "__main__":
    raise SystemExit(main(sys.argv[1:]))
