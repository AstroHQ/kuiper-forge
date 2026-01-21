# Kuiper CI Runner Architecture

Simple overview of the system components and their relationships.

```mermaid
flowchart TB
    subgraph GitHub
        API[GitHub API]
        Actions[GitHub Actions]
        Webhooks[Webhooks]
    end

    subgraph Coordinator[kuiper-forge]
        Server[gRPC Server :9443]
        Fleet[Fleet Manager]
        Auth[Auth Manager]
    end

    subgraph macOS[macOS Hosts]
        Tart1[tart-agent]
        Tart2[tart-agent]
        macVM1[macOS VMs]
        macVM2[macOS VMs]
    end

    subgraph Proxmox[Proxmox Hosts]
        Prox1[proxmox-agent]
        LinuxVM[Linux VMs]
        WinVM[Windows VMs]
    end

    %% Connections
    API <--> Fleet
    Webhooks -.-> Server

    Tart1 --> Server
    Tart2 --> Server
    Prox1 --> Server

    Tart1 --> macVM1
    Tart2 --> macVM2
    Prox1 --> LinuxVM
    Prox1 --> WinVM

    macVM1 --> Actions
    macVM2 --> Actions
    LinuxVM --> Actions
    WinVM --> Actions
```

## Component Overview

| Component | Description |
|-----------|-------------|
| **kuiper-forge** | Central coordinator - manages fleet, issues mTLS certs, communicates with GitHub API |
| **kuiper-tart-agent** | macOS agent using [Tart](https://tart.run) for VM management (max 2 VMs per host) |
| **kuiper-proxmox-agent** | Linux/Windows agent using Proxmox VE API for VM management |
| **Fleet Manager** | Maintains runner pools (fixed capacity) or handles webhooks (dynamic) |
| **Auth Manager** | Certificate authority for mTLS, token-to-certificate exchange |

## Connection Model

All agents initiate **outbound** connections to the coordinator:

```
Agent ──────► Coordinator
       gRPC + mTLS
       (bidirectional streaming)
```

This allows agents behind NAT/firewalls without port forwarding.
