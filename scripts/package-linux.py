#!/usr/bin/env python3
"""Package one native, statically linked Linux build of Horde."""

import argparse
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile
import tomllib


ROOT = Path(__file__).resolve().parents[1]
MACHINES = {"amd64": 62, "arm64": 183}


def validate_binary(binary, architecture):
    with binary.open("rb") as executable:
        header = executable.read(20)
    if (header[:6] != b"\x7fELF\x02\x01"
            or int.from_bytes(header[18:20], "little") != MACHINES[architecture]):
        raise ValueError(f"expected a 64-bit Linux ELF binary for {architecture}")
    linkage = subprocess.check_output(["readelf", "-lWd", binary], text=True)
    if re.search(r"(?m)^\s*INTERP\s|\(NEEDED\)", linkage):
        raise ValueError("the Linux release binary must be statically linked")


def package(binary, architecture, version, output):
    validate_binary(binary, architecture)
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="horde-deb-") as directory:
        work = Path(directory)
        control = work / "DEBIAN"
        control.mkdir(mode=0o755)
        installed = work / "usr/bin/horde"
        installed.parent.mkdir(parents=True)
        shutil.copyfile(binary, installed)
        installed.chmod(0o755)
        copyright_file = work / "usr/share/doc/horde-rs/copyright"
        copyright_file.parent.mkdir(parents=True)
        shutil.copyfile(ROOT / "LICENSE", copyright_file)
        installed_size = (installed.stat().st_size + copyright_file.stat().st_size + 1023) // 1024
        (control / "control").write_text(
            f"Package: horde-rs\n"
            f"Version: {version}\n"
            f"Architecture: {architecture}\n"
            f"Installed-Size: {installed_size}\n"
            "Section: net\n"
            "Priority: optional\n"
            "Maintainer: Horde maintainers <kissifrot@users.noreply.github.com>\n"
            "Homepage: https://github.com/kissifrot/horde-rs\n"
            "Description: HTTP load testing tool\n"
            " Horde runs standalone and distributed tests with authenticated\n"
            " control connections and locally configured Grunt workloads.\n"
        )
        # TemporaryDirectory creates the root with mode 0700. The data archive
        # must have a normal root directory, just like its installed paths.
        work.chmod(0o755)
        deb = output / f"horde-rs_{version}_{architecture}.deb"
        subprocess.run(["dpkg-deb", "--root-owner-group", "-Zxz", "--build", work, deb],
                       check=True)
        standalone = output / f"horde-linux-{architecture}"
        shutil.copyfile(installed, standalone)
        standalone.chmod(0o755)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--architecture", choices=MACHINES, required=True)
    parser.add_argument("--output", type=Path, required=True)
    revision = parser.add_mutually_exclusive_group(required=True)
    revision.add_argument("--tag", help="a vX.Y.Z-rc.N tag matching Cargo.toml")
    revision.add_argument("--snapshot", help="a build number followed by a short commit SHA")
    args = parser.parse_args()
    with (ROOT / "Cargo.toml").open("rb") as manifest:
        base = tomllib.load(manifest)["package"]["version"]
    if args.tag is not None:
        match = re.fullmatch(rf"v{re.escape(base)}-rc\.([1-9][0-9]*)", args.tag)
        if not match:
            parser.error(f"expected v{base}-rc.N, with a positive RC number")
        version = f"{base}~rc.{match[1]}"
    else:
        if not re.fullmatch(r"[1-9][0-9]*\.[0-9a-f]{12}", args.snapshot):
            parser.error("expected a snapshot identifier such as 42.0123456789ab")
        version = f"{base}~git.{args.snapshot}"
    try:
        os.umask(0o022)
        package(args.binary, args.architecture, version, args.output)
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"Linux packaging failed: {error}\n")


if __name__ == "__main__":
    main()
