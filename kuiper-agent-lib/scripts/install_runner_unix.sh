#!/bin/bash
# GitHub Actions Runner installation script for Linux/macOS
# Usage: install_runner_unix.sh <runner_dir> <url> [sha256]
# GitHub publishes checksums in its release notes, not next to the archive, so one is passed in.
set -e

RUNNER_DIR="$1"
URL="$2"
SHA256="${3:-}"

if [ -z "$RUNNER_DIR" ] || [ -z "$URL" ]; then
    echo "Usage: $0 <runner_dir> <url> [sha256]" >&2
    exit 1
fi

echo "Creating runner directory..."
mkdir -p "$RUNNER_DIR"
cd "$RUNNER_DIR"

echo "Downloading runner from $URL..."
curl -fsSL --retry 3 -o actions-runner.tar.gz "$URL"

if [ -n "$SHA256" ]; then
    echo "Checking checksum..."
    if command -v sha256sum >/dev/null 2>&1; then
        echo "$SHA256  actions-runner.tar.gz" | sha256sum -c -
    else
        # macOS ships shasum instead
        echo "$SHA256  actions-runner.tar.gz" | shasum -a 256 -c -
    fi
fi

echo "Extracting runner..."
tar xzf actions-runner.tar.gz
rm actions-runner.tar.gz

echo "Runner installed successfully"
