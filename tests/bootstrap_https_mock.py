#!/usr/bin/env python3
"""Local HTTPS fixture for the privileged bootstrap lifecycle test."""

from __future__ import annotations

import argparse
import json
import ssl
from http import HTTPStatus
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from urllib.parse import parse_qs, urlparse


ZONE_NAME = "example.test"
ZONE_ID = "0123456789abcdef0123456789abcdef"
RECORD_NAME = "host.example.test"
RECORD_ID = "record-id"
PUBLIC_IPV4 = "8.8.8.8"
API_TOKEN = "cirrusync-integration-token"


def api_envelope(result: object) -> dict[str, object]:
    """Return the subset of a Cloudflare v4 response used by the client."""
    count = len(result) if isinstance(result, list) else 1
    return {
        "success": True,
        "errors": [],
        "messages": [],
        "result": result,
        "result_info": {
            "page": 1,
            "per_page": 100,
            "count": count,
            "total_count": count,
            "total_pages": 1,
        },
    }


def dns_record(content: str = PUBLIC_IPV4) -> dict[str, object]:
    """Return one deterministic A record."""
    return {
        "id": RECORD_ID,
        "type": "A",
        "name": RECORD_NAME,
        "content": content,
        "ttl": 120,
        "proxied": False,
    }


class FixtureHandler(SimpleHTTPRequestHandler):
    """Serve static dumb-Git files and the small API surface used by check."""

    server_version = "CirrusyncBootstrapFixture/1"

    def log_message(self, format_string: str, *args: object) -> None:
        # Request paths are useful on CI failure. Headers (including the bearer
        # token) are deliberately never logged.
        print(
            f"{self.address_string()} - {format_string % args}",
            flush=True,
        )

    def do_GET(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler API
        host = self.headers.get("Host", "").split(":", maxsplit=1)[0]
        parsed = urlparse(self.path)

        if parsed.path == "/health":
            self._send_text(HTTPStatus.OK, "ready\n")
            return
        if host == "api.cloudflare.com":
            self._cloudflare_get(parsed.path, parse_qs(parsed.query))
            return
        if host in {"api.ipify.org", "api6.ipify.org"}:
            self._send_text(HTTPStatus.OK, f"{PUBLIC_IPV4}\n")
            return
        super().do_GET()

    def do_PATCH(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler API
        host = self.headers.get("Host", "").split(":", maxsplit=1)[0]
        parsed = urlparse(self.path)
        expected = (
            f"/client/v4/zones/{ZONE_ID}/dns_records/{RECORD_ID}"
        )
        if host != "api.cloudflare.com" or parsed.path != expected:
            self.send_error(HTTPStatus.NOT_FOUND)
            return
        if not self._authorized():
            return

        length = int(self.headers.get("Content-Length", "0"))
        try:
            payload = json.loads(self.rfile.read(length))
        except (json.JSONDecodeError, UnicodeDecodeError):
            self.send_error(HTTPStatus.BAD_REQUEST)
            return
        self._send_json(
            HTTPStatus.OK,
            api_envelope(
                {
                    "id": RECORD_ID,
                    "type": payload.get("type", "A"),
                    "name": payload.get("name", RECORD_NAME),
                    "content": payload.get("content", PUBLIC_IPV4),
                    "ttl": payload.get("ttl", 120),
                    "proxied": payload.get("proxied", False),
                }
            ),
        )

    def _cloudflare_get(
        self,
        path: str,
        query: dict[str, list[str]],
    ) -> None:
        if path == "/cdn-cgi/trace":
            self._send_text(HTTPStatus.OK, f"fl=ci\nip={PUBLIC_IPV4}\n")
            return
        if not self._authorized():
            return
        if path == "/client/v4/user/tokens/verify":
            self._send_json(
                HTTPStatus.OK,
                api_envelope({"id": "token-id", "status": "active"}),
            )
            return
        if path == "/client/v4/zones":
            requested_name = query.get("name", [""])[0]
            zones: list[dict[str, str]] = []
            if requested_name == ZONE_NAME:
                zones.append(
                    {"id": ZONE_ID, "name": ZONE_NAME, "status": "active"}
                )
            self._send_json(HTTPStatus.OK, api_envelope(zones))
            return
        records_path = f"/client/v4/zones/{ZONE_ID}/dns_records"
        if path == records_path:
            requested_name = query.get("name.exact", [""])[0]
            requested_type = query.get("type", [""])[0]
            records = (
                [dns_record()]
                if requested_name == RECORD_NAME and requested_type == "A"
                else []
            )
            self._send_json(HTTPStatus.OK, api_envelope(records))
            return
        self.send_error(HTTPStatus.NOT_FOUND)

    def _authorized(self) -> bool:
        if self.headers.get("Authorization") == f"Bearer {API_TOKEN}":
            return True
        self._send_json(
            HTTPStatus.UNAUTHORIZED,
            {
                "success": False,
                "errors": [{"code": 1000, "message": "invalid token"}],
                "messages": [],
                "result": None,
            },
        )
        return False

    def _send_json(self, status: HTTPStatus, value: object) -> None:
        body = json.dumps(value, separators=(",", ":")).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _send_text(self, status: HTTPStatus, value: str) -> None:
        body = value.encode()
        self.send_response(status)
        self.send_header("Content-Type", "text/plain; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


def parse_arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("--bind", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=443)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--certificate", type=Path, required=True)
    parser.add_argument("--private-key", type=Path, required=True)
    return parser.parse_args()


def main() -> None:
    arguments = parse_arguments()
    handler = lambda *args, **kwargs: FixtureHandler(  # noqa: E731
        *args,
        directory=str(arguments.root),
        **kwargs,
    )
    server = ThreadingHTTPServer((arguments.bind, arguments.port), handler)
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    context.load_cert_chain(arguments.certificate, arguments.private_key)
    server.socket = context.wrap_socket(server.socket, server_side=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
