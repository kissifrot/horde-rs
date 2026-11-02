#!/usr/bin/env python3
"""Functional checks for the loopback benchmark server."""
import socket
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


class ServerTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.temp = tempfile.TemporaryDirectory(prefix="horde-server-test-")
        binary = Path(cls.temp.name) / "server"
        subprocess.run(["rustc", "--edition=2024", "-O", str(ROOT / "benchmarks/server.rs"), "-o", str(binary)], check=True)
        cls.server = subprocess.Popen([str(binary)], stdout=subprocess.PIPE, text=True)
        cls.port = int(cls.server.stdout.readline())

    @classmethod
    def tearDownClass(cls):
        cls.server.terminate()
        cls.server.wait(timeout=5)
        cls.temp.cleanup()

    def test_keep_alive_and_close(self):
        response = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK"
        with socket.create_connection(("127.0.0.1", self.port), timeout=5) as conn:
            conn.settimeout(5)
            with conn.makefile("rb") as reader:
                for request in [b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n",
                                b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive, close\r\n\r\n"]:
                    conn.sendall(request)
                    self.assertEqual(reader.read(len(response)), response)
                self.assertEqual(reader.read(1), b"")

    def test_multiple_connections(self):
        sockets = [socket.create_connection(("127.0.0.1", self.port), timeout=5) for _ in range(64)]
        try:
            for conn in sockets:
                conn.sendall(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            for conn in sockets:
                with conn.makefile("rb") as reader:
                    self.assertIn(b"200 OK", reader.read())
        finally:
            for conn in sockets:
                conn.close()


if __name__ == "__main__":
    unittest.main()
