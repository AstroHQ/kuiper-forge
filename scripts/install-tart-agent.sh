#!/bin/sh
# installs kuiper-tart-agent from github releases. later updates: `kuiper-tart-agent update`
#
#   curl -fsSL https://raw.githubusercontent.com/AstroHQ/kuiper-forge/main/scripts/install-tart-agent.sh | sh
#
# env:
#   KUIPER_VERSION      version to install, e.g. 0.4.0 (default: latest)
#   KUIPER_INSTALL_DIR  where the binary goes (default: /usr/local/bin)
#   GITHUB_TOKEN        optional, avoids the unauthenticated API rate limit
set -eu

REPO=AstroHQ/kuiper-forge
BIN=kuiper-tart-agent
INSTALL_DIR="${KUIPER_INSTALL_DIR:-/usr/local/bin}"

die() {
    echo "error: $*" >&2
    exit 1
}

api() {
    if [ -n "${GITHUB_TOKEN:-}" ]; then
        curl -fsSL -H "Authorization: Bearer $GITHUB_TOKEN" "$@"
    else
        curl -fsSL "$@"
    fi
}

[ "$(uname -s)" = Darwin ] || die "$BIN only runs on macOS"

arch=$(uname -m)

# uname says x86_64 when the shell runs under rosetta, still want the native build
if [ "$arch" = x86_64 ] && [ "$(sysctl -n sysctl.proc_translated 2>/dev/null)" = 1 ]; then
    arch=arm64
fi
case "$arch" in
    arm64 | aarch64) target=aarch64-apple-darwin ;;
    x86_64) target=x86_64-apple-darwin ;;
    *) die "unsupported architecture: $arch" ;;
esac

version="${KUIPER_VERSION:-}"
version="${version#v}"
if [ -z "$version" ]; then
    # the repo releases several crates so /releases/latest may not be ours. the list is newest first, and the
    # pattern skips pre-release tags like -rc1
    version=$(api "https://api.github.com/repos/$REPO/releases?per_page=100" |
        grep -o "\"tag_name\": *\"$BIN-v[0-9]*\.[0-9]*\.[0-9]*\"" |
        head -1 |
        sed "s/.*$BIN-v\([^\"]*\)\"/\1/") || true
    [ -n "$version" ] || die "couldn't find a $BIN release"
fi

archive="$BIN-v$version-$target.tar.gz"
url="https://github.com/$REPO/releases/download/$BIN-v$version/$archive"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

echo "Downloading $archive..."
curl -fsSL -o "$tmp/$archive" "$url" || die "download failed: $url"

# releases before checksums were added to CI don't have one
if curl -fsL -o "$tmp/$archive.sha256" "$url.sha256" 2>/dev/null; then
    expected=$(awk '{print $1}' "$tmp/$archive.sha256")
    actual=$(shasum -a 256 "$tmp/$archive" | awk '{print $1}')
    [ "$expected" = "$actual" ] || die "checksum mismatch: expected $expected, got $actual"
    echo "Checksum OK"
else
    echo "Warning: no checksum published for $archive, skipping verification"
fi

tar xzf "$tmp/$archive" -C "$tmp"
[ -f "$tmp/$BIN" ] || die "archive didn't contain $BIN"

sudo=""
if ! mkdir -p "$INSTALL_DIR" 2>/dev/null || [ ! -w "$INSTALL_DIR" ]; then
    echo "$INSTALL_DIR isn't writable, using sudo"
    sudo=sudo
fi
$sudo mkdir -p "$INSTALL_DIR"

# install unlinks the old file first. writing over a signed binary in place gets it SIGKILLed on next launch
$sudo install -m 755 "$tmp/$BIN" "$INSTALL_DIR/$BIN"

echo "✓ Installed $BIN $version to $INSTALL_DIR/$BIN"

case ":$PATH:" in
    *":$INSTALL_DIR:"*) ;;
    *) echo "Warning: $INSTALL_DIR isn't in your PATH, add it before running \`$BIN install\`" ;;
esac

if launchctl print "gui/$(id -u)/com.astropad.kuiper-tart-agent" >/dev/null 2>&1; then
    echo
    echo "The LaunchAgent is running the old version. Restart it when no jobs are running:"
    echo "  launchctl kickstart -k gui/$(id -u)/com.astropad.kuiper-tart-agent"
else
    echo
    echo "Next: run \`$BIN setup\` to register this host and install the LaunchAgent"
fi
