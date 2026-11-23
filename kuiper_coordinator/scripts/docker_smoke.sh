#!/usr/bin/env bash
# Boots the Phoenix image the way a Rust deployment would swap to it (D72): the Rust image's env
# names (KUIPER_TLS__*, KUIPER_GRPC__PROXY_PROTOCOL, no KUIPER_DATA_DIR) and its /data volume,
# against a Postgres that only takes TLS, reached by a managed-Postgres style URL with
# ?sslmode=require (D77). Passes when the default command migrates and serves, HTTP answers on
# 9480, the agent port (9443, from the image's KUIPER_GRPC__LISTEN_ADDR) does TLS with a server
# cert from the CA made in /data, and a restart keeps that CA.
#
#   IMAGE   image to test (default: build kuiper_coordinator/ as kuiper-coordinator:smoke)
set -euo pipefail

app_dir="$(cd "$(dirname "$0")/.." && pwd)"
id="kpc-smoke-$$"
net="$id-net"
vol="$id-data"
work="$(mktemp -d)"
chmod 0755 "$work"

cleanup() {
  docker rm -f "$id-app" "$id-pg" >/dev/null 2>&1 || true
  docker volume rm "$vol" >/dev/null 2>&1 || true
  docker network rm "$net" >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT

if [ -z "${IMAGE:-}" ]; then
  IMAGE=kuiper-coordinator:smoke
  echo "== build $IMAGE"
  docker build -q -t "$IMAGE" "$app_dir" >/dev/null
fi

docker network create "$net" >/dev/null
docker volume create "$vol" >/dev/null

echo "== Postgres that rejects plain TCP"
openssl req -x509 -newkey rsa:2048 -nodes -days 1 -subj /CN=pg -keyout "$work/pg.key" -out "$work/pg.crt" 2>/dev/null
cat >"$work/hba.conf" <<HBA
local all all trust
hostssl all all all scram-sha-256
HBA
# a throwaway key, readable so the postgres user can copy it to a 0600 file of its own
chmod 0644 "$work/pg.crt" "$work/pg.key" "$work/hba.conf"
docker run -d --name "$id-pg" --network "$net" -e POSTGRES_PASSWORD=pw -v "$work:/certs:ro" \
  --entrypoint sh postgres:17 -c \
  'install -o postgres -m 600 /certs/pg.key /certs/pg.crt /certs/hba.conf /tmp/ &&
   exec docker-entrypoint.sh postgres -c ssl=on -c ssl_cert_file=/tmp/pg.crt -c ssl_key_file=/tmp/pg.key -c hba_file=/tmp/hba.conf' \
  >/dev/null
for _ in $(seq 1 60); do
  # over TCP: the image's first-run init server only listens on its Unix socket
  docker exec "$id-pg" pg_isready -h 127.0.0.1 -U postgres >/dev/null 2>&1 && break
  sleep 1
done

# the Rust image's env, as a k8s StatefulSet or compose file sets it, plus what Phoenix needs
app_env=(
  -e KUIPER_TLS__CA_CERT=/data/ca.crt -e KUIPER_TLS__CA_KEY=/data/ca.key
  -e KUIPER_TLS__SERVER_CERT=/data/server.crt -e KUIPER_TLS__SERVER_KEY=/data/server.key
  -e KUIPER_GRPC__PROXY_PROTOCOL=true -e KUIPER_CA_ORG=Smoke -e KUIPER_SERVER_HOSTNAME=localhost
  -e KUIPER_NO_LOG_FILE=true -e KUIPER_CONFIG=/data/config.toml
  -e "DATABASE_URL=postgresql://postgres:pw@$id-pg:5432/kuiper_phoenix?sslmode=require"
  -e "SECRET_KEY_BASE=$(openssl rand -base64 48)" -e "KUIPER_ENCRYPTION_KEY=$(openssl rand -base64 32)"
  -e PHX_HOST=localhost
)

boot() {
  docker rm -f "$id-app" >/dev/null 2>&1 || true
  docker run -d --name "$id-app" --network "$net" -v "$vol:/data" -p 127.0.0.1::9480 -p 127.0.0.1::9443 \
    "${app_env[@]}" "$IMAGE" >/dev/null
  http_port="$(docker port "$id-app" 9480 | head -1 | cut -d: -f2)"
  agent_port="$(docker port "$id-app" 9443 | head -1 | cut -d: -f2)"
  for _ in $(seq 1 90); do
    if curl -fs -o /dev/null "http://127.0.0.1:$http_port/"; then return 0; fi
    [ "$(docker inspect -f '{{.State.Running}}' "$id-app")" = true ] || break
    sleep 1
  done
  echo "HTTP never answered"; docker logs "$id-app" 2>&1 | tail -40; return 1
}

ca_fingerprint() {
  docker run --rm -v "$vol:/data" --entrypoint cat "$IMAGE" /data/ca.crt | openssl x509 -noout -fingerprint -sha256
}

agent_tls() {
  docker run --rm -v "$vol:/data" --entrypoint cat "$IMAGE" /data/ca.crt >"$work/ca.crt"
  echo | openssl s_client -connect "127.0.0.1:$agent_port" -servername localhost -CAfile "$work/ca.crt" \
    -verify_return_error 2>&1 | grep -q "Verify return code: 0 (ok)"
}

echo "== first boot: default command migrates (creating the database) and serves"
boot
echo "HTTP on 9480 answers"
# the k8s readiness probe (deploy/k8s/statefulset.yaml) hits the pod IP over plain HTTP: prod's
# force_ssl redirects that, and X-Forwarded-Proto: https (the probe's header) gets a real answer
probe() { curl -s -o /dev/null -w '%{http_code}' -H "Host: 10.244.0.7:9480" "$@" "http://127.0.0.1:$http_port/"; }
[ "$(probe)" = 301 ] || { echo "plain HTTP by pod IP wasn't redirected: $(probe)"; exit 1; }
code="$(probe -H 'X-Forwarded-Proto: https')"
case "$code" in 2* | 302) ;; *) echo "probe with X-Forwarded-Proto got $code"; exit 1 ;; esac
echo "readiness probe's X-Forwarded-Proto header gets $code, not the HTTPS redirect"
docker logs "$id-app" 2>&1 | grep -q "KUIPER_GRPC__PROXY_PROTOCOL" || { echo "PROXY alias not used"; exit 1; }
agent_tls || { echo "agent port 9443 didn't do TLS with a cert from /data/ca.crt"; exit 1; }
echo "agent port 9443 does TLS from the CA made in /data"
owner="$(docker run --rm -v "$vol:/data" --entrypoint stat "$IMAGE" -c '%u:%g %a' /data/ca.key)"
[ "$owner" = "999:999 600" ] || { echo "ca.key is $owner, want 999:999 600"; exit 1; }
echo "ca.key is the Rust image's kuiper user's (999:999), mode 600"
first="$(ca_fingerprint)"

echo "== migrate runs on its own"
docker run --rm --network "$net" "${app_env[@]}" "$IMAGE" migrate >/dev/null

echo "== import_rust reaches the release's importer"
set +e
out="$(docker run --rm --network "$net" "${app_env[@]}" "$IMAGE" import_rust 2>&1)"; rc=$?
set -e
[ "$rc" = 1 ] && grep -q -- "--sqlite PATH" <<<"$out" || { echo "import_rust: exit $rc, want 1 with usage: $out"; exit 1; }
echo "import_rust without a database exits 1 with its usage"

echo "== restart keeps the CA"
boot
agent_tls || { echo "agent TLS failed after restart"; exit 1; }
[ "$(ca_fingerprint)" = "$first" ] || { echo "CA changed on restart"; exit 1; }
echo "same CA after restart"

echo "docker smoke: OK"
