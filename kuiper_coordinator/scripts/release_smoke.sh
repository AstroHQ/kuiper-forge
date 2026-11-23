#!/usr/bin/env bash
# Builds the prod release and boots it the way the runbook does (docs/verify-real-agents.md):
# migrate, then bin/server with the HTTP and agent gRPC listeners on their own ports, from an
# empty data dir: the first launch makes the CA and server cert there (D57). Passes when the HTTP
# port answers, the keys are 0600, and the agent port does TLS with a server cert from that CA.
#
#   DATABASE_URL  Postgres to migrate (default ecto://postgres:postgres@localhost:$PGPORT/kuiper_coordinator_smoke)
#   MIX           how to run mix (default mix), e.g. a wrapper that runs it in a container
#   SMOKE_HTTP_PORT / SMOKE_AGENT_PORT  default 19480 / 19543
set -euo pipefail

app_dir="$(cd "$(dirname "$0")/.." && pwd)"
MIX="${MIX:-mix}"
http_port="${SMOKE_HTTP_PORT:-19480}"
agent_port="${SMOKE_AGENT_PORT:-19543}"
export DATABASE_URL="${DATABASE_URL:-ecto://postgres:postgres@localhost:${PGPORT:-5432}/kuiper_coordinator_smoke}"

cd "$app_dir"
echo "== build the prod release"
MIX_ENV=prod $MIX deps.get --only prod >/dev/null
MIX_ENV=prod $MIX compile
MIX_ENV=prod $MIX assets.deploy
MIX_ENV=prod $MIX release --overwrite
rel="$app_dir/_build/prod/rel/kuiper_coordinator"

work="$(mktemp -d)"
server_pid=""
cleanup() {
  [ -n "$server_pid" ] && kill "$server_pid" 2>/dev/null || true
  "$rel/bin/kuiper_coordinator" eval 'KuiperCoordinator.Release.drop_db()' >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT

export SECRET_KEY_BASE="$(openssl rand -base64 48)"
export KUIPER_ENCRYPTION_KEY="$(openssl rand -base64 32)"
export PHX_HOST=localhost PORT="$http_port"
# agent gRPC is on by default (D58); the data dir doesn't exist yet
data="$work/data"
export KUIPER_AGENT_PORT="$agent_port" KUIPER_DATA_DIR="$data"
unset KUIPER_AGENT_GRPC KUIPER_CA_CERT KUIPER_CA_KEY KUIPER_SERVER_CERT KUIPER_SERVER_KEY

echo "== create the database and migrate"
"$rel/bin/kuiper_coordinator" eval 'KuiperCoordinator.Release.create_db()'
"$rel/bin/migrate"

echo "== bin/import_rust: a Rust SQLite DB with an env-only GitHub forge (the prod k8s shape)"
rust_db="$work/rust/kuiper.db"
mkdir -p "$work/rust"
python3 -I - "$app_dir/../kuiper-forge/migrations" "$rust_db" <<'PY'
import glob, os, sqlite3, sys
mig, db = sys.argv[1], sys.argv[2]
con = sqlite3.connect(db)
for d in ("shared", "sqlite"):
    for f in sorted(glob.glob(os.path.join(mig, d, "*.sql"))):
        con.executescript(open(f).read())
con.execute("""INSERT INTO registered_agents (agent_id, hostname, agent_type, labels, max_vms,
  serial_number, created_at, expires_at, revoked, revoked_at, agent_version)
  VALUES ('agent-smoke-rust', 'mac-mini-1', 'tart', '["macos"]', 2, '0a1b2c',
  '2026-01-02T03:04:05+00:00', '2099-01-01T00:00:00+00:00', 0, NULL, '0.9.1')""")
con.commit()
PY
openssl genrsa -out "$work/rust/app.pem" 2048 2>/dev/null
smoke_secret="smoke-webhook-$(openssl rand -hex 8)"
rust_env() {
  KUIPER_GITHUB__APP_ID=4242 KUIPER_GITHUB__PRIVATE_KEY="$(cat "$work/rust/app.pem")" \
  KUIPER_RUNNER_SCOPE__TYPE=organization KUIPER_RUNNER_SCOPE__NAME=AstroHQ \
  KUIPER_PROVISIONING__MODE=webhook KUIPER_PROVISIONING__WEBHOOK__SECRET="$smoke_secret" "$@"
}
import_out="$(rust_env "$rel/bin/import_rust" --sqlite "$rust_db" --dry-run && rust_env "$rel/bin/import_rust" --sqlite "$rust_db")"
echo "$import_out"
# the rerun finds both already here, unchanged
rerun="$(rust_env "$rel/bin/import_rust" --sqlite "$rust_db" --dry-run)"
echo "$rerun"
case "$import_out$rerun" in
  *"$smoke_secret"*|*"PRIVATE KEY"*) echo "bin/import_rust printed a secret"; exit 1 ;;
esac
for want in "agents: 1 unchanged" "forges: 1 unchanged"; do
  case "$rerun" in *"$want"*) ;; *) echo "bin/import_rust rerun didn't say '$want'"; exit 1 ;; esac
done
echo "bin/import_rust imported the env-only forge and the agent"

echo "== boot bin/server"
"$rel/bin/server" >"$work/server.log" 2>&1 &
server_pid=$!

ok=""
for _ in $(seq 1 60); do
  if curl -fs -o /dev/null "http://127.0.0.1:$http_port/"; then ok=1; break; fi
  kill -0 "$server_pid" 2>/dev/null || break
  sleep 1
done
if [ -z "$ok" ]; then
  echo "HTTP port $http_port never answered"; cat "$work/server.log"; exit 1
fi
echo "HTTP on $http_port answers"

for f in ca.crt ca.key server.crt server.key; do
  [ -f "$data/$f" ] || { echo "first launch didn't make $data/$f"; cat "$work/server.log"; exit 1; }
done
for f in ca.key server.key; do
  mode="$(stat -c %a "$data/$f" 2>/dev/null || stat -f %Lp "$data/$f")"
  [ "$mode" = 600 ] || { echo "$data/$f is mode $mode, want 600"; exit 1; }
done
echo "first launch made the CA and server cert in $data, keys 0600"

if ! openssl s_client -connect "127.0.0.1:$agent_port" -servername localhost \
  -CAfile "$data/ca.crt" -verify_return_error </dev/null 2>/dev/null | grep -q "Verify return code: 0"; then
  echo "agent port $agent_port didn't do TLS with the coordinator CA's server cert"
  cat "$work/server.log"; exit 1
fi
echo "agent gRPC TLS on $agent_port verifies against the coordinator CA"

# the runbook's token step, as written there
bundle="$("$rel/bin/kuiper_coordinator" rpc '{:ok, t} = KuiperCoordinator.Agents.create_registration_token(3600); IO.puts(KuiperCoordinator.Agents.registration_bundle(t, "https://localhost:'"$agent_port"'"))')"
case "$bundle" in
  kfr1_*) echo "registration bundle from bin/kuiper_coordinator rpc" ;;
  *) echo "rpc didn't print a registration bundle: $bundle"; cat "$work/server.log"; exit 1 ;;
esac

# rpc can reach the fleet's PubSub (what the switch-off step broadcasts; no agent exists here to switch off)
"$rel/bin/kuiper_coordinator" rpc 'Phoenix.PubSub.broadcast(KuiperCoordinator.PubSub, "agents", {:agent_availability, "agent_smoke", false})'
echo SMOKE_OK
