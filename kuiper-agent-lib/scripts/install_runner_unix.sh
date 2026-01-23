#!/bin/bash
# GitHub Actions Runner installation script for Linux/macOS
# Usage: install_runner_unix.sh <runner_dir> <url>
set -e

RUNNER_DIR="$1"
URL="$2"

if [ -z "$RUNNER_DIR" ] || [ -z "$URL" ]; then
    echo "Usage: $0 <runner_dir> <url>" >&2
    exit 1
fi

echo "Creating runner directory..."
mkdir -p "$RUNNER_DIR"
cd "$RUNNER_DIR"

echo "Downloading runner from $URL..."
curl -sL -o actions-runner.tar.gz "$URL"

echo "Extracting runner..."
tar xzf actions-runner.tar.gz
rm actions-runner.tar.gz

echo "Runner installed successfully"
