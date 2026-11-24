#!/bin/sh
set -eu

usage() {
    printf 'Usage: sh install.sh [vX.Y.Z[-rc.N]]\n'
    printf 'Without a tag, install the most recently published compatible release.\n'
    printf 'Set HORDE_INSTALL_DIR to change the default destination ($HOME/.local/bin).\n'
}

fail() {
    printf 'horde installer: %s\n' "$*" >&2
    exit 1
}

if [ "${1:-}" = "--help" ] || [ "${1:-}" = "-h" ]; then
    usage
    exit 0
fi

if [ "$#" -gt 1 ]; then
    usage >&2
    exit 2
fi
tag=${1-}
tag_pattern='^v[0-9]+[.][0-9]+[.][0-9]+(-rc[.][0-9]+)?$'

system=$(uname -s)
machine=$(uname -m)
case "$system:$machine" in
    Linux:x86_64) asset=horde-linux-amd64; set -- sha256sum ;;
    Linux:aarch64|Linux:arm64) asset=horde-linux-arm64; set -- sha256sum ;;
    Darwin:arm64) asset=horde-macos-arm64; set -- shasum -a 256 ;;
    *) fail "unsupported platform: $system $machine (Linux AMD64/ARM64 or macOS Apple Silicon required)" ;;
esac

for command in curl "$1"; do
    command -v "$command" >/dev/null 2>&1 || fail "$command is required"
done

destination=${HORDE_INSTALL_DIR:-${HOME:?HOME is required}/.local/bin}
case "$destination" in
    /*) ;;
    *) destination=$PWD/$destination ;;
esac
downloads=$(mktemp -d "${TMPDIR:-/tmp}/horde-install.XXXXXX")
staged=
trap 'rm -rf -- "$downloads"; if [ -n "$staged" ]; then rm -f -- "$staged"; fi' EXIT
trap 'exit 1' HUP INT TERM

fetch() {
    curl --disable --fail --silent --show-error --location \
        --proto '=https' --proto-redir '=https' --connect-timeout 15 --max-time 180 \
        --output "$2" "$1"
}

select_release() {
    # Read the public release list by JSON nesting level. Consume quoted strings
    # as whole tokens so release notes cannot look like metadata or asset names.
    LC_ALL=C awk -v asset="$asset" -v pattern="$tag_pattern" '
        function inspect(token,    value) {
            if (token == ":") {
                key[depth] = previous
            } else if (token == "[" || token == "{") {
                if (depth == 2)
                    in_assets = (token == "[" && key[depth] == "\"assets\"")
                key[depth] = ""
                opener[++depth] = token
                if (depth == 2)
                    name = date = published = has_binary = has_checksums = ""
            } else if (token == "]" || token == "}") {
                if (opener[depth] != (token == "]" ? "[" : "{")) exit 2
                if (depth == 2 && published && date != "" && name ~ pattern &&
                    has_binary && has_checksums && date > newest) {
                    selected = name
                    newest = date
                }
                delete key[depth]
                depth--
            } else if (token != ",") {
                if (token ~ /^"/)
                    value = substr(token, 2, length(token) - 2)
                if (depth == 2 && key[depth] == "\"tag_name\"")
                    name = value
                if (depth == 2 && key[depth] == "\"published_at\"")
                    date = value
                if (depth == 2 && key[depth] == "\"draft\"")
                    published = (token == "false")
                if (depth == 4 && in_assets && key[depth] == "\"name\"") {
                    if (value == asset) has_binary = 1
                    if (value == "SHA256SUMS") has_checksums = 1
                }
                key[depth] = ""
            }
            previous = token
        }
        {
            line = $0
            for (;;) {
                sub(/^[[:space:]]+/, "", line)
                if (line == "") break
                if (!match(line, /^("(\\.|[^"\\])*"|\[|\]|[{}:,]|true|false|null|-?[0-9]+(\.[0-9]+)?([eE][+-]?[0-9]+)?)/)) exit 2
                token = substr(line, 1, RLENGTH)
                line = substr(line, RLENGTH + 1)
                if (depth == 0 && (started++ || token != "[")) exit 2
                inspect(token)
            }
        }
        END {
            if (!started || depth != 0) exit 2
            if (selected != "") print selected
        }
    ' "$1"
}

if [ -z "$tag" ]; then
    fetch 'https://api.github.com/repos/kissifrot/horde-rs/releases?per_page=100' "$downloads/releases.json"
    tag=$(select_release "$downloads/releases.json") || fail "could not read the GitHub release list"
    [ -n "$tag" ] || fail "no compatible release for $asset; you can specify a tag explicitly"
fi
case "$tag" in
    *[!a-z0-9.-]*) fail "invalid release tag: $tag" ;;
esac
if ! printf '%s\n' "$tag" | grep -Eq "$tag_pattern"; then
    fail "invalid release tag: $tag"
fi
release_url=https://github.com/kissifrot/horde-rs/releases/download/$tag

download() {
    fetch "$release_url/$1" "$downloads/$1"
}

download SHA256SUMS
if ! checksum=$(awk -v asset="$asset" '
    NF == 2 && ($2 == asset || $2 == "*" asset) {
        count++
        if (length($1) == 64 && $1 ~ /^[0-9a-fA-F]+$/) hash = $1
    }
    END {
        if (count != 1 || hash == "") exit 1
        print tolower(hash)
    }
' "$downloads/SHA256SUMS"); then
    fail "missing or invalid SHA-256 entry for $asset in $tag"
fi

download "$asset"
actual=$(cd "$downloads" && "$@" "$asset") || fail "could not hash $asset"
if [ "${actual%% *}" != "$checksum" ]; then
    fail "SHA-256 verification failed for $asset"
fi

mkdir -p -- "$destination"
[ ! -d "$destination/horde" ] || fail "$destination/horde is a directory"
staged=$(mktemp "$destination/.horde.XXXXXX")
install -m 755 "$downloads/$asset" "$staged"
if [ "$system" = Darwin ]; then
    mv -fh "$staged" "$destination/horde"
else
    mv -fT "$staged" "$destination/horde"
fi
staged=
printf 'Installed Horde %s at %s/horde\n' "$tag" "$destination"
case ":${PATH:-}:" in
    *":$destination:"*) ;;
    *) printf 'Add %s to PATH to run horde by name.\n' "$destination" ;;
esac
