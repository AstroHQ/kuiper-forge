#!/usr/bin/env bash
# starts the dev server with the ngrok tunnel's host as its public url. run `mise run ngrok` first, or set PHX_HOST
set -euo pipefail

cd "$(dirname "$0")/.."

port="${KUIPER_HTTP_PORT:-${PORT:-9480}}"
ngrok_api="${NGROK_API:-http://127.0.0.1:4040}"

# ask the local ngrok agent for the tunnel to our port, so nobody's domain is hard-coded in the repo
if [[ -z "${PHX_HOST:-}" ]]; then
  tunnels="$(curl -fsS "$ngrok_api/api/tunnels" 2>/dev/null)" || {
    echo "error: no ngrok agent on $ngrok_api, run \`mise run ngrok\` in another terminal first" >&2
    exit 1
  }

  PHX_HOST="$(jq -r --arg p ":$port" \
    '[.tunnels[] | select(.config.addr | endswith($p)) | .public_url][0] // "" | sub("^https?://"; "")' <<<"$tunnels")"

  # an agent that is running for some other project has no tunnel to this port
  [[ -n "$PHX_HOST" ]] || {
    echo "error: the ngrok agent on $ngrok_api has no tunnel to port $port" >&2
    exit 1
  }
fi

echo "public url: https://$PHX_HOST"

# `PHX_HOST` is also the name the agent gRPC cert is checked against, and the dev cert is only for localhost. the
# tunnel carries http only, so agents keep using localhost
export PHX_HOST KUIPER_SERVER_HOSTNAME="${KUIPER_SERVER_HOSTNAME:-localhost}"

exec mix phx.server
