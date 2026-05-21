# Kuiper Forge

Ephemeral GitHub Actions self-hosted runners on VMs.

Kuiper automatically provisions and destroys VMs for each CI job, giving you clean, isolated build environments without persistent runner infrastructure.

## Architecture

```
┌─────────────────┐         ┌──────────────────┐
│  GitHub Actions │ ──────► │  kuiper-forge    │  (coordinator)
└─────────────────┘         └────────┬─────────┘
                                     │ gRPC + mTLS
                    ┌────────────────┼────────────────┐
                    ▼                ▼                ▼
            ┌──────────────┐  ┌──────────────┐  ┌──────────────┐
            │ tart-agent   │  │ proxmox-agent│  │ proxmox-agent│
            │ (macOS host) │  │ (Linux host) │  │ (Windows)    │
            └──────────────┘  └──────────────┘  └──────────────┘
```

- **kuiper-forge** - Central coordinator. Manages GitHub App auth, issues runner registration tokens, dispatches jobs to agents.
- **kuiper-tart-agent** - Runs on macOS hosts. Creates VMs using [Tart](https://github.com/cirruslabs/tart).
- **kuiper-proxmox-agent** - Runs on Proxmox hosts. Creates VMs via Proxmox API (Linux/Windows).

## Quick Start

### 1. Set up the coordinator

```bash
# Generate TLS certificates
kuiper-forge ca init

# Create config file (see examples/coordinator-config.toml)
# Configure your GitHub App credentials and runner pools

# Run the coordinator
kuiper-forge serve --config config.toml
```

### 2. Register an agent

```bash
# Generate a registration bundle from the coordinator (includes server trust)
kuiper-forge token create --url https://coordinator:9443

# On a macOS host with Tart installed:
kuiper-tart-agent register kfr1_BUNDLE_TOKEN
kuiper-tart-agent \
  --labels macos,arm64 \
  --base-image ghcr.io/cirruslabs/macos-sequoia-base:latest

# On a Proxmox host:
kuiper-proxmox-agent register kfr1_BUNDLE_TOKEN
kuiper-proxmox-agent --config agent-config.toml
```

#### Running as a system daemon (Linux)

By default the proxmox agent uses per-user directories (`~/.config`, `~/.local/share`).
Pass `--system` (or set `KUIPER_PROXMOX_AGENT_SYSTEM=1`) to use FHS daemon paths instead:

| Path | default | `--system` |
| --- | --- | --- |
| config | `./`, `~/.config/...`, `/etc/...` (searched) | `/etc/kuiper-proxmox-agent/config.toml` |
| logs | `~/.local/share/kuiper-proxmox-agent/logs` | `/var/log/kuiper-proxmox-agent` |
| data / certs | `~/.local/share/kuiper-proxmox-agent` | `/var/lib/kuiper-proxmox-agent` |

`--config` and `--log-dir` (env: `KUIPER_PROXMOX_AGENT_LOG_DIR`) override individual
paths regardless of `--system`. Register and run in system mode with:

```bash
sudo kuiper-proxmox-agent --system register kfr1_BUNDLE_TOKEN   # writes /etc + /var/lib
sudo kuiper-proxmox-agent --system                              # logs to /var/log

# Generate a systemd unit (uses this binary's own path) and install it. The unit
# is printed to stdout; status notes go to stderr, so redirect it directly:
kuiper-proxmox-agent --system generate-service \
  > /etc/systemd/system/kuiper-proxmox-agent.service
systemctl daemon-reload && systemctl enable --now kuiper-proxmox-agent
```

#### Linux packages (deb/rpm/...)

The proxmox agent can be packaged with [nfpm](https://nfpm.goreleaser.com). The
package installs the binary to `/usr/bin`, a `--system` systemd unit to
`/usr/lib/systemd/system`, and (via maintainer scripts) creates a dedicated
`kuiper` user plus `/etc`, `/var/lib`, and `/var/log` directories. Config is
defined in `packaging/proxmox-agent/`.

```bash
# Build a package locally for testing (mirrors build-proxmox-agent-musl.sh).
# Requires Docker for the cross build; nfpm runs natively (brew install nfpm).
mise run package:proxmox-agent             # x86_64, deb + rpm -> target/packages/
mise run package:proxmox-agent arm64       # arm64
PACKAGERS="deb rpm apk" mise run package:proxmox-agent

# Equivalent to calling the script directly:
scripts/package-proxmox-agent.sh arm64
```

Then on the target host:

```bash
sudo apt install ./kuiper-proxmox-agent_*_amd64.deb        # or: dnf install ...rpm
sudo -u kuiper kuiper-proxmox-agent --system register kfr1_BUNDLE_TOKEN
sudo systemctl enable --now kuiper-proxmox-agent
```

### 3. Use in workflows

```yaml
jobs:
  build:
    runs-on: [self-hosted, macOS, arm64]
    steps:
      - uses: actions/checkout@v4
      - run: swift build
```

## Configuration

The coordinator supports both TOML config files and environment variables:

```bash
# Environment variables use KUIPER_ prefix with __ for nesting
export KUIPER_GITHUB__APP_ID=123456
export KUIPER_GRPC__LISTEN_ADDR=0.0.0.0:9443
```

See `examples/` for sample configuration files.

## Provisioning Modes

- **Fixed Capacity** (default) - Maintains a constant pool of ready runners
- **Webhook** - Creates runners on-demand via GitHub webhook events

## Requirements

- Rust 1.88+
- For macOS agents: [Tart](https://github.com/cirruslabs/tart)
- For Proxmox agents: Proxmox VE 7+ with API token

## Building

```bash
cargo build --release
```

## Similar projects

If you're looking for just macOS & simpler, [Tartelet](https://github.com/shapehq/tartelet) is great.

## License

MIT
