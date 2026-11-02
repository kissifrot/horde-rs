#!/usr/bin/env python3
"""Loopback-only demo server for login.json; not a production server."""
import argparse
import hmac
import secrets
import threading
from http import cookies
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlsplit

sessions = set()
sessions_lock = threading.Lock()


class Handler(BaseHTTPRequestHandler):
    def respond(self, status, content, cookie=None):
        body = content.encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(body)))
        if cookie:
            self.send_header("Set-Cookie", f"session={cookie}; Path=/; HttpOnly; SameSite=Strict")
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        path = urlsplit(self.path).path
        if path == "/login":
            self.respond(200, '<form method="post" action="/login"><input name="username"><input name="password" type="password"><button>Log in</button></form>')
        elif path == "/protected":
            jar = cookies.SimpleCookie()
            try:
                jar.load(self.headers.get("Cookie", ""))
            except cookies.CookieError:
                self.respond(400, "Invalid cookie")
                return
            token = jar["session"].value if "session" in jar else ""
            with sessions_lock:
                authenticated = token in sessions
            self.respond(200 if authenticated else 401, "Lok'tar, welcome!" if authenticated else "Login required")
        else:
            self.respond(404, "Not found")

    def do_POST(self):
        if urlsplit(self.path).path != "/login":
            self.respond(404, "Not found")
            return
        try:
            length = int(self.headers.get("Content-Length", "0"))
        except ValueError:
            self.respond(400, "Invalid content length")
            return
        if not 0 <= length <= 4096:
            self.respond(400, "Invalid content length")
            return
        form = parse_qs(self.rfile.read(length).decode("utf-8", errors="replace"))
        username, password = form.get("username", [""])[0], form.get("password", [""])[0]
        if hmac.compare_digest(username.encode(), b"grunt") and hmac.compare_digest(password.encode(), b"zugzug"):
            token = secrets.token_urlsafe(24)
            with sessions_lock:
                sessions.add(token)
            self.respond(200, "Logged in", token)
        else:
            self.respond(401, "Invalid credentials")

    def log_message(self, format, *args):
        pass


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--port", type=int, default=8080)
    args = parser.parse_args()
    server = ThreadingHTTPServer(("127.0.0.1", args.port), Handler)
    print(f"Listening on http://127.0.0.1:{server.server_port}", flush=True)
    server.serve_forever()
