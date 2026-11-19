#!/usr/bin/env python3
"""Measure scheduler cost with a loopback-only pacing-ablation binary."""
import argparse
from contextlib import ExitStack
import hashlib
import json
import os
from pathlib import Path
import platform
import statistics
import subprocess
import tempfile

from distributed import ROOT, FIXTURES, CONCURRENCY, affinity, identity, measure, service

REQUESTS = 32_000
RATE = 300_000
VARIANTS = {"paced": "", "reserve_only": "reserve_only", "unpaced": "unpaced"}
METRICS = {
    "wall_req_s": lambda s: s["wall_requests_per_second"],
    "reported_req_s": lambda s: s["report"]["requests_per_second"],
    **{p: lambda s, p=p: s["report"]["latency_ms"][p] for p in ("p50", "p95", "p99")},
}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--client-cpus")
    parser.add_argument("--server-cpus")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()

    binary = args.binary.resolve()
    base = [*affinity(args.client_cpus), str(binary)]
    env = {key: value for key, value in os.environ.items() if key.lower() not in {
        "http_proxy", "https_proxy", "all_proxy", "no_proxy", "horde_bench_pacing",
    }} | {"TOKIO_WORKER_THREADS": "2"}
    budget = ["--requests", str(REQUESTS), "--concurrency", str(CONCURRENCY)]
    rate = ["--rate", str(RATE)]
    results = {
        "rustc": subprocess.check_output(["rustc", "--version"], text=True).strip(),
        "platform": platform.platform(), "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "client_cpus": args.client_cpus, "server_cpus": args.server_cpus,
        "tokio_workers_per_process": 2, "requests": REQUESTS, "concurrency": CONCURRENCY,
        "paced_rate": RATE, "rounds": [],
    }
    with tempfile.TemporaryDirectory(prefix="horde-pacing-bench-") as directory, ExitStack() as stack:
        server = Path(directory) / "server"
        subprocess.run(["rustc", "--edition=2024", "-O", str(ROOT / "benchmarks/server.rs"),
                        "-o", str(server)], check=True)
        results["server_sha256"] = hashlib.sha256(server.read_bytes()).hexdigest()
        port = stack.enter_context(service([*affinity(args.server_cpus), str(server)], env))
        url = f"http://127.0.0.1:{int(port)}/"
        commands = {}
        for variant, mode in VARIANTS.items():
            trial_env = env | {"HORDE_BENCH_PACING": mode}
            grunts = [stack.enter_context(service([
                *base, "grunt", "--listen", "127.0.0.1:0", "--target", url,
                "--controller-cert", str(FIXTURES / "controller.der"), *identity("server"),
            ], trial_env)) for _ in range(2)]
            # Standalone unpaced uses the normal CLI path with no --rate.
            standalone_options = [] if variant == "unpaced" else rate
            commands[f"standalone/{variant}"] = (
                [*base, *budget, *standalone_options, "--timeout", "10", "--bounded-latency",
                 "--json", url],
                env if variant == "unpaced" else trial_env,
            )
            commands[f"distributed/{variant}"] = (
                [*base, "controller", *budget, *rate, "--grunt", grunts[0], "--grunt", grunts[1],
                 *identity("controller")], env,
            )
        for command, command_env in commands.values():
            measure(command, command_env, REQUESTS)

        order = list(commands)
        # Rotate the order so each combination is measured at every position.
        for offset in range(len(order)):
            trial_order = order[offset:] + order[:offset]
            samples = {}
            for name in trial_order:
                command, command_env = commands[name]
                samples[name] = measure(command, command_env, REQUESTS)
            results["rounds"].append({"order": trial_order, "samples": samples})
            print(f"round {offset + 1}/{len(order)}", flush=True)
        results["summary"] = {
            name: {
                metric: {
                    "median": statistics.median(values),
                    "range": [min(values), max(values)],
                }
                for metric, getter in METRICS.items()
                if (values := [getter(trial["samples"][name]) for trial in results["rounds"]])
            }
            for name in commands
        }
    args.output.write_text(json.dumps(results, indent=2) + "\n")


if __name__ == "__main__":
    main()
