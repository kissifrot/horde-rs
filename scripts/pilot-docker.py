#!/usr/bin/env python3
"""Run the distributed pilot against two container-local HTTP fixtures."""

import argparse
from contextlib import ExitStack
from datetime import datetime, timezone
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
    fixtures = ("pilot-http", "pilot-relay")
    for fixture in fixtures:
        run("rustc", "--edition=2024", "-O", ROOT / "scripts" / f"{fixture}.rs",
            "-o", rootfs / fixture, timeout=60)

    # Use the host build's own glibc and loader. Nothing needs to be pulled.
    for name in ("horde", *fixtures):
        executable = rootfs / name
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
            return str(subnet), {role: next(hosts) for role in ("controller", *GRUNTS, "relay")}
    raise RuntimeError("Docker must allocate a private IPv4 subnet with room for four endpoints.")


def tls_config(leaf, roots):
    return {
        "cert": f"{leaf}/cert.der",
        "key": f"{leaf}/key.der",
        "ca": [f"{root}-ca.der" for root in roots],
    }


def issue_certificates(work, addresses, inventory_file):
    def tls(*args):
        run(sys.executable, ROOT / "scripts/pilot-tls.py", *args)

    roles = ("controller", *GRUNTS)
    for generation in ("old", "new"):
        issuer = work / f"{generation}-issuer"
        tls("init-ca", issuer)
        for role in roles:
            # The pilot uses isolated role directories to stand in for separate hosts.
            role_dir = work / role
            identity = role_dir / generation
            if role == "controller":
                tls("request", role, identity)
                sign = ("issue", role)
            else:
                selection = ("--inventory", inventory_file, "--address", addresses[role])
                tls("request-grunt", identity, *selection)
                sign = ("issue-grunt", *selection)
            csr = issuer / f"{role}.req.pem"
            signed_leaf = issuer / f"{role}.der"
            shutil.copyfile(identity / "request.pem", csr)
            tls(*sign, "--ca-dir", issuer, "--csr", csr, "--out", signed_leaf)
            shutil.copyfile(signed_leaf, identity / "cert.der")
            shutil.copyfile(issuer / "ca.der", role_dir / f"{generation}-ca.der")
            if role != "controller":
                shutil.copyfile(issuer / "controller.der", role_dir / f"{generation}-controller.der")


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
        addresses = {role: f"{peers[role]}:7071" for role in GRUNTS}
        inventory = {"grunts": [{"address": addresses["grunt-a"]}, {
            "address": addresses["grunt-b"], "tls_identity": "grunt-b.pilot.test",
        }]}
        for role in peers:
            (work / role).mkdir(mode=0o700)
        inventory_file = work / "controller" / "grunts.json"
        wrong_identity = "wrong-grunt.invalid"
        wrong_inventory = {"grunts": [inventory["grunts"][0], {
            "address": addresses["grunt-b"], "tls_identity": wrong_identity,
        }]}
        relay_address = f"{peers['relay']}:7071"
        relay_inventory = {"grunts": [inventory["grunts"][0], {
            **inventory["grunts"][1], "address": relay_address,
        }]}
        for file, data in (("grunts.json", inventory), ("wrong-grunts.json", wrong_inventory),
                           ("relay-grunts.json", relay_inventory)):
            (work / "controller" / file).write_text(json.dumps(data))
        issue_certificates(work, addresses, inventory_file)
        containers = []
        grunt_names = {}

        def start(name, role, entrypoint, *args, source_ip=None):
            container = f"{prefix}-{name}"
            containers.append(container)
            cleanup.callback(remove, "rm", "-f", container)
            docker("run", "-d", "--pull=never", "--name", container, "--label", LABEL,
                   "--network", prefix, "--ip", source_ip or peers[role], "--read-only", "--cap-drop", "ALL",
                   "--security-opt", "no-new-privileges", "--user", f"{os.getuid()}:{os.getgid()}",
                   "--mount", f"type=bind,source={work / role},destination=/identity,readonly",
                   "--entrypoint", entrypoint, image, *args)

        def logs(name, *options):
            return docker("logs", *options, f"{prefix}-{name}")

        def timestamps(name, message, stream="stdout"):
            found = []
            for line in getattr(logs(name, "--timestamps"), stream).splitlines():
                timestamp, _, content = line.partition(" ")
                if content == message:
                    # Docker's RFC 3339 log timestamps have nanosecond precision.
                    base, _, fraction = timestamp.removesuffix("Z").partition(".")
                    found.append(datetime.fromisoformat(f"{base}.{fraction[:6]:0<6}+00:00"))
            return found

        def requests(role):
            return logs(grunt_names[role]).stdout.splitlines().count("pilot-http: 200")

        def target_connections():
            return {role: logs(grunt_names[role]).stdout.splitlines().count("pilot-http: connection")
                    for role in GRUNTS}

        def config_args(role, filename, settings):
            (work / role / filename).write_text(json.dumps(settings))
            return "--config", f"/identity/{filename}"

        def controller_config(leaf, roots, inventory_file):
            filename = f"controller-{leaf}-{'-'.join(roots)}-{inventory_file}"
            return config_args("controller", filename, {
                "inventory": inventory_file,
                "private_network": True,
                "tls": tls_config(leaf, roots),
            })

        def launch_grunt(role, name, config):
            grunt_names[role] = name
            start(name, role, "/pilot-http", "grunt", *config)
            wait_for(lambda: addresses[role] in logs(name).stdout.splitlines(), f"{name} readiness")

        def start_grunts(stage, leaves, roots, controllers, limits=None, controller_ips=None):
            for name in grunt_names.values():
                remove("rm", "-f", f"{prefix}-{name}")
            configs = {}
            for role, leaf in zip(GRUNTS, leaves):
                configs[role] = config_args(role, f"{stage}.json", {
                    "listen": addresses[role],
                    "allow_controller_ip": [(controller_ips or {}).get(role, peers["controller"])],
                    "target": "http://127.0.0.1:18080/",
                    "controller_cert": [f"{controller}-controller.der" for controller in controllers],
                    "tls": tls_config(leaf, roots),
                    **(limits or {}).get(role, {}),
                })
                launch_grunt(role, f"{stage}-{role}", configs[role])
            return configs

        def start_controller(name, budget, inventory_file="grunts.json"):
            start(name, "controller", "/horde", "controller",
                  *controller_config("new", ("new",), inventory_file),
                  "--requests", str(budget), "--concurrency", "2", "--rate", "4",
                  "--deadline", "30", "--max-failure-rate", "0")

        def collect_output(name, expected_exit):
            container = f"{prefix}-{name}"
            exit_code = docker("wait", container, timeout=40).stdout.strip()
            result = logs(name)
            require(exit_code == str(expected_exit), f"{name} exited with {exit_code}",
                    result.stdout + result.stderr)
            remove("rm", container)
            return result.stdout

        def collect(name, expected_exit):
            return json.loads(collect_output(name, expected_exit))

        def preflight(name, leaf, roots, states, inventory_file="grunts.json", *, source_ip=None):
            before = target_connections()
            start(name, "controller", "/horde", "preflight",
                  *controller_config(leaf, roots, inventory_file), source_ip=source_ip)
            ready = all(status == "ready" for status in states)
            report = collect(name, 0 if ready else 1)
            require(report["type"] == "distributed_preflight" and report["ready"] is ready,
                    "Unexpected preflight result", report)
            require([(grunt["address"], grunt["status"]) for grunt in report["grunts"]]
                    == list(zip(addresses.values(), states)), "Unexpected preflight peer states", report)
            connections = {role: total - before[role] for role, total in target_connections().items()}
            require(connections == dict.fromkeys(GRUNTS, 0),
                    "Preflight reached a workload target", connections)
            return {"report": report, "target_connections": connections}

        def verify(report, completed, states, partial, agent_addresses=None):
            metrics = report["metrics"]
            expected_agents = addresses.values() if agent_addresses is None else agent_addresses
            require(report["partial"] is partial, "Unexpected partial status", report)
            require({agent["address"]: agent["status"] for agent in report["agents"]}
                    == dict(zip(expected_agents, states)), "Unexpected Grunt states", report)
            require(metrics["started"] == metrics["completed"] == completed and metrics["failed"] == 0
                    and metrics["response_bytes"] == 2 * completed
                    and metrics["status_codes"] == {"200": completed}, "Unexpected totals", metrics)
            require(0 < report["latency_ms"]["p95"] <= report["latency_ms"]["p99"],
                    "Unexpected merged percentiles", report["latency_ms"])

        def controller_stop(signal_name, expected_exit, latest_request_limit):
            name = f"controller-{signal_name.lower()}"
            start_grunts(name, ("new", "new"), ("new",), ("new",))
            start_controller(name, 40)
            wait_for(lambda: all(target_connections().values()), "workload on both Grunts", timeout=5)
            signal_sent_at = datetime.now(timezone.utc)
            observe_until = time.monotonic() + 7.0
            docker("kill", "--signal", signal_name, f"{prefix}-{name}")
            output = collect_output(name, expected_exit)
            if signal_name == "SIGINT":
                report = json.loads(output)
                require(report["partial"] and report["interruption"] == "sigint"
                        and report["thresholds"]["failure_rate"] == "not_evaluable"
                        and {agent["address"]: agent["status"] for agent in report["agents"]}
                        == dict.fromkeys(addresses.values(), "cancelled"),
                        "Expected both Grunts to acknowledge cancellation after SIGINT", report)
            else:
                require(not output.strip(), "Killed controller unexpectedly emitted a report", output)
                report = None

            # Observe targets independently of the controller's exit status and stdout.
            time.sleep(max(0.0, observe_until - time.monotonic()))
            observed_at = datetime.now(timezone.utc)
            targets = {}
            for role in GRUNTS:
                arrivals = timestamps(grunt_names[role], "pilot-http: connection")
                require(0 < len(arrivals) < 20 and arrivals[0] <= signal_sent_at,
                        f"{role} must start but leave part of its budget unused", len(arrivals))
                last_request = (arrivals[-1] - signal_sent_at).total_seconds()
                quiet_seconds = (observed_at - arrivals[-1]).total_seconds()
                require(last_request <= latest_request_limit and quiet_seconds >= 3.0,
                        f"{role} did not stop within the expected time", (last_request, quiet_seconds))
                targets[role] = {
                    "target_connections": len(arrivals),
                    "last_request_seconds_from_signal": last_request,
                    "quiet_seconds": quiet_seconds,
                }
            ready = preflight(f"{name}-preflight", "new", ("new",), ("ready", "ready"))
            stopped_counts = {role: target["target_connections"] for role, target in targets.items()}
            require(target_connections() == stopped_counts,
                    "A Grunt resumed workload during the post-stop preflight", targets)
            print(f"{signal_name} passed: both Grunts stopped at {stopped_counts}.", flush=True)
            return {"exit_code": expected_exit, "controller_report": report, "targets": targets,
                    "post_stop_preflight": ready}

        def inventory_changes():
            inventory_file = "managed-grunts.json"
            _, config = controller_config("new", ("new",), inventory_file)
            command = (binary, "inventory", "--config", work / "controller" / Path(config).name)
            run(*command, "add", addresses["grunt-a"])
            start_controller("inventory-first", 12, inventory_file)
            wait_for(lambda: requests("grunt-a") > 0, "inventory run before adding Grunt B")
            run(*command, "add", f"{addresses['grunt-b']}=grunt-b.pilot.test")
            require(docker("inspect", "--format", "{{.State.Running}}", f"{prefix}-inventory-first")
                    .stdout.strip() == "true", "Expected the first run to still be active", inventory_file)
            listed = json.loads(run(*command, "list").stdout)
            require([peer["address"] for peer in listed["grunts"]] == list(addresses.values()),
                    "Inventory order changed", listed)
            first = collect("inventory-first", 0)
            verify(first, 12, ("complete",), partial=False, agent_addresses=(addresses["grunt-a"],))
            require(target_connections() == {"grunt-a": 12, "grunt-b": 0},
                    "Inventory addition changed an active run", target_connections())

            start_controller("inventory-next", 4, inventory_file)
            following = collect("inventory-next", 0)
            verify(following, 4, ("complete", "complete"), partial=False)
            require(target_connections() == {"grunt-a": 14, "grunt-b": 2},
                    "Next run did not use the added peer", target_connections())

            run(*command, "remove", addresses["grunt-a"])
            start_controller("inventory-removed", 2, inventory_file)
            removed = collect("inventory-removed", 0)
            verify(removed, 2, ("complete",), partial=False, agent_addresses=(addresses["grunt-b"],))
            require(target_connections() == {"grunt-a": 14, "grunt-b": 4},
                    "Removed peer received traffic", target_connections())
            print("Inventory changes passed: the active run kept its peers and later runs picked up edits.",
                  flush=True)
            return {"during_addition": first, "after_addition": following, "after_removal": removed}

        try:
            both_ready = ("ready", "ready")
            both_failed = ("failed", "failed")
            both_roots = ("old", "new")
            start_grunts("initial", ("old", "old"), ("old",), ("old",))
            checked = preflight("preflight", "old", ("old",), both_ready)
            # Use the relay's reserved address before the relay starts. Keep the controller's TLS files.
            unlisted_ip = peers["relay"]
            source_denied = preflight("preflight-unlisted-source", "old", ("old",),
                                      both_failed, source_ip=unlisted_ip)
            for grunt in source_denied["report"]["grunts"]:
                error = grunt["error"] or ""
                require(grunt["epoch"] is None
                        and (error == "tls handshake eof" or error.startswith("Connection reset by peer")),
                        "Expected unlisted source to be disconnected before TLS hello", grunt)
            print(f"Unlisted source {unlisted_ip} denied by both Grunts; no target connections.",
                  flush=True)
            rejected = preflight("preflight-wrong-identity", "old", ("old",),
                                 ("ready", "failed"), "wrong-grunts.json")
            failed = rejected["report"]["grunts"][1]
            require(failed["tls_identity"] == wrong_identity and "certificate" in (failed["error"] or ""),
                    "Expected Grunt B certificate rejection", failed)
            print("Preflight passed and identified the misconfigured Grunt; no target connections.",
                  flush=True)

            start_grunts("overlap", ("new", "old"), both_roots, both_roots)
            rotation = {
                "overlap_old_controller": preflight("overlap-old", "old", both_roots, both_ready),
                "overlap_new_controller": preflight("overlap-new", "new", both_roots, both_ready),
            }
            # Keeping both roots proves that removing a controller leaf alone denies it.
            start_grunts("allowlist", ("new", "new"), both_roots, ("new",))
            rotation["old_leaf_removed"] = preflight("old-leaf-removed", "old", both_roots, both_failed)
            rotation["new_leaf_allowed"] = preflight("new-leaf-allowed", "new", both_roots, both_ready)

            start_grunts("rotated", ("new", "new"), ("new",), ("new",))
            rotation["new_ca_only"] = preflight("new-ca-only", "new", ("new",), both_ready)
            rotation["old_ca_only"] = preflight("old-ca-only", "new", ("old",), both_failed)
            rotation["old_controller_after_rotation"] = preflight(
                "old-controller-after-rotation", "old", ("new",), both_failed
            )
            print("Two-CA rotation passed; retired controller leaf and old trust were refused.", flush=True)

            start_controller("nominal", 20)
            nominal = collect("nominal", 0)
            counts = {role: requests(role) for role in GRUNTS}
            verify(nominal, 20, ("complete", "complete"), partial=False)
            require(counts == {"grunt-a": 10, "grunt-b": 10}, "Unexpected nominal split", counts)
            print(f"Nominal run passed: {counts}", flush=True)

            start_controller("disconnect", 40)
            wait_for(lambda: requests("grunt-b") > counts["grunt-b"], "Grunt B workload")
            docker("network", "disconnect", prefix, f"{prefix}-{grunt_names['grunt-b']}")
            fault = collect("disconnect", 1)
            after_drop = {role: requests(role) - counts[role] for role in GRUNTS}
            verify(fault, 20, ("complete", "missing"), partial=True)
            require(fault["thresholds"]["failure_rate"] == "not_evaluable",
                    "Unexpected partial threshold result", fault["thresholds"])
            require(after_drop["grunt-a"] == 20 and 0 < after_drop["grunt-b"] < 20,
                    "Unexpected split after disconnect", after_drop)
            print(f"Link disconnect passed: {after_drop}", flush=True)

            start_grunts("silent", ("new", "new"), ("new",), ("new",),
                         controller_ips={"grunt-b": peers["relay"]})
            start("silent-relay", "relay", "/pilot-relay", relay_address, addresses["grunt-b"])
            wait_for(lambda: "pilot-relay: ready" in logs("silent-relay").stdout,
                     "silent relay readiness")
            start_controller("silent-failure", 40, "relay-grunts.json")
            wait_for(lambda: requests("grunt-b") > 0, "Grunt B workload through relay", timeout=5)
            (work / "relay" / "silent").touch()
            timeout_message = f"Grunt {relay_address}: deadline has elapsed"
            wait_for(lambda: timestamps("silent-failure", timeout_message, "stderr"),
                     "controller I/O timeout on the silent connection", timeout=4)
            [detected_at] = timestamps("silent-failure", timeout_message, "stderr")
            [paused_at] = timestamps("silent-relay", "pilot-relay: traffic paused")
            detection_seconds = (detected_at - paused_at).total_seconds()
            require(0.5 <= detection_seconds <= 2.5,
                    "Unexpected controller detection delay after relay paused", detection_seconds)
            silent_report = collect("silent-failure", 1)
            verify(silent_report, 20, ("complete", "missing"), partial=True,
                   agent_addresses=(addresses["grunt-a"], relay_address))
            require(silent_report["thresholds"]["failure_rate"] == "not_evaluable",
                    "Silent failure did not invalidate the threshold", silent_report["thresholds"])
            starts = {role: timestamps(grunt_names[role], "pilot-http: connection") for role in GRUNTS}
            counts_silent = {role: len(times) for role, times in starts.items()}
            require(counts_silent["grunt-a"] == 20 and 0 < counts_silent["grunt-b"] < 20,
                    "Unexpected workload shares after silence", counts_silent)
            last_isolated_seconds = (starts["grunt-b"][-1] - paused_at).total_seconds()
            quiet_seconds = (starts["grunt-a"][-1] - starts["grunt-b"][-1]).total_seconds()
            require(3.0 <= last_isolated_seconds <= 6.5 and quiet_seconds >= 2.0,
                    "Unexpected workload stop after silence", (last_isolated_seconds, quiet_seconds))
            require(docker("inspect", "--format", "{{.State.Running}}", f"{prefix}-silent-relay")
                    .stdout.strip() == "true", "Relay exited during silent failure", relay_address)
            print(f"Silent failure passed: controller detected it in {detection_seconds:.2f}s; "
                  f"last isolated request at {last_isolated_seconds:.2f}s; counts {counts_silent}.",
                  flush=True)

            # Each retained result costs 32 KiB of metadata plus its histogram.
            # The two-record table on B admits one result and one failed record.
            configs = start_grunts("maintenance", ("new", "new"), ("new",), ("new",), limits={
                "grunt-b": {"max_records": 2, "max_retained_result_bytes": 33_792},
            })
            start_controller("fill", 2)
            first = collect("fill", 0)
            verify(first, 2, ("complete", "complete"), partial=False)
            require(target_connections() == {"grunt-a": 1, "grunt-b": 1},
                    "First run did not reach both targets once", target_connections())

            saturation = {}
            for reason, expected_b_connections in (("result_storage_capacity", 2),
                                                   ("run_record_capacity", 2)):
                start_controller(reason, 2)
                report = collect(reason, 1)
                verify(report, 1, ("complete", "failed"), partial=True)
                failed = next(agent for agent in report["agents"] if agent["address"] == addresses["grunt-b"])
                require(failed.get("reason") == reason, "Incorrect capacity failure", failed)
                require(report["thresholds"]["failure_rate"] == "not_evaluable",
                        "Capacity failure did not invalidate the threshold", report["thresholds"])
                connections = target_connections()
                require(connections == {"grunt-a": len(saturation) + 2, "grunt-b": expected_b_connections},
                        "Unexpected workload at capacity", connections)
                require(f"Grunt {reason}:" in logs(grunt_names["grunt-b"]).stderr,
                        "Missing Grunt capacity diagnostic", reason)
                saturation[reason] = {"report": report, "target_connections": connections}

            before_restart = preflight("before-restart", "new", ("new",), both_ready)
            stopped_name = f"{prefix}-{grunt_names['grunt-b']}"
            docker("stop", "--time", "5", stopped_name)
            exit_code = docker("wait", stopped_name).stdout.strip()
            require(exit_code == "143", "Grunt did not handle SIGTERM", exit_code)
            remove("rm", stopped_name)
            launch_grunt("grunt-b", "maintenance-restarted-grunt-b", configs["grunt-b"])

            after_restart = preflight("after-restart", "new", ("new",), both_ready)
            before_epochs = [agent["epoch"] for agent in before_restart["report"]["grunts"]]
            after_epochs = [agent["epoch"] for agent in after_restart["report"]["grunts"]]
            require(all(isinstance(epoch, list) and len(epoch) == 16
                        for epoch in before_epochs + after_epochs)
                    and before_epochs[0] == after_epochs[0] and before_epochs[1] != after_epochs[1],
                    "Expected only Grunt B to change epoch", (before_epochs, after_epochs))
            before_recovery = target_connections()
            start_controller("recovery", 2)
            recovery = collect("recovery", 0)
            verify(recovery, 2, ("complete", "complete"), partial=False)
            recovered = {role: total - before_recovery[role] for role, total in target_connections().items()}
            require(recovered == {"grunt-a": 1, "grunt-b": 1},
                    "Unexpected workload after restart", recovered)
            print("Capacity diagnostics and controlled Grunt restart passed.", flush=True)

            stopped_controllers = {
                "sigint": controller_stop("SIGINT", 130, latest_request_limit=1.5),
                "sigkill": controller_stop("SIGKILL", 137, latest_request_limit=4.0),
            }
            start_grunts("inventory", ("new", "new"), ("new",), ("new",))
            managed_inventory = inventory_changes()
        except Exception:
            for container in containers:
                result = docker("logs", container, check=False)
                if result.returncode == 0:
                    print(f"--- {container} ---\n{result.stdout}{result.stderr}", file=sys.stderr)
            raise

        return {
            "subnet": subnet,
            "roles": peers,
            "preflight": {
                "nominal": checked,
                "wrong_identity": rejected,
                "unlisted_source": {"source_ip": unlisted_ip, **source_denied},
            },
            "certificate_rotation": rotation,
            "nominal": {"controller_report": nominal, "requests_per_target": counts},
            "link_disconnect": {"controller_report": fault, "requests_per_target": after_drop},
            "silent_failure": {
                "controller_report": silent_report,
                "controller_detection_seconds": detection_seconds,
                "last_isolated_request_seconds": last_isolated_seconds,
                "quiet_while_healthy_seconds": quiet_seconds,
                "target_connections": counts_silent,
            },
            "maintenance": {
                "saturation": saturation,
                "before_restart": before_restart,
                "after_restart": after_restart,
                "grunt_exit_code": int(exit_code),
                "recovery": {"controller_report": recovery, "target_connections": recovered},
            },
            "controller_stop": stopped_controllers,
            "inventory_changes": managed_inventory,
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
