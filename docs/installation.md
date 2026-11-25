# Install Horde

The installer supports Linux (AMD64 and ARM64) and macOS 15+ (Apple Silicon).

```sh
/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/kissifrot/horde-rs/main/install.sh)"
```

The installer puts `horde` in `$HOME/.local/bin`. Add that directory to your
PATH if needed. To select a release, pass the tag after `--`. Set
`HORDE_INSTALL_DIR` to change the installation directory.

## Linux and macOS prereleases

Get the package or standalone executable for your architecture from
[GitHub Releases](https://github.com/kissifrot/horde-rs/releases). Use
`dpkg --print-architecture` to check whether to select `amd64` or `arm64`.
Download `SHA256SUMS` alongside the asset and verify it before installing.
For the immutable `v0.1.0-rc.1` release, GitHub changed the `~` in
the package asset names to a dot. Normalize those two names in the
checksum input when verifying this release. For example, on AMD64:

```sh
sed 's/~rc\./.rc./g' SHA256SUMS | sha256sum --ignore-missing -c -
sudo apt install './horde-rs_0.1.0.rc.1_amd64.deb'
horde --version
```

For later prereleases, verify with `sha256sum --ignore-missing -c SHA256SUMS`.

The standalone file contains the same executable as the package.
It can be run directly with `chmod +x horde-linux-amd64` followed by
`./horde-linux-amd64 --version`.

The Linux package installs `horde` in `/usr/bin`. Start Grunts explicitly
when following the [distributed quickstart](distributed-quickstart.md).

## Feature availability

The packaged Linux and macOS builds include the distributed CLI.
For MCP, choose a source build with the `mcp` feature. Choose both
`mcp` and `distributed-transport` for distributed MCP tools.
See the [source build options](development.md#build-from-source).
