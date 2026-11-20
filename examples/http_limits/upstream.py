#!/usr/bin/env python3
"""The demo's upstream: POST /login answers 200 when the X-Password
header is "right" and 401 otherwise; every other request answers 200
with the path. Usage: upstream.py PORT."""
import sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def answer(self):
        length = int(self.headers.get("Content-Length") or 0)
        if length:
            self.rfile.read(length)
        status = 200
        if self.path == "/login" and self.command == "POST":
            status = 200 if self.headers.get("X-Password") == "right" else 401
        body = f"{status} {self.path}\n".encode()
        self.send_response(status)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    do_GET = do_POST = answer

    def log_message(self, *args):
        pass


class Server(ThreadingHTTPServer):
    # The default backlog of 5 resets connections the proxy opens at once.
    request_queue_size = 128
    daemon_threads = True


Server(("127.0.0.1", int(sys.argv[1])), Handler).serve_forever()
