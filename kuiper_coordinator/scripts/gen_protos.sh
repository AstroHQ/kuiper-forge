#!/usr/bin/env bash
# Regenerate the Elixir protobuf/gRPC modules from the Rust workspace's proto
# files (kuiper-agent-proto/proto is the contract; never edit the output).
# Needs protoc on PATH. Installs the protoc-gen-elixir escript at the version
# pinned in mix.lock if it isn't there already. Run via `mix proto.gen`.
set -euo pipefail

cd "$(dirname "$0")/.."
PROTO_DIR=../kuiper-agent-proto/proto
OUT_DIR=lib/kuiper_coordinator/proto

version=$(sed -n 's/^ *"protobuf": {:hex, :protobuf, "\([^"]*\)".*/\1/p' mix.lock)
[ -n "$version" ] || { echo "protobuf not found in mix.lock" >&2; exit 1; }

escripts="${MIX_HOME:-$HOME/.mix}/escripts"
export PATH="$escripts:$PATH"
if [ "$(cat "$escripts/.protoc-gen-elixir.version" 2>/dev/null)" != "$version" ] ||
  ! command -v protoc-gen-elixir >/dev/null; then
  mix escript.install hex protobuf "$version" --force
  echo "$version" >"$escripts/.protoc-gen-elixir.version"
fi

rm -rf "$OUT_DIR"
mkdir -p "$OUT_DIR"
protoc -I "$PROTO_DIR" --elixir_out=plugins=grpc:"$OUT_DIR" "$PROTO_DIR"/agent.proto "$PROTO_DIR"/management.proto
mix format "$OUT_DIR/**/*.ex"
