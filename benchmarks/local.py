#!/usr/bin/env python3
"""Run repeatable loopback HTTP benchmarks and emit a JSON record."""
import argparse
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
    parser.add_argument("--server-binary", type=Path, help="use a prebuilt benchmark server")
    parser.add_argument("--binary", type=Path, default=ROOT / "target/release/horde")
    parser.add_argument("--binary-revision", help="revision used to build the binary, if known")
    parser.add_argument("--requests", type=int, default=10000)
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--allow-unsupported-scenarios", action="store_true", help="skip scenario cases unsupported by a comparison binary")
    parser.add_argument("--output", type=Path, help="write JSON to this file instead of stdout")
    args = parser.parse_args()
    if args.requests <= 0 or args.repeats <= 0:
        parser.error("--requests and --repeats must be positive")
    binary = args.binary.resolve()
    if not args.server and not binary.is_file():
        parser.error(f"binary not found: {binary}; run cargo build --release first")
    server_dir = tempfile.TemporaryDirectory(prefix="horde-bench-server-")
    server_binary = args.server_binary.resolve() if args.server_binary else Path(server_dir.name) / "server"
    try:
        if not args.server_binary:
            subprocess.run(["rustc", "--edition=2024", "-O", str(ROOT / "benchmarks/server.rs"), "-o", str(server_binary)], check=True)
        if args.server:
            subprocess.run([str(server_binary)], check=True)
            return
        server = subprocess.Popen([str(server_binary)], stdout=subprocess.PIPE, text=True)
        try:
            run_benchmark(args, binary, server, server_binary)
        finally:
            server.terminate()
            server.wait(timeout=5)
    finally:
        server_dir.cleanup()


def run_benchmark(args, binary, server, server_binary):
    port = server.stdout.readline().strip()
    if not port.isdecimal():
        raise RuntimeError("benchmark server failed to start")
    target = f"http://127.0.0.1:{port}/"
    with tempfile.TemporaryDirectory(prefix="horde-bench-scenarios-") as directory:
        run_cases(args, binary, server, server_binary, target, Path(directory))


def run_cases(args, binary, server, server_binary, target, directory):
    one_step = {"version": 1, "name": "one step", "variables": {"base": "<loopback>"},
                "steps": [{"name": "get", "url": "{{base}}/"}]}
    journey = {"version": 1, "name": "journey", "variables": {"base": "<loopback>"},
               "steps": [
                   {"name": "open", "url": "{{base}}/", "expect": {"body_contains": "OK"},
                    "extract": {"length": {"from": "header", "name": "Content-Length"}}},
                   {"name": "next", "url": "{{base}}/", "headers": {"X-Length": "{{length}}"}},
                   {"name": "finish", "url": "{{base}}/", "expect": {"status": [200]}}
               ]}
    cases = [("serial", 1, [], None, "/", 0), ("concurrent", 16, [], None, "/", 0),
             ("high_concurrency", 64, [], None, "/", 0), ("no_reuse", 16, ["--no-connection-reuse"], None, "/", 0),
             ("rate_limited", 16, ["--rate", "1000"], None, "/", 0),
             ("redirect_one", 16, ["-L"], None, "/redirect/1", 1),
             ("redirect_three", 16, ["-L"], None, "/redirect/3", 3),
             ("scenario_single", 16, [], one_step, "/", 0), ("scenario_journey", 16, [], journey, "/", 0),
             ("scenario_journey_bounded", 16, ["--bounded-latency"], journey, "/", 0)]
    results = []
    skipped = []
    for name, concurrency, options, scenario, path, redirects in cases:
        scenario_hash = None
        if scenario is None:
            destination = [target.rstrip("/") + path]
        else:
            scenario_hash = hashlib.sha256(json.dumps(scenario, sort_keys=True).encode()).hexdigest()
            scenario = json.loads(json.dumps(scenario))
            scenario["variables"]["base"] = target.rstrip("/")
            scenario_path = directory / f"{name}.json"
            scenario_path.write_text(json.dumps(scenario), encoding="utf-8")
            destination = ["--scenario", str(scenario_path)]
        command = [str(binary), "--json", "-n", str(args.requests), "-c", str(concurrency), *options, *destination]
        # Warm up connections, caches and executable pages; do not record the run.
        warmup_count = min(args.requests, 200)
        warmup = [str(binary), "--json", "-n", str(warmup_count), "-c", str(concurrency), *options, *destination]
        warmup_result = subprocess.run(warmup, capture_output=True, text=True)
        if warmup_result.returncode != 0:
            if scenario is not None and args.allow_unsupported_scenarios:
                skipped.append({"name": name, "reason": "base binary could not run scenario warm-up"})
                print(f"Skipping {name}: base binary could not run scenario warm-up", file=sys.stderr)
                continue
            raise RuntimeError(f"{name}: warm-up failed: {warmup_result.stderr}")
        warmup_report = json.loads(warmup_result.stdout)
        if not valid_report(warmup_report, scenario, warmup_count, redirects):
            if scenario is not None and args.allow_unsupported_scenarios:
                skipped.append({"name": name, "reason": "base binary did not complete scenario warm-up"})
                print(f"Skipping {name}: base binary did not complete scenario warm-up", file=sys.stderr)
                continue
            raise RuntimeError(f"{name}: incomplete or failed warm-up")
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
            if not valid_report(report, scenario, args.requests, redirects):
                raise RuntimeError(f"{name}: incomplete or failed benchmark run")
            rate = report["requests_per_second"] if scenario is None else args.requests / report["elapsed_seconds"]
            samples.append({"wall_seconds": wall, "rate_per_second": rate, "server_cpu_seconds": None if before is None or after is None else after - before, "client_resources": client, "report": report})
        results.append({"name": name, "concurrency": concurrency, "options": options, "path": path,
                        "unit": "requests/s" if scenario is None else "iterations/s", "scenario_sha256": scenario_hash,
                        "median_rate_per_second": statistics.median(s["rate_per_second"] for s in samples),
                        "samples": samples})
    dirty = command_output("git", "status", "--porcelain")
    result = {"schema_version": 2, "metadata": {"platform": platform.platform(), "cpu_count": os.cpu_count(),
              "python": platform.python_version(), "rustc": command_output("rustc", "--version"),
              "git_commit": command_output("git", "rev-parse", "HEAD"), "git_dirty": None if dirty is None else bool(dirty),
              "binary": str(binary), "binary_revision": args.binary_revision,
              "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
              "server_implementation": "custom" if args.server_binary else "rust-thread-pool-v2", "server_sha256": hashlib.sha256(server_binary.read_bytes()).hexdigest(),
              "requests": args.requests, "repeats": args.repeats}, "cases": results, "skipped_cases": skipped}
    output = json.dumps(result, indent=2) + "\n"
    if args.output:
        args.output.write_text(output)
    else:
        print(output, end="")


def valid_report(report, scenario, count, redirects=0):
    if scenario is None:
        return (report.get("completed") == count and report.get("failed") == 0
                and report.get("status_codes", {}).get("200") == count
                and report.get("redirects_followed") == count * redirects)
    totals = report.get("scenarios", {})
    steps = report.get("steps", [])
    return (totals.get("completed") == count and totals.get("failed") == 0
            and len(steps) == len(scenario["steps"])
            and all(step.get("completed") == count and step.get("failed") == 0 for step in steps)
            and report.get("elapsed_seconds", 0) > 0)

if __name__ == "__main__":
    main()
