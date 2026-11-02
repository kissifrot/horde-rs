#!/usr/bin/env python3
"""Two loopback demo services for federated-login.json; not an OAuth server."""
import argparse
import hmac
import json
import secrets
import threading
from http import cookies
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import parse_qs, urlsplit

lock = threading.Lock()
auth_sessions = {}
tickets = set()
site_sessions = set()


def session_cookie(handler, name):
    jar = cookies.SimpleCookie()
    try:
        jar.load(handler.headers.get("Cookie", ""))
    except cookies.CookieError:
        return ""
    return jar[name].value if name in jar else ""


class Handler(BaseHTTPRequestHandler):
    def respond(self, status, body="", headers=()):
        content = body.encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "text/html; charset=utf-8")
        self.send_header("Content-Length", str(len(content)))
        for name, value in headers:
            self.send_header(name, value)
        self.end_headers()
        self.wfile.write(content)

    def log_message(self, format, *args):
        pass


class Site(Handler):
    def do_GET(self):
        url = urlsplit(self.path)
        if url.path == "/login":
            self.respond(302, headers=[("Location", f"{self.server.auth_url}/authorize")])
        elif url.path == "/callback":
            ticket = parse_qs(url.query).get("ticket", [""])[0]
            with lock:
                valid = ticket in tickets
                if valid:
                    tickets.remove(ticket)
                    session = secrets.token_urlsafe(24)
                    site_sessions.add(session)
            if not valid:
                self.respond(401, "Invalid ticket")
                return
            self.respond(302, headers=[("Set-Cookie", f"site_session={session}; Path=/; HttpOnly; SameSite=Strict"), ("Location", "/protected")])
        elif url.path == "/protected":
            token = session_cookie(self, "site_session")
            with lock:
                authorized = token in site_sessions
            self.respond(200 if authorized else 401, "Welcome back!" if authorized else "Login required")
        else:
            self.respond(404, "Not found")


class Auth(Handler):
    def do_GET(self):
        if urlsplit(self.path).path != "/authorize":
            self.respond(404, "Not found")
            return
        session = secrets.token_urlsafe(24)
        csrf = secrets.token_urlsafe(24)
        with lock:
            auth_sessions[session] = csrf
        self.respond(200, f'<form method="post" action="/authenticate"><input type="hidden" name="csrf" value="{csrf}"><input name="username"><input name="password" type="password"><button>Log in</button></form>', [("Set-Cookie", f"auth_session={session}; Path=/; HttpOnly; SameSite=Strict")])

    def do_POST(self):
        if urlsplit(self.path).path != "/authenticate":
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
        username = form.get("username", [""])[0]
        password = form.get("password", [""])[0]
        csrf = form.get("csrf", [""])[0]
        session = session_cookie(self, "auth_session")
        with lock:
            expected_csrf = auth_sessions.get(session, "")
            valid_session = bool(expected_csrf) and hmac.compare_digest(csrf.encode(), expected_csrf.encode())
        valid_credentials = hmac.compare_digest(username.encode(), b"grunt") and hmac.compare_digest(password.encode(), b"zugzug")
        if not valid_session or not valid_credentials:
            self.respond(401, "Invalid session or credentials")
            return
        ticket = secrets.token_urlsafe(24)
        with lock:
            tickets.add(ticket)
        self.respond(302, headers=[("Location", f"{self.server.site_url}/callback?ticket={ticket}")])


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--site-port", type=int, default=8081)
    parser.add_argument("--auth-port", type=int, default=8082)
    args = parser.parse_args()
    site = ThreadingHTTPServer(("127.0.0.1", args.site_port), Site)
    auth = ThreadingHTTPServer(("127.0.0.1", args.auth_port), Auth)
    site.auth_url = f"http://127.0.0.1:{auth.server_port}"
    auth.site_url = f"http://localhost:{site.server_port}"
    for server in (site, auth):
        threading.Thread(target=server.serve_forever, daemon=True).start()
    print(json.dumps({"site_url": auth.site_url, "auth_url": site.auth_url}), flush=True)
    try:
        threading.Event().wait()
    except KeyboardInterrupt:
        pass
    finally:
        site.shutdown()
        auth.shutdown()
        site.server_close()
        auth.server_close()
