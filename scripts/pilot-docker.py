#!/usr/bin/env python3
"""Run the distributed pilot against two container-local HTTP fixtures."""

import argparse
from contextlib import ExitStack
import ipaddress
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tarfile
import tempfile
import time
import uuid


ROOT = Path(__file__).resolve().parents[1]
GRUNTS = ("grunt-a", "grunt-b")
IDENTITY = ["--ca", "/identity/ca.der", "--cert", "/identity/cert.der",
            "--key", "/identity/key.der"]
LABEL = "io.horde-rs.docker-pilot"


def run(*args, timeout=20, check=True):
    result = subprocess.run(args, capture_output=True, text=True, timeout=timeout)
    if check and result.returncode:
        raise RuntimeError(f"{' '.join(map(str, args))} failed ({result.returncode}):\n"
                           f"{result.stdout}{result.stderr}")
    return result


def docker(*args, **options):
    return run("docker", *args, **options)


def remove(*args):
    result = docker(*args, check=False)
    if result.returncode and "no such" not in result.stderr.lower() and "not found" not in result.stderr.lower():
        raise RuntimeError(f"Cleanup failed for {args[-1]}: {result.stderr.strip()}")


def build_image(work, image, binary):
    rootfs = work / "rootfs"
    rootfs.mkdir()
    shutil.copy2(binary, rootfs / "horde")
    run("rustc", "--edition=2024", "-O", ROOT / "scripts/pilot-http.rs",
        "-o", rootfs / "pilot-http", timeout=60)

    # Use the host build's own glibc and loader. Nothing needs to be pulled.
    for executable in (rootfs / "horde", rootfs / "pilot-http"):
        libraries = run("ldd", executable, check=False)
        if "not a dynamic executable" in libraries.stderr or "statically linked" in libraries.stdout:
            continue
        if libraries.returncode or "not found" in libraries.stdout:
            raise RuntimeError(f"Cannot resolve libraries for {executable}: "
                               f"{libraries.stdout}{libraries.stderr}")
        for line in libraries.stdout.splitlines():
            path = line.split("=>", 1)[-1].strip().split()[0]
            if path.startswith("/"):
                destination = rootfs / path.lstrip("/")
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(path, destination)

    archive = work / "rootfs.tar"
    with tarfile.open(archive, "w") as layer:
        for path in rootfs.iterdir():
            layer.add(path, arcname=path.name)
    docker("image", "import", "--change", f"LABEL {LABEL}=true", archive, image, timeout=120)


def allocate_peers(network):
    ipam = json.loads(docker("network", "inspect", network).stdout)[0]["IPAM"]["Config"]
    private_ranges = [ipaddress.ip_network(cidr) for cidr in
                      ("10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16")]
    for pool in ipam:
        subnet = ipaddress.ip_network(pool["Subnet"])
        if subnet.version != 4 or subnet.num_addresses < 8:
            continue
        if any(subnet.subnet_of(private) for private in private_ranges):
            hosts = (str(ip) for ip in subnet.hosts() if str(ip) != pool.get("Gateway"))
            return str(subnet), {role: next(hosts) for role in ("controller", *GRUNTS)}
    raise RuntimeError("Docker must allocate a private IPv4 subnet with room for three endpoints.")


def issue_certificates(work, peers):
    def tls(*args):
        run(sys.executable, ROOT / "scripts/pilot-tls.py", *args)

    issuer = work / "issuer"
    tls("init-ca", issuer)
    for role, ip in peers.items():
        identity = work / role
        tls("request", role, identity)
        san = [] if role == "controller" else ["--ip", ip]
        tls("issue", role, *san, "--ca-dir", issuer, "--csr", identity / "request.pem",
            "--out", identity / "cert.der")
        shutil.copyfile(issuer / "ca.der", identity / "ca.der")
        if role != "controller":
            shutil.copyfile(work / "controller" / "cert.der", identity / "controller.der")


def wait_for(condition, description, timeout=10):
    deadline = time.monotonic() + timeout
    while not condition():
        if time.monotonic() >= deadline:
            raise RuntimeError(f"Timed out waiting for {description}")
        time.sleep(0.1)


def require(condition, description, detail):
    if not condition:
        raise RuntimeError(f"{description}: {detail}")


def pilot(binary):
    prefix = f"horde-pilot-{uuid.uuid4().hex[:12]}"
    image = f"{prefix}:local"
    with tempfile.TemporaryDirectory(prefix=prefix + "-") as directory, ExitStack() as cleanup:
        work = Path(directory)
        cleanup.callback(remove, "image", "rm", image)
        build_image(work, image, binary)
        cleanup.callback(remove, "network", "rm", prefix)
        docker("network", "create", "--internal", "--label", LABEL, prefix)
        subnet, peers = allocate_peers(prefix)
        issue_certificates(work, peers)
        addresses = {role: f"{peers[role]}:7071" for role in GRUNTS}
        inventory = {"grunts": [{"address": addresses[role]} for role in GRUNTS]}
        wrong_identity = "wrong-grunt.invalid"
        wrong_inventory = {"grunts": [inventory["grunts"][0], {
            "address": addresses["grunt-b"], "tls_identity": wrong_identity,
        }]}
        for file, data in (("grunts.json", inventory), ("wrong-grunts.json", wrong_inventory)):
            (work / "controller" / file).write_text(json.dumps(data))
        containers = []

        def start(name, role, entrypoint, *args):
            container = f"{prefix}-{name}"
            containers.append(container)
            cleanup.callback(remove, "rm", "-f", container)
            docker("run", "-d", "--pull=never", "--name", container, "--label", LABEL,
                   "--network", prefix, "--ip", peers[role], "--read-only", "--cap-drop", "ALL",
                   "--security-opt", "no-new-privileges", "--user", f"{os.getuid()}:{os.getgid()}",
                   "--mount", f"type=bind,source={work / role},destination=/identity,readonly",
                   "--entrypoint", entrypoint, image, *args)

        def logs(name):
            return docker("logs", f"{prefix}-{name}")

        def requests(role):
            return logs(role).stdout.splitlines().count("pilot-http: 200")

        def start_controller(name, budget):
            start(name, "controller", "/horde", "controller", "--private-network",
                  "--inventory", "/identity/grunts.json", *IDENTITY,
                  "--requests", str(budget), "--concurrency", "2", "--rate", "4",
                  "--deadline", "30", "--max-failure-rate", "0")

        def collect(name, expected_exit):
            container = f"{prefix}-{name}"
            exit_code = docker("wait", container, timeout=40).stdout.strip()
            result = logs(name)
            require(exit_code == str(expected_exit), f"{name} exited with {exit_code}",
                    result.stdout + result.stderr)
            remove("rm", container)
            return json.loads(result.stdout)

        def preflight(name, inventory_file, ready):
            start(name, "controller", "/horde", "preflight", "--private-network",
                  "--inventory", f"/identity/{inventory_file}", *IDENTITY)
            report = collect(name, 0 if ready else 1)
            require(report["type"] == "distributed_preflight" and report["ready"] is ready,
                    "Unexpected preflight result", report)
            require([(grunt["address"], grunt["status"]) for grunt in report["grunts"]] == [
                (addresses["grunt-a"], "ready"),
                (addresses["grunt-b"], "ready" if ready else "failed"),
            ], "Unexpected preflight peer states", report)
            connections = {role: logs(role).stdout.splitlines().count("pilot-http: connection")
                           for role in GRUNTS}
            require(connections == dict.fromkeys(GRUNTS, 0),
                    "Preflight reached a workload target", connections)
            return {"report": report, "target_connections": connections}

        def verify(report, completed, states, partial):
            metrics = report["metrics"]
            require(report["partial"] is partial, "Unexpected partial status", report)
            require({agent["address"]: agent["status"] for agent in report["agents"]}
                    == dict(zip(addresses.values(), states)), "Unexpected Grunt states", report)
            require(metrics["started"] == metrics["completed"] == completed and metrics["failed"] == 0
                    and metrics["response_bytes"] == 2 * completed
                    and metrics["status_codes"] == {"200": completed}, "Unexpected totals", metrics)
            require(0 < report["latency_ms"]["p95"] <= report["latency_ms"]["p99"],
                    "Unexpected merged percentiles", report["latency_ms"])

        try:
            for role in GRUNTS:
                start(role, role, "/pilot-http", "grunt", "--listen", addresses[role],
                      "--allow-controller-ip", peers["controller"], "--target", "http://127.0.0.1:18080/",
                      "--controller-cert", "/identity/controller.der", *IDENTITY)
                wait_for(lambda: addresses[role] in logs(role).stdout.splitlines(), f"{role} readiness")

            checked = preflight("preflight", "grunts.json", ready=True)
            rejected = preflight("preflight-wrong-identity", "wrong-grunts.json", ready=False)
            failed = rejected["report"]["grunts"][1]
            require(failed["tls_identity"] == wrong_identity and "certificate" in (failed["error"] or ""),
                    "Expected Grunt B certificate rejection", failed)
            print("Preflight passed and identified the misconfigured Grunt; no target connections.",
                  flush=True)

            start_controller("nominal", 20)
            nominal = collect("nominal", 0)
            counts = {role: requests(role) for role in GRUNTS}
            verify(nominal, 20, ("complete", "complete"), partial=False)
            require(counts == {"grunt-a": 10, "grunt-b": 10}, "Unexpected nominal split", counts)
            print(f"Nominal run passed: {counts}", flush=True)

            start_controller("disconnect", 40)
            wait_for(lambda: requests("grunt-b") > counts["grunt-b"], "Grunt B workload")
            docker("network", "disconnect", prefix, f"{prefix}-grunt-b")
            fault = collect("disconnect", 1)
            after_drop = {role: requests(role) - counts[role] for role in GRUNTS}
            verify(fault, 20, ("complete", "missing"), partial=True)
            require(fault["thresholds"]["failure_rate"] == "not_evaluable",
                    "Unexpected partial threshold result", fault["thresholds"])
            require(after_drop["grunt-a"] == 20 and 0 < after_drop["grunt-b"] < 20,
                    "Unexpected split after disconnect", after_drop)
            print(f"Link disconnect passed: {after_drop}", flush=True)
        except Exception:
            for container in containers:
                result = docker("logs", container, check=False)
                if result.returncode == 0:
                    print(f"--- {container} ---\n{result.stdout}{result.stderr}", file=sys.stderr)
            raise

        return {
            "subnet": subnet,
            "roles": peers,
            "preflight": {"nominal": checked, "wrong_identity": rejected},
            "nominal": {"controller_report": nominal, "requests_per_target": counts},
            "link_disconnect": {"controller_report": fault, "requests_per_target": after_drop},
        }


def interrupt(signum, _frame):
    raise KeyboardInterrupt(signal.Signals(signum).name)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/release/horde")
    parser.add_argument("--output", type=Path, default=ROOT / "target/pilot-docker-report.json")
    args = parser.parse_args()
    if not args.binary.is_file():
        parser.error("build horde with cargo build --release --locked --features distributed-transport")
    signal.signal(signal.SIGTERM, interrupt)
    try:
        report = pilot(args.binary.resolve())
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(report, indent=2) + "\n")
    except KeyboardInterrupt as error:
        print(f"Pilot interrupted {error}", file=sys.stderr)
        return 130
    except (OSError, RuntimeError, subprocess.SubprocessError, ValueError) as error:
        print(f"Pilot failed: {error}", file=sys.stderr)
        return 1
    print(f"Pilot report: {args.output}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
