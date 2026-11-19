#!/usr/bin/env python3
"""Compare release standalone and two-Grunt runs on the same loopback server."""
import argparse
from contextlib import ExitStack, contextmanager
import hashlib
import json
import os
from pathlib import Path
import platform
import select
import statistics
import subprocess
import tempfile
import time

ROOT = Path(__file__).resolve().parent.parent
FIXTURES = ROOT / "tests/fixtures/grunt-tls"
CASES = {"short": 100, "sustained": 3600}
RATE = 1800
CONCURRENCY = 16


def affinity(cpus):
    return ["taskset", "-c", cpus] if cpus else []


@contextmanager
def service(command, env, *, with_pid=False):
    process = subprocess.Popen(command, stdout=subprocess.PIPE, env=env, text=True)
    try:
        ready, _, _ = select.select([process.stdout], [], [], 10)
        if not ready or not (address := process.stdout.readline().strip()):
            raise RuntimeError(f"service did not publish an address: {command[0]}")
        yield (address, process.pid) if with_pid else address
    finally:
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=6)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
        process.stdout.close()


def identity(name):
    return [
        "--ca", str(FIXTURES / "ca.der"),
        "--cert", str(FIXTURES / f"{name}.der"),
        "--key", str(FIXTURES / f"{name}-key.der"),
    ]


def measure(command, env, requests):
    start = time.perf_counter()
    process = subprocess.run(command, capture_output=True, text=True, env=env, timeout=30)
    wall = time.perf_counter() - start
    if process.returncode:
        raise RuntimeError(f"command exited {process.returncode}: {process.stderr}\n{process.stdout}")
    report = json.loads(process.stdout)
    metrics = report.get("metrics", report)
    if report.get("partial") or (
        metrics["completed"], metrics["failed"], metrics["status_codes"], metrics["response_bytes"]
    ) != (requests, 0, {"200": requests}, 2 * requests):
        raise RuntimeError(f"unexpected benchmark result: {report}")
    return {"wall_seconds": wall, "wall_requests_per_second": requests / wall, "report": report}


def summarize(samples):
    measurements = {
        "wall_seconds": lambda s: s["wall_seconds"],
        "wall_requests_per_second": lambda s: s["wall_requests_per_second"],
        "reported_requests_per_second": lambda s: s["report"]["requests_per_second"],
        **{p: lambda s, p=p: s["report"]["latency_ms"][p] for p in ("p50", "p95", "p99")},
    }
    summary = {}
    for name, measure_sample in measurements.items():
        ratios = [100 * (measure_sample(pair["distributed"]) / measure_sample(pair["standalone"]) - 1)
                  for pair in samples]
        summary[name] = {
            mode: statistics.median(measure_sample(pair[mode]) for pair in samples)
            for mode in ("standalone", "distributed")
        } | {"paired_change_percent_median": statistics.median(ratios),
             "paired_change_percent_range": [min(ratios), max(ratios)]}
    return summary


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/release/horde")
    parser.add_argument("--pairs", type=int, default=6)
    parser.add_argument("--rates", type=int, nargs="+",
                        help="run an increasing rate sweep instead of the short and sustained cases")
    parser.add_argument("--seconds-per-step", type=float, default=1.25,
                        help="nominal run length in a sweep")
    parser.add_argument("--max-requests", type=int, default=200_000,
                        help="cap each sweep run at this budget")
    parser.add_argument("--client-cpus")
    parser.add_argument("--server-cpus")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.pairs < 2:
        parser.error("--pairs must be at least 2")
    if args.rates and (args.rates != sorted(set(args.rates)) or
                       any(rate < 2 for rate in args.rates)):
        parser.error("--rates must be strictly increasing integers of at least 2")
    if not 0.1 <= args.seconds_per_step <= 10:
        parser.error("--seconds-per-step must be between 0.1 and 10")
    if not 3000 <= args.max_requests <= 200_000:
        parser.error("--max-requests must be between 3000 and 200000")
    cases = ([(str(rate), rate, min(args.max_requests, max(3000, round(rate * args.seconds_per_step))))
              for rate in args.rates]
             if args.rates else [(name, RATE, requests) for name, requests in CASES.items()])
    binary = args.binary.resolve()
    env = {k: v for k, v in os.environ.items() if k.lower() not in {
        "http_proxy", "https_proxy", "all_proxy", "no_proxy",
    }} | {"TOKIO_WORKER_THREADS": "2"}
    rust = subprocess.check_output(["rustc", "--version"], text=True).strip()
    results = {"rustc": rust, "platform": platform.platform(), "client_cpus": args.client_cpus,
               "server_cpus": args.server_cpus, "tokio_worker_threads_per_process": 2,
               "rate": None if args.rates else RATE, "concurrency": CONCURRENCY, "pairs": args.pairs,
               "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(), "cases": {}}
    with tempfile.TemporaryDirectory(prefix="horde-distributed-bench-") as directory, ExitStack() as stack:
        server = Path(directory) / "server"
        subprocess.run(["rustc", "--edition=2024", "-O", str(ROOT / "benchmarks/server.rs"),
                        "-o", str(server)], check=True)
        results["server_sha256"] = hashlib.sha256(server.read_bytes()).hexdigest()
        port = stack.enter_context(service([*affinity(args.server_cpus), str(server)], env))
        url = f"http://127.0.0.1:{int(port)}/"
        base = [*affinity(args.client_cpus), str(binary)]
        grunts = [stack.enter_context(service([
            *base, "grunt", "--listen", "127.0.0.1:0", "--target", url,
            "--controller-cert", str(FIXTURES / "controller.der"), *identity("server"),
        ], env)) for _ in range(2)]
        for case, rate, requests in cases:
            budget = ["--requests", str(requests), "--concurrency", str(CONCURRENCY), "--rate", str(rate)]
            commands = {
                "standalone": [*base, *budget, "--timeout", "10", "--bounded-latency", "--json", url],
                "distributed": [*base, "controller", *budget, *identity("controller"),
                                "--grunt", grunts[0], "--grunt", grunts[1]],
            }
            for command in commands.values():
                measure(command, env, requests)
            samples = []
            for i in range(args.pairs):
                order = list(commands) if i % 2 == 0 else list(reversed(commands))
                samples.append({"order": order, **{
                    mode: measure(commands[mode], env, requests) for mode in order
                }})
            results["cases"][case] = {
                "rate": rate, "requests": requests, "summary": summarize(samples), "samples": samples,
            }
            args.output.write_text(json.dumps(results, indent=2) + "\n")
            print(f"{case}: {rate} starts/s, {requests} requests, {args.pairs} pairs", flush=True)


if __name__ == "__main__":
    main()
