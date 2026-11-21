# Verifying the Phoenix coordinator with real agents

How to run the Phoenix coordinator next to the Rust coordinator on the same CA and point one
tart, one Proxmox and one libvirt agent at it, unchanged, then check it behaves like Rust. The
IDs in the checklists (A8, S9, F16, ...) are rows of [docs/phoenix-parity.md](../../docs/phoenix-parity.md).

`scripts/release_smoke.sh` (CI job "Release smoke test") runs steps 2-4 against a throwaway CA,
so if this page and the app drift apart, CI fails.

## 1. What runs where

| | Rust coordinator | Phoenix coordinator |
|---|---|---|
| Agent gRPC (TLS) | 9443 | `KUIPER_AGENT_PORT`, 9543 |
| HTTP (admin, webhooks) | its own | `PORT`, 9480 |
| Database | its own | its own Postgres DB (`DATABASE_URL`) |
| CA, server cert | `/var/lib/kuiper-forge/{ca,server}.{crt,key}` | the same files, read only |

Both listen side by side; never put them on one port. An agent talks to one coordinator at a
time: moving it to Phoenix and back is a re-register each way (section 5).

To keep the two from taking each other's jobs, give the test agents an extra label,
`phoenix-verify`, and run test workflows on `runs-on: [self-hosted, phoenix-verify]`. Rust's
agents can't take those jobs (they lack the label), and Phoenix only takes them
(the required labels setting, section 3).

## 2. Build the release

On the coordinator host (or a machine with the same OS and OpenSSL), with Erlang 28.5 and
Elixir 1.19.6 (`mise install` in `kuiper_coordinator/` reads `mise.toml`):

```sh
cd kuiper-forge/kuiper_coordinator
export MIX_ENV=prod
mix deps.get --only prod
mix compile
mix assets.deploy
mix release --overwrite
# the release is self-contained (it bundles ERTS); copy it where you run it
sudo mkdir -p /opt/kuiper-phoenix && sudo cp -a _build/prod/rel/kuiper_coordinator/. /opt/kuiper-phoenix/
```

## 3. Configure it

A Postgres database for Phoenix alone (any Postgres 14+; not the Rust coordinator's):

```sh
sudo -u postgres psql -c "CREATE ROLE kuiper_phoenix LOGIN PASSWORD 'change-me'"
sudo -u postgres psql -c "CREATE DATABASE kuiper_phoenix OWNER kuiper_phoenix"
```

`/etc/kuiper-phoenix.env` (mode 0600, readable by the user that runs it, who also needs read
access to the Rust coordinator's CA and server key):

```sh
DATABASE_URL=ecto://kuiper_phoenix:change-me@localhost/kuiper_phoenix
SECRET_KEY_BASE=<output of: openssl rand -base64 48>
PHX_HOST=coordinator.example.com          # the name agents use; must match server.crt
PORT=9480
KUIPER_AGENT_GRPC=true
KUIPER_AGENT_PORT=9543
KUIPER_DATA_DIR=/var/lib/kuiper-forge     # ca.crt, ca.key, server.crt, server.key: the Rust coordinator's
# chain-mode server certs: KUIPER_SERVER_TRUST_MODE=chain plus KUIPER_SERVER_CA_CERT or
# KUIPER_SERVER_USE_NATIVE_ROOTS=true, as set on the Rust side
```

Forges (the GitHub App, Forgejo URL and token) and provisioning settings (mode, scope,
required labels, poll interval, idle timeout) aren't env vars: they're stored in Postgres and
set in the admin UI (see the README once the forge storage work lands). For the GitHub checks
use fixed capacity; for the Forgejo checks, polling. Set the required labels to
`self-hosted,phoenix-verify`, and use a GitHub scope or a repo that the Rust coordinator isn't
also serving with webhooks for the same labels, or the two race for the job.

## 4. Start it

```sh
set -a; . /etc/kuiper-phoenix.env; set +a
K=/opt/kuiper-phoenix/bin
$K/kuiper_coordinator eval 'KuiperCoordinator.Release.create_db()'   # no-op if it exists
$K/migrate
$K/server            # foreground; or: $K/kuiper_coordinator daemon
```

Check both listeners from another shell (the agent port must verify against the Rust CA):

```sh
curl -fsS -o /dev/null -w '%{http_code}\n' http://127.0.0.1:9480/
openssl s_client -connect 127.0.0.1:9543 -servername "$PHX_HOST" \
  -CAfile /var/lib/kuiper-forge/ca.crt </dev/null 2>/dev/null | grep 'Verify return code'
# want: 200, and "Verify return code: 0 (ok)"
```

Handy commands on the running node (`$K/kuiper_coordinator rpc '...'`, same env loaded):

```sh
# a registration bundle (single use, 1 h)
$K/kuiper_coordinator rpc '{:ok, t} = KuiperCoordinator.Agents.create_registration_token(3600); IO.puts(KuiperCoordinator.Agents.registration_bundle(t, "https://coordinator.example.com:9543"))'
# switch an agent off (no new runners, idle ones drained), and on again; the admin UI's switch replaces this once it lands
$K/kuiper_coordinator rpc 'Phoenix.PubSub.broadcast(KuiperCoordinator.PubSub, "agents", {:agent_availability, "AGENT_ID", false})'
$K/kuiper_coordinator rpc 'Phoenix.PubSub.broadcast(KuiperCoordinator.PubSub, "agents", {:agent_availability, "AGENT_ID", true})'
# revoke
$K/kuiper_coordinator rpc 'IO.inspect(KuiperCoordinator.Agents.revoke_agent("AGENT_ID"))'
```

And the database: `psql "$DATABASE_URL"` with
`select agent_id, hostname, labels, revoked_at from agents;` and
`select runner_name, agent_id, vm_name, job_key, pool, inserted_at from runners;`.

## 5. Move an agent to Phoenix, and back

The agents don't change: only the coordinator URL (carried by the bundle) and the
registration token do. `register` overwrites the agent's certs and its config file, so save
both first. The agent's identity files are `ca.crt`, `client.crt`, `client.key`,
`metadata.json` (its `agent_id`) and `server_trust.mode`.

| Agent | Config | Certs dir | Register |
|---|---|---|---|
| tart (macOS) | `~/Library/Application Support/kuiper-tart-agent/config.toml`, or `-c PATH` | `~/Library/Application Support/kuiper-tart-agent/certs` | `kuiper-tart-agent -c CONFIG register kfr1_...` |
| Proxmox | `/etc/kuiper-proxmox-agent/config.toml` (`--system`) | `/var/lib/kuiper-proxmox-agent/certs` | `kuiper-proxmox-agent --system register kfr1_...` |
| libvirt | `/etc/kuiper-libvirt-agent/config.toml` (`--system`) | `/var/lib/kuiper-libvirt-agent/certs` | `kuiper-libvirt-agent --system register kfr1_...` |

For each agent (Proxmox shown; same for the others with their paths):

```sh
sudo systemctl stop kuiper-proxmox-agent           # or however it runs on that host
sudo cp -a /etc/kuiper-proxmox-agent/config.toml /etc/kuiper-proxmox-agent/config.toml.rust
sudo cp -a /var/lib/kuiper-proxmox-agent/certs /var/lib/kuiper-proxmox-agent/certs.rust
sudo kuiper-proxmox-agent --system register kfr1_...   # the bundle from section 4
# add phoenix-verify to [agent] labels (or to the pool's label set) in the config, then
sudo systemctl start kuiper-proxmox-agent
```

Back to Rust: stop it, restore `config.toml.rust` and `certs.rust` over the originals, start
it. (Or register again with a bundle from `coordinator token create`.)

## 6. Checklist, per agent

Do each check on the tart, the Proxmox and the libvirt agent. Logs are the Phoenix server's
unless an agent's are named.

| Check | How | Expect | Parity |
|---|---|---|---|
| Register | `register` with a fresh bundle | agent stores a cert; `agents` row with `agent_<type>_<host>_<8>`; reusing the same bundle fails | A8-A12 |
| Stream | start the agent | log `agent ... connected`; status within 30 s; it stays connected past several 30 s pings | S1-S5 |
| Existing certs | restart the agent (no register) | it reconnects with its stored cert, same agent_id | A15 |
| GitHub job | fixed capacity; push a workflow on `[self-hosted, phoenix-verify]` | pool fills to the agent's pool size (`runners` rows written before CreateRunner); the job runs on a VM of this agent; after it, the pool is topped up | P2-P4, F3, F5 |
| Forgejo job | polling mode; push a workflow on the test repo | within one poll a one-job runner starts, runs the job and is removed (`runners` row gone) | J5-J6, FJ1-FJ5, F7 |
| Idle teardown | Forgejo: queue a job, then cancel it before it starts | the runner is destroyed after the idle timeout setting (clock from when it started) and not straight back | F13-F15 |
| Reused VMID destroy (Proxmox) | while a runner runs, note its VM name; let it finish | agent log shows DestroyRunner for exactly that VM name; a VMID reused by a newer VM isn't destroyed | F8 (#52) |
| Switch off (drain) | fixed capacity, pool full and idle; switch the agent off | within a tick (30 s) idle runners are unregistered then their VMs destroyed; a busy one finishes its job first; nothing is replaced until it's switched on | F16, R6-R7 |
| Disconnect | stop the agent for under 30 s, then over 30 s | under: its runners survive; over: they're failed over and cleaned up | S9, F11-F12 |
| Revoke | `revoke_agent` (section 4) | the stream closes now; a restart of the agent is refused | A13, A15 |
| Back to Rust | section 5 | the agent connects to the Rust coordinator again as before | — |

Note any difference from the Rust coordinator's behaviour as a parity issue with the agent
type, the check, and the Phoenix log lines around it.
