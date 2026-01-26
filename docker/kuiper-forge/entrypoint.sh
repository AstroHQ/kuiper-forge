#!/bin/sh
set -e

DATA_DIR="/data"

# First-run: generate CA certificate
if [ ! -f "$DATA_DIR/ca.crt" ]; then
  if [ -z "$KUIPER_CA_ORG" ]; then
    echo "Error: KUIPER_CA_ORG is required for initial CA setup" >&2
    exit 1
  fi
  echo "Initializing CA (org: $KUIPER_CA_ORG)..."
  kuiper-forge --data-dir "$DATA_DIR" ca init --org "$KUIPER_CA_ORG"
fi

# First-run: generate server certificate
if [ ! -f "$DATA_DIR/server.crt" ]; then
  if [ -z "$KUIPER_SERVER_HOSTNAME" ]; then
    echo "Error: KUIPER_SERVER_HOSTNAME is required for initial server certificate setup" >&2
    exit 1
  fi
  echo "Generating server certificate (hostname: $KUIPER_SERVER_HOSTNAME)..."
  kuiper-forge --data-dir "$DATA_DIR" ca server-cert --hostname "$KUIPER_SERVER_HOSTNAME"
fi

# Inject --data-dir and --config before the subcommand
BIN="$1"
shift
exec "$BIN" --data-dir "$DATA_DIR" --config "$DATA_DIR/config.toml" "$@"
