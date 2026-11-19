#!/usr/bin/env python3
"""Compare per-request CPU time for two loopback pacing implementations on Linux."""
import argparse
from contextlib import ExitStack
import hashlib
import json
import os
from pathlib import Path
import platform
import resource
import statistics
import subprocess
import tempfile

from distributed import CONCURRENCY, FIXTURES, ROOT, affinity, identity, measure, service

RATES = (2_000, 4_000, 8_000)
TICKS_PER_SECOND = os.sysconf("SC_CLK_TCK")


def child_cpu():
    usage = resource.getrusage(resource.RUSAGE_CHILDREN)
    return usage.ru_utime, usage.ru_stime


def process_cpu(pid):
    # The process name can contain spaces and parentheses. Field 3 follows its last ')'.
    fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
    return tuple(int(fields[i]) / TICKS_PER_SECOND for i in (11, 12))


def delta(before, after):
    return {"user_seconds": after[0] - before[0], "system_seconds": after[1] - before[1]}


def cpu_trial(command, env, requests, grunt_pids, server_pid):
    before_server = process_cpu(server_pid)
    before_grunts = [process_cpu(pid) for pid in grunt_pids]
    before_command = child_cpu()
    result = measure(command, env, requests)
    command_cpu = delta(before_command, child_cpu())
    grunt_cpu = [delta(before, process_cpu(pid)) for before, pid in zip(before_grunts, grunt_pids)]
    server_cpu = delta(before_server, process_cpu(server_pid))
    components = [command_cpu, *grunt_cpu]
    result["cpu"] = {
        "command": command_cpu, "grunts": grunt_cpu, "http_server": server_cpu,
        "total_user_seconds": sum(c["user_seconds"] for c in components),
        "total_system_seconds": sum(c["system_seconds"] for c in components),
    }
    total = result["cpu"]["total_user_seconds"] + result["cpu"]["total_system_seconds"]
    result["cpu"]["client_microseconds_per_request"] = total * 1_000_000 / requests
    result["cpu"]["client_average_cores"] = total / result["wall_seconds"]
    return result


def summarize(rounds):
    metrics = {
        "wall_requests_per_second": lambda s: s["wall_requests_per_second"],
        "cpu_us_per_request": lambda s: s["cpu"]["client_microseconds_per_request"],
        "average_cores": lambda s: s["cpu"]["client_average_cores"],
        "p95_ms": lambda s: s["report"]["latency_ms"]["p95"],
        "p99_ms": lambda s: s["report"]["latency_ms"]["p99"],
        "user_seconds": lambda s: s["cpu"]["total_user_seconds"],
        "system_seconds": lambda s: s["cpu"]["total_system_seconds"],
        "command_cpu_seconds": lambda s: sum(s["cpu"]["command"].values()),
        "grunt_cpu_seconds": lambda s: sum(sum(g.values()) for g in s["cpu"]["grunts"]),
        "server_cpu_seconds": lambda s: sum(s["cpu"]["http_server"].values()),
    }
    result = {}
    for name in rounds[0]["samples"]:
        result[name] = {}
        for metric, getter in metrics.items():
            values = [getter(r["samples"][name]) for r in rounds]
            result[name][metric] = {"median": statistics.median(values), "range": [min(values), max(values)]}
    return result


def parse_case(value):
    try:
        workers, rate = map(int, value.split(":"))
    except ValueError:
        raise argparse.ArgumentTypeError("expected WORKERS:RATE") from None
    if workers not in range(2, 17, 2) or rate < 2:
        raise argparse.ArgumentTypeError("expected even worker count 2–16 and rate at least 2")
    return workers, rate


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--before", type=Path, required=True)
    parser.add_argument("--after", type=Path, required=True)
    parser.add_argument("--client-cpus")
    parser.add_argument("--server-cpus")
    parser.add_argument("--output", type=Path, required=True)
    sweep = parser.add_mutually_exclusive_group()
    sweep.add_argument("--rates", type=int, nargs="+", default=RATES)
    sweep.add_argument("--worker-matrix", type=int, nargs="+",
                       help="test 80%%, 100%% and 120%% of 500 starts/s per worker")
    sweep.add_argument("--case", action="append", type=parse_case, metavar="WORKERS:RATE",
                       help="test an explicit concurrency and rate; repeat to add cases")
    parser.add_argument("--timer-reference", type=Path,
                        help="optional timer-only binary tested above the cutoff in a worker matrix")
    args = parser.parse_args()

    if any(rate < 2 for rate in args.rates) or sorted(set(args.rates)) != list(args.rates):
        parser.error("--rates must be strictly increasing integers of at least 2")

    if args.worker_matrix and (any(count not in range(4, 17, 2) for count in args.worker_matrix)
                               or sorted(set(args.worker_matrix)) != args.worker_matrix):
        parser.error("--worker-matrix needs increasing even worker counts between 4 and 16")
    if args.timer_reference and not args.worker_matrix:
        parser.error("--timer-reference needs --worker-matrix")
    cases = ([(workers, 500 * workers * percent // 100, percent)
              for workers in args.worker_matrix for percent in (80, 100, 120)]
             if args.worker_matrix else [(CONCURRENCY, rate, None) for rate in args.rates])
    if args.case:
        if len(set(args.case)) != len(args.case):
            parser.error("--case entries must be unique")
        cases = [(workers, rate, None) for workers, rate in args.case]
    binaries = {"before": args.before.resolve(), "after": args.after.resolve()}
    if args.timer_reference:
        binaries["timer"] = args.timer_reference.resolve()
    env = {k: v for k, v in os.environ.items() if k.lower() not in {
        "http_proxy", "https_proxy", "all_proxy", "no_proxy", "horde_bench_pacing",
    }} | {"TOKIO_WORKER_THREADS": "2"}
    results = {
        "platform": platform.platform(), "client_cpus": args.client_cpus,
        "server_cpus": args.server_cpus, "ticks_per_second": TICKS_PER_SECOND,
        "tokio_workers_per_process": 2,
        "concurrency": None if args.worker_matrix or args.case else CONCURRENCY,
        "binaries_sha256": {name: hashlib.sha256(path.read_bytes()).hexdigest()
                            for name, path in binaries.items()},
        "cases": {},
    }

    with tempfile.TemporaryDirectory(prefix="horde-pacer-cpu-") as directory, ExitStack() as stack:
        server = Path(directory) / "server"
        subprocess.run(["rustc", "--edition=2024", "-O", str(ROOT / "benchmarks/server.rs"),
                        "-o", str(server)], check=True)
        results["server_sha256"] = hashlib.sha256(server.read_bytes()).hexdigest()
        port, server_pid = stack.enter_context(service([*affinity(args.server_cpus), str(server)], env, with_pid=True))
        url = f"http://127.0.0.1:{int(port)}/"
        grunts = {}
        for name, path in binaries.items():
            grunts[name] = [stack.enter_context(service([
                *affinity(args.client_cpus), str(path), "grunt", "--listen", "127.0.0.1:0", "--target", url,
                "--controller-cert", str(FIXTURES / "controller.der"), *identity("server"),
            ], env, with_pid=True)) for _ in range(2)]

        def commands(rate, requests, workers, include_timer=False):
            budget = ["--requests", str(requests), "--concurrency", str(workers), "--rate", str(rate)]
            variants = {}
            for name, path in binaries.items():
                if name == "timer" and not include_timer:
                    continue
                base = [*affinity(args.client_cpus), str(path)]
                variants[f"standalone/{name}"] = (
                    [*base, *budget, "--timeout", "10", "--bounded-latency", "--json", url], []
                )
                addresses, pids = zip(*grunts[name])
                variants[f"distributed/{name}"] = (
                    [*base, "controller", *budget, *identity("controller"),
                     "--grunt", addresses[0], "--grunt", addresses[1]], pids
                )
            return variants

        for command, _ in commands(RATES[1], RATES[1], CONCURRENCY, bool(args.timer_reference)).values():
            measure(command, env, RATES[1])

        for workers, rate, percent in cases:
            requests = min(16_000, rate if args.worker_matrix or args.case else 2 * rate)
            variants = commands(rate, requests, workers, include_timer=percent == 120)
            order = list(variants)
            rounds = []
            # Alternate forward/reverse order and swap halves for the second pair.
            half = len(order) // 2
            shifted = order[half:] + order[:half]
            for trial_order in (order, order[::-1], shifted, shifted[::-1]):
                samples = {
                    name: cpu_trial(variants[name][0], env, requests, variants[name][1], server_pid)
                    for name in trial_order
                }
                rounds.append({"order": trial_order, "samples": samples})
            case_id = f"c{workers}-r{rate}" if args.worker_matrix or args.case else str(rate)
            results["cases"][case_id] = {
                "rate": rate, "workers": workers, "cutoff_percent": percent, "requests": requests,
                "rounds": rounds, "summary": summarize(rounds),
            }
            args.output.write_text(json.dumps(results, indent=2) + "\n")
            print(f"{workers} workers at {rate}: {requests} requests, {len(rounds)} rounds", flush=True)


if __name__ == "__main__":
    main()
