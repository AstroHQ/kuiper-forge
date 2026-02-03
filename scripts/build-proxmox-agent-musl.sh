#!/usr/bin/env bash
set -euo pipefail

# Build kuiper-proxmox-agent for Linux musl targets (arm64 and x86_64)
# Requires: cross (cargo install cross --git https://github.com/cross-rs/cross)
# Requires: Docker running
#
# Note: This project uses aws-lc-rs for TLS. If you encounter build issues,
# you may need to add a Cross.toml with custom images that have cmake/clang.

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"
OUTPUT_DIR="${PROJECT_ROOT}/target/musl-release"

TARGETS=(
    "x86_64-unknown-linux-musl"
    "aarch64-unknown-linux-musl"
)

# Check if cross is installed
if ! command -v cross &> /dev/null; then
    echo "Error: 'cross' is not installed."
    echo "Install it with: cargo install cross --git https://github.com/cross-rs/cross"
    exit 1
fi

# Check if Docker is running
if ! docker info &> /dev/null; then
    echo "Error: Docker is not running. cross requires Docker."
    exit 1
fi

mkdir -p "$OUTPUT_DIR"

echo "Building kuiper-proxmox-agent for musl targets..."
echo ""

for target in "${TARGETS[@]}"; do
    echo "=== Building for $target ==="

    cross build \
        --release \
        --target "$target" \
        --package kuiper-proxmox-agent \
        --manifest-path "$PROJECT_ROOT/Cargo.toml"

    # Copy binary to output directory with target suffix
    src_binary="$PROJECT_ROOT/target/$target/release/kuiper-proxmox-agent"

    # Determine architecture suffix for output filename
    case "$target" in
        x86_64-*)
            arch="x86_64"
            ;;
        aarch64-*)
            arch="arm64"
            ;;
    esac

    dest_binary="$OUTPUT_DIR/kuiper-proxmox-agent-linux-musl-$arch"
    cp "$src_binary" "$dest_binary"

    echo "Built: $dest_binary"
    echo ""
done

echo "=== Build complete ==="
echo "Binaries available in: $OUTPUT_DIR"
ls -lh "$OUTPUT_DIR"/kuiper-proxmox-agent-*
