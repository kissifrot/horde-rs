#!/usr/bin/env python3
"""Run repeatable loopback HTTP benchmarks and emit a JSON record."""
import argparse
import asyncio
import hashlib
import json
import os
import platform
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


async def serve():
    async def handle(reader, writer):
        try:
            while True:
                headers = await reader.readuntil(b"\r\n\r\n")
                close = b"connection: close" in headers.lower()
                writer.write(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nOK")
                await writer.drain()
                if close:
                    break
        except (asyncio.IncompleteReadError, ConnectionError):
            pass
        finally:
            writer.close()
            try:
                await writer.wait_closed()
            except ConnectionError:
                pass

    server = await asyncio.start_server(handle, "127.0.0.1", 0, backlog=1024)
    print(server.sockets[0].getsockname()[1], flush=True)
    async with server:
        await server.serve_forever()


def command_output(*args):
    try:
        return subprocess.check_output(args, cwd=ROOT, text=True, stderr=subprocess.DEVNULL).strip()
    except (OSError, subprocess.CalledProcessError):
        return None


def cpu_seconds(pid):
    # Linux /proc reports user and system CPU time in clock ticks.
    try:
        stat = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
        return (int(stat[11]) + int(stat[12])) / os.sysconf("SC_CLK_TCK")
    except (OSError, ValueError, IndexError):
        return None


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--server", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/release/horde")
    parser.add_argument("--binary-revision", help="revision used to build the binary, if known")
    parser.add_argument("--requests", type=int, default=10000)
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--output", type=Path, help="write JSON to this file instead of stdout")
    args = parser.parse_args()
    if args.server:
        asyncio.run(serve())
        return
    if args.requests <= 0 or args.repeats <= 0:
        parser.error("--requests and --repeats must be positive")
    binary = args.binary.resolve()
    if not binary.is_file():
        parser.error(f"binary not found: {binary}; run cargo build --release first")
    server = subprocess.Popen([sys.executable, __file__, "--server"], stdout=subprocess.PIPE, text=True)
    try:
        port = server.stdout.readline().strip()
        if not port.isdecimal():
            raise RuntimeError("benchmark server failed to start")
        target = f"http://127.0.0.1:{port}/"
        cases = [("serial", 1, []), ("concurrent", 16, []),
                 ("high_concurrency", 64, []), ("no_reuse", 16, ["--no-connection-reuse"]),
                 ("rate_limited", 16, ["--rate", "1000"])]
        results = []
        for name, concurrency, options in cases:
            command = [str(binary), "--json", "-n", str(args.requests), "-c", str(concurrency), *options, target]
            # Warm up connections, caches and executable pages; do not record the run.
            warmup = [str(binary), "--json", "-n", str(min(args.requests, 200)), "-c", str(concurrency), *options, target]
            subprocess.run(warmup, check=True, stdout=subprocess.DEVNULL)
            samples = []
            for _ in range(args.repeats):
                before = cpu_seconds(server.pid)
                started = time.monotonic()
                with tempfile.TemporaryDirectory(prefix="horde-bench-") as temp:
                    timing = Path(temp) / "time.txt"
                    time_tool = Path("/usr/bin/time")
                    measured_command = ([str(time_tool), "-f", "%U %S %M", "-o", str(timing), "--", *command]
                                        if time_tool.is_file() and sys.platform.startswith("linux") else command)
                    completed = subprocess.run(measured_command, check=True, capture_output=True, text=True)
                    if timing.exists():
                        user, system, rss = timing.read_text().split()
                        client = {"user_cpu_seconds": float(user), "system_cpu_seconds": float(system), "max_rss_kib": int(rss)}
                    else:
                        client = None
                wall = time.monotonic() - started
                after = cpu_seconds(server.pid)
                report = json.loads(completed.stdout)
                if report["completed"] != args.requests or report["failed"]:
                    raise RuntimeError(f"{name}: invalid benchmark run: {report['completed']} completed, {report['failed']} failed")
                samples.append({"wall_seconds": wall, "server_cpu_seconds": None if before is None or after is None else after - before, "client_resources": client, "report": report})
            results.append({"name": name, "concurrency": concurrency, "options": options,
                            "median_requests_per_second": statistics.median(s["report"]["requests_per_second"] for s in samples),
                            "samples": samples})
        dirty = command_output("git", "status", "--porcelain")
        result = {"schema_version": 1, "metadata": {"platform": platform.platform(), "cpu_count": os.cpu_count(),
                  "python": platform.python_version(), "rustc": command_output("rustc", "--version"),
                  "git_commit": command_output("git", "rev-parse", "HEAD"), "git_dirty": None if dirty is None else bool(dirty),
                  "binary": str(binary), "binary_revision": args.binary_revision,
                  "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
                  "requests": args.requests, "repeats": args.repeats}, "cases": results}
        output = json.dumps(result, indent=2) + "\n"
        if args.output:
            args.output.write_text(output)
        else:
            print(output, end="")
    finally:
        server.terminate()
        server.wait(timeout=5)


if __name__ == "__main__":
    main()
