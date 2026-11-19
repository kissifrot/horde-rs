#!/usr/bin/env python3
"""Generate and sign keys for the private Horde pilot using OpenSSL 3."""

import argparse
import ipaddress
import json
import os
from pathlib import Path
import re
import secrets
import subprocess
import tempfile

ROLES = ("controller", "grunt-a", "grunt-b")
PRIVATE_RANGES = tuple(
    ipaddress.ip_network(cidr)
    for cidr in ("10.0.0.0/8", "172.16.0.0/12", "192.168.0.0/16", "fc00::/7")
)
KEY_OPTIONS = ["-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:prime256v1", "-noenc"]


def openssl(*args):
    subprocess.run(["openssl", *map(str, args)], check=True)


def private_ip(value):
    try:
        ip = ipaddress.ip_address(value)
    except ValueError as error:
        raise argparse.ArgumentTypeError("expected a private IP without a port") from error
    if isinstance(ip, ipaddress.IPv6Address):
        if ip.scope_id is not None:
            raise argparse.ArgumentTypeError("IPv6 identity must not include an interface suffix")
        ip = ip.ipv4_mapped or ip
    if not any(ip in network for network in PRIVATE_RANGES):
        raise argparse.ArgumentTypeError("expected an RFC 1918 IPv4 or ULA IPv6 address")
    return ip


def init_ca(directory):
    directory.mkdir(mode=0o700)
    openssl(
        "req", "-x509", *KEY_OPTIONS, "-sha256", "-days", 30,
        "-subj", "/CN=Horde private pilot CA",
        "-addext", "basicConstraints=critical,CA:TRUE,pathlen:0",
        "-addext", "keyUsage=critical,keyCertSign,cRLSign",
        "-out", directory / "ca.pem", "-keyout", directory / "ca-key.pem",
    )
    openssl("x509", "-in", directory / "ca.pem", "-outform", "DER", "-out", directory / "ca.der")


def socket_address(value):
    if not isinstance(value, str):
        raise ValueError("Grunt address must be a string")
    host, _, port = value.rpartition(":")
    if host.startswith("[") and host.endswith("]"):
        host = host[1:-1]
        if ":" not in host:
            raise ValueError("only IPv6 addresses use brackets")
    elif ":" in host:
        raise ValueError("IPv6 socket addresses require brackets")
    if not re.fullmatch(r"[0-9]{1,5}", port) or not 1 <= int(port) <= 65535:
        raise ValueError("Grunt address must have a nonzero port")
    return private_ip(host), int(port)


def subject_alt_name(value):
    try:
        ipaddress.ip_address(value)
    except ValueError:
        name = value.lower().removesuffix(".")
        if not value.isascii() or len(name) > 253 or not all(
            re.fullmatch(r"[a-z0-9](?:[a-z0-9-]{0,61}[a-z0-9])?", label)
            for label in name.split(".")
        ):
            raise ValueError(f"invalid Grunt DNS identity: {value!r}") from None
        return "DNS", name
    return "IP", str(private_ip(value))


def inventory_identity(path, address):
    selected = socket_address(address)
    inventory = json.loads(path.read_text())
    if not isinstance(inventory, dict) or set(inventory) != {"grunts"}:
        raise ValueError("inventory must contain a grunts array")
    if not isinstance(inventory["grunts"], list):
        raise ValueError("inventory must contain a grunts array")
    peers = {}
    identities = set()
    for entry in inventory["grunts"]:
        if not isinstance(entry, dict) or "address" not in entry or entry.keys() - {"address", "tls_identity"}:
            raise ValueError("expected a Grunt address and optional tls_identity")
        socket = socket_address(entry["address"])
        identity = entry.get("tls_identity")
        if identity is not None and not isinstance(identity, str):
            raise ValueError(f"TLS identity for {entry['address']} must be a string")
        san = ("IP", str(socket[0])) if identity is None else subject_alt_name(identity)
        if socket in peers or san in identities:
            raise ValueError(f"duplicate Grunt address or TLS identity: {entry['address']}")
        peers[socket] = san
        identities.add(san)
    if selected not in peers:
        raise ValueError(f"Grunt address is absent from the inventory: {address}")
    return peers[selected]


def request(role, directory, san=None):
    directory.mkdir(mode=0o700)
    with tempfile.TemporaryDirectory() as work:
        pem_key = Path(work) / "key.pem"
        openssl(
            "req", "-new", *KEY_OPTIONS, "-sha256", "-subj", f"/CN=horde-pilot-{role}",
            *(["-addext", f"subjectAltName={':'.join(san)}"] if san else []),
            "-out", directory / "request.pem", "-keyout", pem_key,
        )
        openssl(
            "pkcs8", "-topk8", "-nocrypt", "-in", pem_key,
            "-outform", "DER", "-out", directory / "key.der",
        )


def issue(role, san, ca_dir, csr, output):
    ca = ca_dir / "ca.pem"
    if output.exists():
        raise FileExistsError(f"certificate already exists: {output}")
    with tempfile.TemporaryDirectory() as work:
        ext = Path(work) / "extensions.cnf"
        purpose = "sslclient" if role == "controller" else "sslserver"
        eku = "clientAuth" if role == "controller" else "serverAuth"
        extensions = [
            "[leaf]",
            "basicConstraints=critical,CA:FALSE",
            "keyUsage=critical,digitalSignature",
            f"extendedKeyUsage={eku}",
        ]
        if san is not None:
            extensions.append(f"subjectAltName={':'.join(san)}")
        ext.write_text("\n".join(extensions) + "\n")
        pem = Path(work) / "certificate.pem"
        der = Path(work) / "certificate.der"
        openssl(
            "x509", "-req", "-in", csr, "-CA", ca, "-CAkey", ca_dir / "ca-key.pem",
            "-set_serial", f"0x{secrets.randbits(127) + 1:x}", "-days", 7, "-sha256",
            "-subj", f"/CN=horde-pilot-{role}", "-copy_extensions", "none",
            "-extfile", ext, "-extensions", "leaf", "-out", pem,
        )
        identity_check = []
        if san is not None:
            kind, name = san
            identity_check = ["-verify_ip" if kind == "IP" else "-verify_hostname", name]
        openssl("verify", "-CAfile", ca, "-purpose", purpose, *identity_check, pem)
        openssl("x509", "-in", pem, "-outform", "DER", "-out", der)
        with output.open("xb") as certificate:
            certificate.write(der.read_bytes())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    ca = commands.add_parser("init-ca", help="create a dedicated 30-day pilot CA")
    ca.add_argument("directory", type=Path)
    csr = commands.add_parser("request", help="create a local private key and CSR")
    csr.add_argument("role", choices=ROLES)
    csr.add_argument("directory", type=Path)
    grunt_csr = commands.add_parser("request-grunt", help="create a Grunt key and CSR locally from inventory")
    grunt_csr.add_argument("directory", type=Path)
    sign = commands.add_parser("issue", help="verify a CSR and issue a 7-day leaf")
    sign.add_argument("role", choices=ROLES)
    sign.add_argument("--ip", type=private_ip, help="required Grunt IP SAN")
    grunt_sign = commands.add_parser("issue-grunt", help="sign a Grunt CSR for the inventory identity")
    for command in (grunt_csr, grunt_sign):
        command.add_argument("--inventory", type=Path, required=True)
        command.add_argument("--address", required=True, help="the Grunt's exact control socket address")
    for command in (sign, grunt_sign):
        command.add_argument("--ca-dir", type=Path, required=True)
        command.add_argument("--csr", type=Path, required=True)
        command.add_argument("--out", type=Path, required=True)
    args = parser.parse_args()
    if args.command == "issue" and ((args.role == "controller") == (args.ip is not None)):
        parser.error("--ip is required for a Grunt and omitted for the controller")
    os.umask(0o077)
    try:
        if args.command == "init-ca":
            init_ca(args.directory)
        elif args.command == "request":
            request(args.role, args.directory)
        elif args.command == "request-grunt":
            request("grunt", args.directory, inventory_identity(args.inventory, args.address))
        elif args.command == "issue-grunt":
            san = inventory_identity(args.inventory, args.address)
            issue("grunt", san, args.ca_dir, args.csr, args.out)
        else:
            san = ("IP", str(args.ip)) if args.ip is not None else None
            issue(args.role, san, args.ca_dir, args.csr, args.out)
    except (OSError, subprocess.SubprocessError, ValueError, argparse.ArgumentTypeError) as error:
        parser.exit(1, f"pilot certificate setup failed: {error}\n")


if __name__ == "__main__":
    main()
