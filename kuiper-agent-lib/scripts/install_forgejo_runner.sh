#!/bin/bash
# forgejo-runner installation script for Linux
# Usage: install_forgejo_runner.sh <runner_dir> <url> <file_name>
# Downloads the release binary and its .sha256, checks it, and installs it as <runner_dir>/forgejo-runner
set -eu

RUNNER_DIR="$1"
URL="$2"
NAME="$3"

mkdir -p "$RUNNER_DIR"
cd "$RUNNER_DIR"

echo "Downloading forgejo-runner from $URL..."
curl -fsSL --retry 3 -o "$NAME" "$URL"
curl -fsSL --retry 3 -o "$NAME.sha256" "$URL.sha256"

echo "Checking checksum..."
sha256sum -c "$NAME.sha256"

chmod +x "$NAME"
mv "$NAME" forgejo-runner
rm -f "$NAME.sha256"

./forgejo-runner --version
echo "forgejo-runner installed successfully"
