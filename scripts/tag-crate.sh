#!/usr/bin/env bash
set -euo pipefail

if [ $# -ne 1 ]; then
    echo "Usage: $0 <crate-directory>"
    echo "Example: $0 kuiper-forge"
    exit 1
fi

crate_dir="$1"
cargo_toml="$crate_dir/Cargo.toml"

if [ ! -f "$cargo_toml" ]; then
    echo "Error: $cargo_toml not found"
    exit 1
fi

name=$(grep '^name' "$cargo_toml" | head -1 | sed 's/.*= *"\(.*\)"/\1/')
version=$(grep '^version' "$cargo_toml" | head -1 | sed 's/.*= *"\(.*\)"/\1/')

if [ -z "$name" ] || [ -z "$version" ]; then
    echo "Error: could not parse name/version from $cargo_toml"
    exit 1
fi

tag="${name}-v${version}"

echo "Tagging: $tag"
git tag -s -a "$tag" -m "$tag"
echo "Created signed tag: $tag"
