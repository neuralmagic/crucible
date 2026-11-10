#!/usr/bin/env python3
"""Serve the browser build and forward its API calls to a controller, for local development.

    web-dev-proxy.py [--port 8765] [--dir target/web] [--write]

The page talks to its own origin, so this stands in for the controller serving it: files come
from --dir, and /api and /auth go to the controller in crux's config (or CONTROLLER_URL) with
crux's key (or CONTROLLER_API_TOKEN). Unless --write is given, only GET, HEAD and the POSTs that
store nothing (webhook check and preview) are forwarded.
Listens on 127.0.0.1 alone: anything that can reach the port acts as the key's owner.
"""

import argparse
import http.client
import http.server
import os
import pathlib
import tomllib
import urllib.parse

FORWARDED = ("/api/", "/auth/")
# POSTs that only evaluate what they are sent, forwarded even when read-only.
STORES_NOTHING = ("/api/webhooks/check", "/api/webhooks/preview")
FORWARDED_HEADERS = ("accept", "content-type", "last-event-id", "x-crucible-act-as")
HOP_BY_HOP = {"connection", "keep-alive", "transfer-encoding", "te", "trailer", "upgrade",
              "proxy-authenticate", "proxy-authorization", "content-length", "set-cookie"}


def crux_config() -> dict:
    base = os.environ.get("XDG_CONFIG_HOME") or os.path.join(os.path.expanduser("~"), ".config")
    path = pathlib.Path(base, "crux", "config.toml")
    return tomllib.loads(path.read_text()) if path.exists() else {}


class Handler(http.server.SimpleHTTPRequestHandler):
    upstream: urllib.parse.SplitResult
    token: str | None
    write: bool

    def do_GET(self):
        if self.path.startswith(FORWARDED):
            self.forward()
        else:
            super().do_GET()

    def do_HEAD(self):
        if self.path.startswith(FORWARDED):
            self.forward()
        else:
            super().do_HEAD()

    def do_POST(self):
        if self.path in STORES_NOTHING:
            self.forward()
        else:
            self.mutate()

    def do_PUT(self):
        self.mutate()

    def do_PATCH(self):
        self.mutate()

    def do_DELETE(self):
        self.mutate()

    def mutate(self):
        if not self.path.startswith(FORWARDED):
            self.send_error(404)
        elif self.write:
            self.forward()
        else:
            self.send_error(405, "read-only: start the proxy with --write to forward changes")

    def end_headers(self):
        if not self.path.startswith(FORWARDED):
            self.send_header("Cache-Control", "no-store")
        super().end_headers()

    def forward(self):
        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length) if length else None
        headers = {name: value for name, value in self.headers.items()
                   if name.lower() in FORWARDED_HEADERS}
        headers["Accept-Encoding"] = "identity"
        if self.token:
            headers["Authorization"] = f"Bearer {self.token}"
        secure = self.upstream.scheme == "https"
        connect = http.client.HTTPSConnection if secure else http.client.HTTPConnection
        upstream = connect(self.upstream.netloc, timeout=300)
        try:
            upstream.request(self.command, self.path, body=body, headers=headers)
            answer = upstream.getresponse()
            self.send_response(answer.status)
            for name, value in answer.getheaders():
                if name.lower() not in HOP_BY_HOP:
                    self.send_header(name, value)
            self.send_header("Connection", "close")
            self.end_headers()
            while chunk := answer.read1(65536):
                self.wfile.write(chunk)
                self.wfile.flush()
        except (BrokenPipeError, ConnectionResetError):
            pass
        except OSError as error:
            self.send_error(502, f"reaching the controller: {error}")
        finally:
            upstream.close()
            self.close_connection = True

    def log_message(self, format, *args):
        if self.path.startswith(FORWARDED):
            super().log_message(format, *args)


def main():
    parser = argparse.ArgumentParser(
        description="Serve the browser build and forward its API calls to a controller."
    )
    parser.add_argument("--port", type=int, default=8765)
    parser.add_argument("--dir", default="target/web")
    parser.add_argument("--write", action="store_true")
    args = parser.parse_args()
    config = crux_config()
    url = os.environ.get("CONTROLLER_URL") or config.get("url")
    if not url:
        raise SystemExit("no controller: set CONTROLLER_URL or `url` in crux's config")
    Handler.upstream = urllib.parse.urlsplit(url.rstrip("/"))
    Handler.token = os.environ.get("CONTROLLER_API_TOKEN") or config.get("api_token")
    Handler.write = args.write
    directory = os.path.abspath(args.dir)
    server = http.server.ThreadingHTTPServer(
        ("127.0.0.1", args.port),
        lambda *a, **k: Handler(*a, directory=directory, **k),
    )
    mode = "read and write" if args.write else "read-only"
    print(f"http://127.0.0.1:{args.port} -> {url} ({mode})", flush=True)
    server.serve_forever()


if __name__ == "__main__":
    main()
