#!/usr/bin/env python3
"""Fixture-backed cockpit preview server (stdlib only, no downloads).

Serves the compiled dashboard assets plus the recorded fixture JSON under a
regenerated serving root, so `make start` previews every read route offline
without a live hub. It replaces the previous inline `python3 -m http.server`
invocation and adds the two UI v2 routes:

* ``GET /api/v1/components/<plugin_id>`` -> ``api/v1/component-detail.json``
* ``GET /api/v1/status``                 -> ``api/v1/status.json``

Every other path (``/``, the compiled assets, ``/api/v1/overview``,
``/api/v1/components``, ``/api/v1/factory``) keeps its exact-file behavior.
Ctrl-C stops the server.

Usage: ``python3 scripts/cockpit-preview.py [PORT] [ROOT]`` (both optional;
defaults 8791 and ``target/start``).
"""

import http.server
import os
import sys
from urllib.parse import unquote, urlsplit

DEFAULT_PORT = 8791
DEFAULT_ROOT = "target/start"


class PreviewHandler(http.server.SimpleHTTPRequestHandler):
    """Exact-file routes plus the two fixture JSON routes."""

    def do_GET(self):  # noqa: N802 (http.server API name)
        path = urlsplit(self.path).path
        if path == "/api/v1/status":
            self._serve_fixture("api/v1/status.json")
            return
        if path.startswith("/api/v1/components/"):
            raw_segment = path[len("/api/v1/components/"):]
            plugin_id = unquote(raw_segment)
            # Mirror the transport: a smuggled separator or NUL is refused.
            if not plugin_id or "/" in plugin_id or "\\" in plugin_id or "\0" in plugin_id:
                self._error(400, "invalid_request")
                return
            self._serve_fixture("api/v1/component-detail.json")
            return
        super().do_GET()

    def do_POST(self):  # noqa: N802 (http.server API name)
        # The read transport grants no mutation method: every POST is refused
        # with 405, matching the dashboard listener.
        body = b"method_not_allowed\n"
        self.send_response(405)
        self.send_header("Content-Type", "text/plain; charset=utf-8")
        self.send_header("Allow", "GET")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _serve_fixture(self, relative):
        target = os.path.join(self.directory, relative)
        try:
            with open(target, "rb") as handle:
                body = handle.read()
        except OSError:
            self._error(404, "not_found")
            return
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _error(self, code, message):
        body = (message + "\n").encode("utf-8")
        self.send_response(code)
        self.send_header("Content-Type", "text/plain; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, fmt, *args):
        # Keep the preview quiet by default; a request line per fetch obscures
        # the banner. Errors still reach stderr through http.server.
        if args and str(args[0]).startswith("4"):
            sys.stderr.write("%s - %s\n" % (self.address_string(), fmt % args))


def main(argv):
    port = DEFAULT_PORT
    root = DEFAULT_ROOT
    if len(argv) >= 2 and argv[1]:
        port = int(argv[1])
    if len(argv) >= 3 and argv[2]:
        root = argv[2]
    root = os.path.abspath(root)

    handler = lambda *args, **kwargs: PreviewHandler(  # noqa: E731
        *args, directory=root, **kwargs
    )
    with http.server.ThreadingHTTPServer(("127.0.0.1", port), handler) as httpd:
        try:
            httpd.serve_forever()
        except KeyboardInterrupt:
            pass
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
