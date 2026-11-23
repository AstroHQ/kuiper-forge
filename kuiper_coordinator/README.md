# kuiper_coordinator

An Elixir/Phoenix coordinator for kuiper-forge, built to do what the Rust
coordinator (`../kuiper-forge`) does: the agent gRPC + mTLS service, GitHub and
Forgejo runner provisioning, and the admin UI. It runs alongside the Rust
coordinator with its own Postgres database, and talks to the existing tart,
Proxmox and libvirt agents unchanged.

## Running it

Tool versions are pinned in `mise.toml` (Erlang/OTP 28, Elixir 1.19). You also
need Postgres (the dev and test configs expect `postgres`/`postgres` on
`localhost:5432`) and `protoc` if you regenerate the protos.

```sh
cd kuiper_coordinator
mise install
mix setup          # deps, database, assets
mix phx.server     # http://localhost:9480 (KUIPER_HTTP_PORT or PORT to change)
mix test
```

To reach the dev server from the internet (GitHub webhooks, the GitHub App
setup redirect), use ngrok in two terminals:

```sh
mise run ngrok          # tunnel to the HTTP port
mise run ngrok-server   # mix phx.server, with PHX_HOST set to the tunnel's host
```

`ngrok-server` reads the host from the local ngrok agent, so it works with any
ngrok account. Only HTTP goes through the tunnel; agents still connect to
`localhost:9543`.

## Ports

Two listeners, so it runs beside the Rust coordinator (9443):

- HTTP (admin UI, webhooks): `KUIPER_HTTP_PORT`, else `PORT`, else 9480.
- Agent gRPC (registration, AgentStream, UploadLogs), on by default
  (`KUIPER_AGENT_GRPC=false` or `0` turns it off): its own TLS port,
  `KUIPER_AGENT_PORT`, else 9543. Agent RPCs need a client cert signed by the
  coordinator CA (`KUIPER_CA_CERT`/`KUIPER_CA_KEY`); registration works without
  one. A gateway in front passes this port's TLS through.

### Certificates

The CA and server cert live in `KUIPER_DATA_DIR` (`/var/lib/kuiper-forge` in
prod; `kuiper_coordinator/.kuiper-data` in dev, gitignored, so `mix phx.server`
needs no sudo) as `ca.crt`, `ca.key`, `server.crt`, `server.key`, or wherever
`KUIPER_CA_CERT`, `KUIPER_CA_KEY`, `KUIPER_SERVER_CERT`, `KUIPER_SERVER_KEY`
point.

- Fresh install: nothing to do. When both CA files are missing, the first launch
  makes a CA like `kuiper-forge ca init` (P-256, 10 years, CN "<org> CA" with
  `KUIPER_CA_ORG`, default "Kuiper Forge"); when both server files are missing,
  it issues a server cert from that CA for `KUIPER_SERVER_HOSTNAME` (else
  `PHX_HOST`) plus localhost and 127.0.0.1, valid a year. Keys are written 0600.
- Sharing with the Rust coordinator: point the paths at its files, so agent
  certs it issued stay valid. Existing files are never overwritten.
- Half a pair (a cert without its key, or the other way round) or a key that
  doesn't match stops startup with a message naming the file.

`KUIPER_SERVER_CERT`/`KUIPER_SERVER_KEY` may be a cert from another CA (put its
intermediates after it in the same file) with `KUIPER_SERVER_TRUST_MODE=chain`
and `KUIPER_SERVER_CA_CERT` (the CA agents should trust) or
`KUIPER_SERVER_USE_NATIVE_ROOTS=true`. Agent client certs are still verified
against the coordinator CA.

### Management socket

`ManagementService` (`management.proto`: create, list and delete registration
tokens; list and revoke agents) listens on a Unix socket, on by default with
agent gRPC (`KUIPER_MANAGEMENT=true|false` overrides). It has no auth beyond the
socket's permissions: the socket is 0600 and a directory it creates is 0700.
The Rust coordinator uses `<data_dir>/coordinator.sock`, so Phoenix defaults to
`<data_dir>/phoenix-coordinator.sock` (in dev, `kuiper-phoenix-<hash of the
data dir>.sock` in the temp dir, since the in-repo data dir is too deep for a Unix socket path;
`KUIPER_MANAGEMENT_SOCKET` to change it; a path over 103 bytes stops startup)
and refuses to start on a socket something still answers on; a stale one is
replaced. The Rust CLI always dials `<--data-dir>/coordinator.sock`, so to use
it with Phoenix, point `KUIPER_MANAGEMENT_SOCKET` at `<dir>/coordinator.sock`
in a directory of its own and run `kuiper-forge --data-dir <dir> token create`.

Before committing, run `mix precommit` (compile with warnings as errors,
format, test).

## Configuration

Env vars only bootstrap the app: `DATABASE_URL`, `SECRET_KEY_BASE`, `PHX_HOST`,
`PORT` (HTTP, default 9480), `KUIPER_AGENT_GRPC` / `KUIPER_AGENT_PORT` (agent
gRPC, default 9543), the CA and server cert paths (`KUIPER_DATA_DIR` or
`KUIPER_CA_CERT`, `KUIPER_CA_KEY`, `KUIPER_SERVER_CERT`, `KUIPER_SERVER_KEY`),
and `KUIPER_ENCRYPTION_KEY` (required in prod; `openssl rand -base64 32`).

Forges (GitHub Apps, Forgejo instances) and the provisioning settings they share
(idle timeout, scan lookback, required labels, webhook path, label mappings)
are stored in Postgres and edited in the admin UI (`KuiperCoordinator.Forges`).
Secrets among them (GitHub App private keys, Forgejo tokens, webhook secrets)
are encrypted with `KUIPER_ENCRYPTION_KEY`; lose it and they have to be entered
again. A forge is checked against its API when it's saved and rejected if it
doesn't work; every change reloads the fleet without a restart. With no forge
stored, nothing is provisioned.

### Provisioning modes

Each forge has its own mode, set on the forge's page, and forges in different
modes run side by side. This differs from the Rust coordinator, which has one
`[provisioning] mode` for all of them.

- **Webhook**: jobs come from the forge's deliveries. It needs a webhook secret.
  Every forge in this mode delivers to the same path (`/webhook` by default);
  the secret that signed a delivery says which forge it's from. A scan every
  5 minutes picks up deliveries that were missed.
- **Polling** (the default): jobs come from scanning the forge every poll
  interval (120 s by default, at least 10).
- **Fixed capacity** (GitHub only): the agents' pools are kept full of runners
  registered in the forge's runner scope (an org or `owner/repo`). The pools
  name no forge, so one forge at most can be in this mode.

A GitHub App made from the admin UI starts in webhook mode when its webhook box
is checked, else in polling mode.

## Moving over from the Rust coordinator

`mix kuiper.import_rust` copies what a switch-over needs from a Rust coordinator, so nothing
has to register again:

```sh
mix kuiper.import_rust --sqlite /var/lib/kuiper/kuiper.db --config /etc/kuiper/config.toml --dry-run
mix kuiper.import_rust --sqlite /var/lib/kuiper/kuiper.db --config /etc/kuiper/config.toml
```

In a release, `bin/import_rust` takes the same flags, so a k8s Job or initContainer with the Rust
data volume mounted can run it:

```sh
bin/import_rust --sqlite /data/kuiper.db --dry-run
bin/import_rust --from postgres://kuiper@db/kuiper --config /etc/kuiper/config.toml
```

- `--sqlite PATH` or `--from sqlite://PATH|postgres://USER:PASS@HOST/DB` is the Rust database.
  It's only read.
- The forge config is built the way the Rust coordinator builds it: `--config` (optional), with
  the Rust `KUIPER_X__Y` environment on top (`KUIPER_GITHUB__APP_ID`, `KUIPER_GITHUB__PRIVATE_KEY`,
  `KUIPER_RUNNER_SCOPE__TYPE`, `KUIPER_PROVISIONING__MODE`, `KUIPER_PROVISIONING__WEBHOOK__SECRET`,
  ...). An env-configured install (no forge in config.toml) imports from its env block alone; run
  the import with the Rust coordinator's env. With neither, only the database is imported.
- From the database: registered agents (revoked ones stay revoked; Phoenix loads the same CA, so
  existing agent certs keep working), unexpired registration tokens, admin users (the Rust
  Argon2id hashes log in as they are; TOTP secrets aren't imported) and API tokens (same SHA-256,
  so Management clients keep their tokens).
- From the config: forges with their secrets (`private_key`, `private_key_path`, `token_env`,
  `token_path` are read the way the Rust coordinator reads them),
  provisioning settings and label mappings. The one global mode becomes a mode per forge: in
  webhook mode the webhook forge takes deliveries and the others poll; in fixed capacity the
  first forge fills the pools and the others poll. The dry run lists each forge with its mode.
- Not imported: runners, pending jobs, sessions, agent logs, failures and run history. They're
  short-lived and rebuild themselves.
- `--dry-run` prints the counts and writes nothing. A real run writes everything in one
  transaction.
- Rerunning is safe. What Phoenix already has is left alone, and if it differs it's reported as
  a conflict (`bin/import_rust` exits 2 then, 1 on an error). The one change a rerun makes is a revocation
  done on the Rust side since. What Phoenix removed after an import, like a used registration
  token or a deleted user, isn't brought back.
- The output never shows a secret: tokens show as their prefix.

## Admin UI

Always on (login required); served at `/admin` on the HTTP port, with the
read-only JSON API at `/api/v1` (bearer `kfapi_` tokens made on the API tokens
page). On a fresh install, with no admin users yet, every `/admin` page goes to
`/admin/setup`, which creates the first account and logs it in; once any user exists that page
is gone (404). The CLI stays for recovery: `KUIPER_ADMIN_PASSWORD=... bin/kuiper_coordinator
eval 'KuiperCoordinator.Release.create_admin("admin")'` (and `reset_admin_password/1`,
`list_admins/0`).

- `KUIPER_ADMIN_SESSION_TIMEOUT`: session lifetime in seconds (3600), never extended.
- `KUIPER_COORDINATOR_URL`: agent-facing URL put in registration bundles.
- `KUIPER_METRICS_RETENTION_DAYS`: run history kept for the metrics page (90).
- `KUIPER_UPDATE_CHECK=false`: stop checking GitHub releases for newer agents
  (on by default; agents behind the newest release get an "available" badge).

For mock metrics history in dev: `mix run priv/repo/seed_metrics.exs [--days 7]`
(`--clear` removes it again).

## Protos

`../kuiper-agent-proto/proto/{agent,management}.proto` is the contract with the
agents. The Elixir modules under `lib/kuiper_coordinator/proto/`
(`Cirunner.Agent.*`, `Cirunner.Management.*`) are generated from it and checked
in. Don't edit them by hand; after the proto changes, run:

```sh
mix proto.gen
```

It runs `scripts/gen_protos.sh`, which installs the `protoc-gen-elixir` escript
at the `protobuf` version in `mix.lock` (if needed), runs `protoc` and formats
the output. CI (`.github/workflows/phoenix.yml`) regenerates them and fails if
the checked-in modules are out of date.
