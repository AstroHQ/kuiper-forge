#!/bin/bash
# Generic version bump script for any crate in the workspace
# Usage: bump_version.sh <version>
# Called from within the package directory by cog's pre_bump_hooks
#
# This script:
# 1. Updates the crate's own version in Cargo.toml
# 2. Updates any workspace crates that depend on this crate
set -e

VERSION="${1}"

if [ -z "$VERSION" ]; then
    echo "Error: No version provided"
    exit 1
fi

# Get the crate name from the current directory's Cargo.toml
CRATE_NAME=$(grep '^name = ' Cargo.toml | head -1 | sed 's/name = "\([^"]*\)"/\1/')

if [ -z "$CRATE_NAME" ]; then
    echo "Error: Could not determine crate name"
    exit 1
fi

# Update this crate's version
sed -i '' "s/^version = \"[^\"]*\"/version = \"${VERSION}\"/" Cargo.toml
git add Cargo.toml

# Get the workspace root (parent directory)
WORKSPACE_ROOT="$(cd .. && pwd)"

# Find and update all Cargo.toml files that depend on this crate
for cargo_file in "$WORKSPACE_ROOT"/*/Cargo.toml; do
    # Skip our own Cargo.toml
    if [ "$cargo_file" = "$(pwd)/Cargo.toml" ]; then
        continue
    fi

    # Check if this file has a dependency on our crate and update the version
    # Matches patterns like: kuiper-agent-proto = { version = "0.1.0", path = ...
    if grep -q "${CRATE_NAME} = { version = " "$cargo_file" 2>/dev/null; then
        sed -i '' "s/${CRATE_NAME} = { version = \"[^\"]*\"/${CRATE_NAME} = { version = \"${VERSION}\"/" "$cargo_file"
        git add "$cargo_file"
        echo "Updated ${CRATE_NAME} dependency in $cargo_file"
    fi
done
