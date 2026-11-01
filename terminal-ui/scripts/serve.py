# SPDX-License-Identifier: Apache-2.0
# Copyright 2026 Anton (darkcite)
"""Test harness only: serve terminal-ui/dist on 127.0.0.1:9393 with the plan's
exact CSP and headers (§3), so the spike is checked under the real policy.
The terminal's own server (Rust, mio) replaces this in T1a."""
import http.server
import os
import sys

CSP = ("default-src 'none'; script-src 'self' 'wasm-unsafe-eval'; "
       "connect-src 'self' ws://127.0.0.1:9393; img-src 'self'; "
       "style-src 'self'; base-uri 'none'; form-action 'self'; "
       "frame-ancestors 'none'")
TYPES = {".html": "text/html; charset=utf-8", ".js": "text/javascript; charset=utf-8",
         ".wasm": "application/wasm", ".css": "text/css; charset=utf-8"}
ALLOWED = {"/", "/index.html", "/loader.js", "/term.css", "/term_ui.js", "/term_ui_bg.wasm"}


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        path = self.path.split("?", 1)[0].split("#", 1)[0]
        host = self.headers.get("Host", "")
        if host != "127.0.0.1:9393" or path not in ALLOWED:
            self.send_error(404 if host == "127.0.0.1:9393" else 421)
            return
        name = "index.html" if path == "/" else path[1:]
        with open(os.path.join(ROOT, name), "rb") as f:
            body = f.read()
        self.send_response(200)
        self.send_header("Content-Type", TYPES[os.path.splitext(name)[1]])
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Content-Security-Policy", CSP)
        self.send_header("X-Content-Type-Options", "nosniff")
        self.send_header("Referrer-Policy", "no-referrer")
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, fmt, *args):
        sys.stderr.write("serve: " + (fmt % args) + "\n")


ROOT = sys.argv[1] if len(sys.argv) > 1 else "dist"
http.server.ThreadingHTTPServer(("127.0.0.1", 9393), Handler).serve_forever()
