# Kuiper

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
# Generate a registration token from the coordinator
kuiper-forge token generate

# On a macOS host with Tart installed:
kuiper-tart-agent \
  --coordinator-url https://coordinator:9443 \
  --token REG_TOKEN \
  --labels macos,arm64 \
  --base-image ghcr.io/cirruslabs/macos-sequoia-base:latest

# On a Proxmox host:
kuiper-proxmox-agent --config agent-config.toml --token REG_TOKEN
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

- Rust 1.75+
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
