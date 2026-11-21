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

## Ports

Two listeners, so it runs beside the Rust coordinator (9443):

- HTTP (admin UI, webhooks): `KUIPER_HTTP_PORT`, else `PORT`, else 9480.
- Agent gRPC (registration, AgentStream, UploadLogs), with `KUIPER_AGENT_GRPC=true`:
  its own TLS port, `KUIPER_AGENT_PORT`, else 9543. Agent RPCs need a client cert
  signed by the coordinator CA (`KUIPER_CA_CERT`/`KUIPER_CA_KEY`, the Rust
  coordinator's `ca.crt`/`ca.key`); registration works without one. A gateway in
  front passes this port's TLS through.

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
`<data_dir>/phoenix-coordinator.sock` (`KUIPER_MANAGEMENT_SOCKET` to change it)
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

Forges (GitHub Apps, Forgejo instances) and provisioning settings (mode, runner
scope and group, poll interval, idle timeout, required labels, label mappings)
are stored in Postgres and edited in the admin UI (`KuiperCoordinator.Forges`).
Secrets among them (GitHub App private keys, Forgejo tokens, webhook secrets)
are encrypted with `KUIPER_ENCRYPTION_KEY`; lose it and they have to be entered
again. A forge is checked against its API when it's saved and rejected if it
doesn't work; every change reloads the fleet without a restart. With no forge
stored, nothing is provisioned.

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
