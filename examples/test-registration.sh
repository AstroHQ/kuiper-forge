#!/bin/bash
# Test script for agent registration flow
# Run from the repo root directory

set -e

echo "=== CI Runner Coordinator - Registration Test ==="
echo ""

# Determine data directories based on OS
if [[ "$OSTYPE" == "darwin"* ]]; then
    COORD_DATA_DIR="$HOME/Library/Application Support/ci-runner-coordinator"
    COORD_CONFIG_DIR="$HOME/Library/Application Support/ci-runner-coordinator"
else
    COORD_DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/ci-runner-coordinator"
    COORD_CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/ci-runner-coordinator"
fi

echo "Coordinator data dir: $COORD_DATA_DIR"
echo ""

# Step 1: Initialize CA
echo "=== Step 1: Initialize CA ==="
if [ -f "$COORD_DATA_DIR/ca.crt" ]; then
    echo "CA already exists at $COORD_DATA_DIR/ca.crt"
else
    cargo run --bin coordinator -- ca init --org "Test Org"
fi
echo ""

# Step 2: Generate server certificate
echo "=== Step 2: Generate server certificate ==="
if [ -f "$COORD_DATA_DIR/server.crt" ]; then
    echo "Server cert already exists at $COORD_DATA_DIR/server.crt"
else
    cargo run --bin coordinator -- ca server-cert --hostname localhost
fi
echo ""

# Step 3: Generate coordinator config
echo "=== Step 3: Generate coordinator config ==="
COORD_CONFIG="$COORD_CONFIG_DIR/config.toml"
mkdir -p "$COORD_CONFIG_DIR"
if [ -f "$COORD_CONFIG" ]; then
    echo "Config already exists at $COORD_CONFIG"
else
    cargo run --bin coordinator -- init-config -o "$COORD_CONFIG"
    echo "Generated config at $COORD_CONFIG"
fi
echo ""

# Step 4: Create registration token
echo "=== Step 4: Create registration token ==="
echo "Creating token for tart-agent..."
TOKEN=$(cargo run --bin coordinator -- token create --labels macos,arm64 2>&1 | grep "Token:" | awk '{print $2}')
echo "Token: $TOKEN"
echo ""

# Step 5: Show next steps
echo "=== Next Steps ==="
echo ""
echo "1. In terminal 1, start the coordinator:"
echo "   cargo run --bin coordinator -- serve"
echo ""
echo "2. In terminal 2, bootstrap and start the tart-agent:"
echo "   cargo run --bin tart-agent -- \\"
echo "       --coordinator-url https://localhost:9443 \\"
echo "       --token $TOKEN \\"
echo "       --ca-cert \"$COORD_DATA_DIR/ca.crt\" \\"
echo "       --labels macos,arm64"
echo ""
echo "3. After registration, subsequent runs just need:"
echo "   cargo run --bin tart-agent"
echo ""
echo "4. Check registered agents:"
echo "   cargo run --bin coordinator -- agent list"
echo ""
echo "=== For proxmox-agent ==="
echo ""
echo "Create a new token and use --token flag:"
echo "   cargo run --bin coordinator -- token create --labels linux,x64"
echo "   cargo run --bin proxmox-agent -- --config examples/proxmox-agent-config.toml --token REG_TOKEN"
