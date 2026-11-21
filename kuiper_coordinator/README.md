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

Before committing, run `mix precommit` (compile with warnings as errors,
format, test).

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
