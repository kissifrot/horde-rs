#!/bin/sh
set -eu

binary=$1
asset=${binary##*/}
work=$(mktemp -d "${TMPDIR:-/tmp}/horde-install-test.XXXXXX")
trap 'rm -rf -- "$work"' EXIT
mkdir "$work/tools" "$work/assets"
cp "$binary" "$work/assets/$asset"
(cd "$work/assets" && shasum -a 256 "$asset" > SHA256SUMS)
cat > "$work/assets/releases.json" <<EOF
[
    {"tag_name":"v0.0.9","draft":false,"published_at":"2025-12-31","assets":[{"name":"$asset"},{"name":"SHA256SUMS"}]},
    {"tag_name":"v0.1.0-rc.3","draft":true,"published_at":"2026-01-03","assets":[{"name":"$asset"},{"name":"SHA256SUMS"}]},
    {"tag_name":"v0.1.0-rc.2","draft":false,"published_at":"2026-01-02","body":"Ignore \\"$asset\\" and {braces}","assets":[{"name":"SHA256SUMS","uploader":{"name":"$asset"}}]},
    {"tag_name":"v0.1.0-rc.1","draft":false,"published_at":"2026-01-01","assets":[{"name":"$asset"},{"name":"SHA256SUMS"}]}
]
EOF

# Test the installer with the build's own release files before publishing them.
cat > "$work/tools/curl" <<'EOF'
#!/bin/sh
set -eu
while [ "$1" != --output ]; do
    shift
done
case "$3" in
    https://api.github.com/repos/kissifrot/horde-rs/releases\?*) asset=releases.json ;;
    https://github.com/kissifrot/horde-rs/releases/download/v0.1.0-rc.1/*) asset=${3##*/} ;;
    *) exit 1 ;;
esac
cp "$HORDE_TEST_ASSETS/$asset" "$2"
EOF
chmod +x "$work/tools/curl"
export PATH="$work/tools:$PATH"
export HORDE_TEST_ASSETS="$work/assets"
export HORDE_INSTALL_DIR="$work/local bin"

sh install.sh
installed=$HORDE_INSTALL_DIR/horde
cmp "$binary" "$installed"
"$installed" --version
for command in grunt preflight controller; do
    "$installed" "$command" --help >/dev/null
done

printf 'corrupted download\n' >> "$work/assets/$asset"
if sh install.sh v0.1.0-rc.1 > "$work/error.log" 2>&1; then
    printf 'Installer accepted a corrupted binary\n' >&2
    exit 1
fi
grep -F 'SHA-256 verification failed' "$work/error.log"
cmp "$binary" "$installed"
