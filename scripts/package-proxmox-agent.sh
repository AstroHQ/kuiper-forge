#!/usr/bin/env bash
set -euo pipefail

# Build a kuiper-proxmox-agent Linux package locally for testing.
#
# Mirrors scripts/build-proxmox-agent-musl.sh: builds the static musl binary
# with `cross`, then runs `nfpm` to produce deb/rpm/... from one config.
#
# Usage:
#   scripts/package-proxmox-agent.sh [arch]
#     arch: x86_64 (default) | arm64
#
# Env:
#   PACKAGERS  space-separated nfpm packagers (default: "deb rpm";
#              nfpm also supports apk and archlinux)
#
# Requires: cross + Docker (for the build). Uses a local `nfpm` if installed,
# otherwise falls back to the goreleaser/nfpm Docker image.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"
cd "$PROJECT_ROOT"

ARCH_INPUT="${1:-x86_64}"
PACKAGERS="${PACKAGERS:-deb rpm}"
NFPM_CONFIG="packaging/proxmox-agent/nfpm.yaml"
OUTPUT_REL="target/packages"
OUTPUT_ABS="$PROJECT_ROOT/$OUTPUT_REL"

case "$ARCH_INPUT" in
    x86_64 | amd64)
        RUST_TARGET="x86_64-unknown-linux-musl"
        export ARCH="amd64"
        ;;
    arm64 | aarch64)
        RUST_TARGET="aarch64-unknown-linux-musl"
        export ARCH="arm64"
        ;;
    *)
        echo "Error: unknown arch '$ARCH_INPUT' (use x86_64 or arm64)" >&2
        exit 1
        ;;
esac

# nfpm expands ${VERSION}/${ARCH} in nfpm.yaml. It does NOT expand env vars in
# contents[].src, so we stage the built binary at a fixed, arch-independent path
# (STAGE_BIN) that the config references. All paths are repo-relative so they
# resolve for a local nfpm (run from PROJECT_ROOT) and inside Docker (/work).
VERSION="$(grep -m1 '^version' kuiper-proxmox-agent/Cargo.toml | cut -d'"' -f2)"
export VERSION
BUILT_BIN="target/${RUST_TARGET}/release/kuiper-proxmox-agent"
STAGE_BIN="target/nfpm/kuiper-proxmox-agent"

if ! command -v cross >/dev/null 2>&1; then
    echo "Error: 'cross' is not installed." >&2
    echo "Install it with: cargo install cross --git https://github.com/cross-rs/cross" >&2
    exit 1
fi
if ! docker info >/dev/null 2>&1; then
    echo "Error: Docker is not running (required by cross)." >&2
    exit 1
fi

echo "=== Building kuiper-proxmox-agent v${VERSION} for ${RUST_TARGET} (${ARCH}) ==="
cross build --release --target "$RUST_TARGET" --package kuiper-proxmox-agent
# Best-effort strip; the host strip can't touch a Linux ELF, so ignore failures.
strip "$BUILT_BIN" 2>/dev/null || true

# Stage the binary at the fixed path nfpm.yaml references (see note there).
mkdir -p "$(dirname "$STAGE_BIN")" "$OUTPUT_ABS"
cp "$BUILT_BIN" "$STAGE_BIN"

# Prefer a local nfpm (runs natively, even on macOS); otherwise run it via the
# official Docker image with the repo mounted at /work and VERSION/ARCH passed in.
run_nfpm() {
    if command -v nfpm >/dev/null 2>&1; then
        nfpm "$@"
    else
        docker run --rm \
            --user "$(id -u):$(id -g)" \
            -v "$PROJECT_ROOT":/work -w /work \
            -e VERSION -e ARCH \
            ghcr.io/goreleaser/nfpm:latest "$@"
    fi
}

if ! command -v nfpm >/dev/null 2>&1; then
    echo "Note: 'nfpm' not found locally; using the goreleaser/nfpm Docker image."
    echo "      (install locally with 'brew install nfpm' for faster runs)"
fi

for pkg in $PACKAGERS; do
    echo "=== Packaging: $pkg ==="
    run_nfpm package --config "$NFPM_CONFIG" --packager "$pkg" --target "$OUTPUT_REL"
done

echo ""
echo "=== Done ==="
ls -lh "$OUTPUT_ABS"
