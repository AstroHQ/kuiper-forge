#!/bin/sh
set -e

DATA_DIR="/data"

# TLS paths - use environment variables if set, otherwise default to /data
CA_CERT="${KUIPER_TLS__CA_CERT:-$DATA_DIR/ca.crt}"
CA_KEY="${KUIPER_TLS__CA_KEY:-$DATA_DIR/ca.key}"
SERVER_CERT="${KUIPER_TLS__SERVER_CERT:-$DATA_DIR/server.crt}"
SERVER_KEY="${KUIPER_TLS__SERVER_KEY:-$DATA_DIR/server.key}"

# First-run: generate CA certificate (skip if KUIPER_SKIP_CA_INIT is set or cert exists)
if [ -z "$KUIPER_SKIP_CA_INIT" ] && [ ! -f "$CA_CERT" ]; then
  if [ -z "$KUIPER_CA_ORG" ]; then
    echo "Error: KUIPER_CA_ORG is required for initial CA setup" >&2
    echo "  Set KUIPER_SKIP_CA_INIT=1 if using externally-provided certificates" >&2
    exit 1
  fi
  echo "Initializing CA (org: $KUIPER_CA_ORG)..."
  kuiper-forge --data-dir "$DATA_DIR" ca init --org "$KUIPER_CA_ORG"
fi

# First-run: generate server certificate (skip if KUIPER_SKIP_SERVER_CERT_INIT is set or cert exists)
if [ -z "$KUIPER_SKIP_SERVER_CERT_INIT" ] && [ ! -f "$SERVER_CERT" ]; then
  if [ -z "$KUIPER_SERVER_HOSTNAME" ]; then
    echo "Error: KUIPER_SERVER_HOSTNAME is required for initial server certificate setup" >&2
    echo "  Set KUIPER_SKIP_SERVER_CERT_INIT=1 if using externally-provided certificates (e.g., cert-manager)" >&2
    exit 1
  fi
  echo "Generating server certificate (hostname: $KUIPER_SERVER_HOSTNAME)..."
  kuiper-forge --data-dir "$DATA_DIR" ca server-cert --hostname "$KUIPER_SERVER_HOSTNAME"
fi

# Inject --data-dir and --config before the subcommand
BIN="$1"
shift
exec "$BIN" --data-dir "$DATA_DIR" --config "$DATA_DIR/config.toml" "$@"
