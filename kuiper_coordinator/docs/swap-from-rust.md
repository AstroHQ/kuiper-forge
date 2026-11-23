# Swapping the Rust coordinator for Phoenix

The Phoenix coordinator is a drop-in swap for the Rust one (D72). Agents (tart, proxmox,
libvirt) don't change: they keep dialing the same host on port 9443, and their client certs keep
working because Phoenix loads the same CA from the same `/data` volume. The Rust coordinator's
data is only read, so going back is stopping Phoenix and starting Rust again.

What changes:

| | Rust | Phoenix |
|---|---|---|
| Agent gRPC (mTLS) | 9443 | 9443 (the image sets `KUIPER_GRPC__LISTEN_ADDR=0.0.0.0:9443`) |
| Admin UI (`/admin`), webhooks (`/webhook`), API | 9443, same TLS port | 9480, plain HTTP; TLS at your ingress (D21) |
| Database | SQLite in `/data/kuiper.db`, or Postgres | Postgres only, its own database (`DATABASE_URL`) |
| Forges, provisioning | `config.toml` / `KUIPER_GITHUB__*` env | Postgres, edited in the admin UI; `import_rust` copies them once |
| New secrets | | `SECRET_KEY_BASE` (`openssl rand -base64 48`), `KUIPER_ENCRYPTION_KEY` (`openssl rand -base64 32`) |

The Rust env block carries over as it is: `KUIPER_TLS__*`, `KUIPER_GRPC__LISTEN_ADDR`,
`KUIPER_GRPC__PROXY_PROTOCOL`, `KUIPER_ADMIN__*`, `KUIPER_METRICS__*` and `KUIPER_DATABASE__*`
(Postgres) are read as aliases for Phoenix's own names (D73), and the ones used are logged at
boot by name. Phoenix's own names win when both are set. `KUIPER_SERVER_HOSTNAME` and
`KUIPER_CA_ORG` work as they do for the Rust image's first launch.

## The image

`kuiper_coordinator/Dockerfile` builds `ghcr.io/astrohq/kuiper-coordinator` (docker.yml publishes
it with each `kuiper-forge-v*` tag, beside `ghcr.io/astrohq/kuiper-forge`). It runs as the Rust
image's `kuiper` user (uid/gid 999), so files the Rust image wrote to `/data` stay readable, and
sets `KUIPER_DATA_DIR=/data`.

| Command | Does |
|---|---|
| *(none)* / `start` | migrate (creating the database if it's missing), then serve |
| `migrate` | only migrate |
| `server` | serve without migrating |
| `import_rust --sqlite /data/kuiper.db [--dry-run]` | copy agents, tokens, admins, API tokens and forges from Rust |

On a first launch with an empty `/data` it makes the CA and server cert there, as the Rust
entrypoint does.

## Postgres

`DATABASE_URL` takes a provider's connection string as it is. Ecto alone ignores libpq's
`sslmode`, so Phoenix reads it:

- `DATABASE_CA_CERT=/path/ca-certificate.crt`: TLS, verifying the server cert and hostname
  against that CA. Use this for DigitalOcean managed Postgres (D77): download the CA from the
  cluster's page and mount it (a Secret or ConfigMap).
- `?sslmode=verify-full` or `verify-ca` (or Ecto's `?ssl=true`): TLS verified against the system CAs.
- `?sslmode=require` (DigitalOcean's default string): TLS without verifying, as libpq does.
- no `sslmode`, or `disable`: plain TCP.

For DigitalOcean, add the cluster's IP range (or the k8s cluster) to the database's trusted
sources, and create the database (`kuiper_phoenix`) and a user for it in the control panel;
`bin/migrate` also creates a missing database when the user may.

With the Rust coordinator on Postgres and no `DATABASE_URL`, Phoenix makes one from
`KUIPER_DATABASE__HOST/PORT/USER/PASSWORD` for a new database named `<KUIPER_DATABASE__DATABASE>_phoenix`
on the same server (`KUIPER_PHOENIX_DATABASE` names it). Phoenix never writes to Rust's database.

## The import

Run it once, with the Rust coordinator stopped (so nothing changes under it) and the Rust env
block in the environment, so env-configured forges (`KUIPER_GITHUB__*`, `KUIPER_PROVISIONING__*`)
come across:

```sh
import_rust --sqlite /data/kuiper.db --config /data/config.toml --dry-run   # counts, forges, modes
import_rust --sqlite /data/kuiper.db --config /data/config.toml
```

`--config` is optional. It exits 2 on conflicts (what Phoenix already has and differs), 1 on an
error. Rerunning is safe. See the README's import section for what's copied.

## Docker (compose)

`deploy/docker-compose.yml` is the swap of `kuiper-forge/docker/docker-compose.yml`:

1. `docker compose stop` in `kuiper-forge/docker/` (the Rust project, its Postgres too). Keep its
   volumes: the Phoenix file mounts them as external `docker_pgdata` and `docker_kuiper-data`
   (the names Compose gave them from that folder; check with `docker volume ls`), so its Postgres
   is the Rust one's, with the Rust tables for the import.
2. Point the service at the Phoenix image, keep its `/data` volume and env, add `SECRET_KEY_BASE`,
   `KUIPER_ENCRYPTION_KEY`, `PHX_HOST` and `DATABASE_URL` (or keep `KUIPER_DATABASE__*`), and
   publish `9480` for the admin UI beside `9443`.
3. `docker compose run --rm kuiper-coordinator import_rust --sqlite /data/kuiper.db --dry-run`, then
   without `--dry-run` (with Rust on Postgres: `--from postgres://...`).
4. `docker compose up -d kuiper-coordinator`. Agents reconnect on their own.
5. Point the forges' webhook URL at the HTTP port (`https://<host>/webhook` through your proxy) and
   log in at `/admin` with the Rust admin users (their passwords carry over; TOTP is set again).

Going back: `docker compose -f deploy/docker-compose.yml stop`, then start the Rust project
again. Phoenix's tables are in their own database (`kuiper_forge_phoenix`), apart from Rust's.

## Kubernetes

`deploy/k8s/` mirrors the Rust coordinator's manifests (astro-remote-infrastructure
`kuiper-forge/`, D76): a StatefulSet with the same `/data` PVC and env block, Traefik in front
with PROXY protocol on the agent port. Apply them in your gitops repo, not from here.

- `statefulset.yaml`: the Rust env block unchanged, plus `DATABASE_URL`, `DATABASE_CA_CERT`,
  `SECRET_KEY_BASE` and `KUIPER_ENCRYPTION_KEY` from a Secret; ports 9443 and 9480; the same
  `volumeClaimTemplates` name so the pod gets the Rust coordinator's PVC. `PHX_HOST` is the
  admin route's host (GitHub App and webhook URLs are made from it); `KUIPER_SERVER_HOSTNAME`
  stays the agents' host, the server cert's name. The readiness probe sends
  `X-Forwarded-Proto: https` so prod's HTTPS redirect doesn't turn it away.
- `service.yaml`: 9443 (agents) and 9480 (HTTP).
- `ingressroute.yaml`: a Traefik `IngressRouteTCP` passing TLS through to 9443 with PROXY v2
  (as the Rust coordinator's), and an `IngressRoute` for `/admin`, `/webhook` and `/api` on 9480.
- `import-job.yaml`: the one-time import, mounting the same PVC.

The swap:

1. `kubectl scale statefulset kuiper-forge --replicas=0` and wait for the pod to go (the PVC is
   ReadWriteOnce).
2. Create the `kuiper-coordinator` Secret (database URL, CA cert, keys).
3. Apply `import-job.yaml` with `--dry-run` first; read its log; then without it.
4. Apply the StatefulSet, Service and routes. Agents reconnect through the same route.
5. Move the forges' webhook URL if its host changed, and check `/admin`.

The Phoenix StatefulSet keeps the name `kuiper-forge` so its pod claims the Rust PVC
(`data-kuiper-forge-0`). Going back: scale it to 0 and apply the Rust StatefulSet again.

## Manual installs

Build the release (`MIX_ENV=prod mix release`, or `scripts/release_smoke.sh` to try it) and run
it with the same env, `KUIPER_DATA_DIR` pointing at the Rust coordinator's data dir (its
`--data-dir`, which holds `ca.crt`), and `KUIPER_AGENT_PORT=9443` (or `KUIPER_GRPC__LISTEN_ADDR`):

```sh
_build/prod/rel/kuiper_coordinator/bin/migrate
_build/prod/rel/kuiper_coordinator/bin/import_rust --sqlite /var/lib/kuiper/kuiper.db --config /etc/kuiper/config.toml
_build/prod/rel/kuiper_coordinator/bin/server
```

Stop the Rust coordinator first: only one of them can own port 9443. Running both at once, on
different ports, works for trying Phoenix out, but agents talk to one coordinator.

## Checking it

`scripts/docker_smoke.sh` (CI's "Docker image" job) boots the image with the Rust image's env
names and no `KUIPER_DATA_DIR`, against a Postgres that only takes TLS reached with
`?sslmode=require`, and checks the default command migrates and serves, 9443 does TLS from the
CA made in `/data` (owned by 999:999, mode 600), and a restart keeps that CA.
