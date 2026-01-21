# Component Diagram

Detailed view of kuiper-forge internals and agent communication.

```mermaid
flowchart TB
    subgraph GitHub[GitHub]
        GHA[GitHub Actions<br/>Workflow Jobs]
        GHAPI[GitHub API]
        GHWebhook[Webhooks<br/>workflow_job events]
    end

    subgraph Coordinator[kuiper-forge Coordinator]
        subgraph Server[gRPC + HTTP Server :9443]
            RegSvc[Registration Service<br/>no mTLS required]
            AgentSvc[Agent Service<br/>mTLS required]
            WebhookEP[Webhook Endpoint<br/>/webhook]
        end

        FleetMgr[Fleet Manager]
        AuthMgr[Auth Manager<br/>CA / Tokens / Certs]
        AgentReg[Agent Registry]
        RunnerState[Runner State Store<br/>JSON persistence]
        GHClient[GitHub Client]

        FleetMgr --> AgentReg
        FleetMgr --> RunnerState
        FleetMgr --> GHClient
        AgentSvc --> AgentReg
        AgentSvc --> FleetMgr
        RegSvc --> AuthMgr
    end

    subgraph Agents[Agent Fleet]
        subgraph TartAgent[kuiper-tart-agent<br/>macOS Host]
            TartMgr[Tart VM Manager]
            TartVM1[macOS VM]
            TartVM2[macOS VM]
        end

        subgraph ProxmoxAgent[kuiper-proxmox-agent<br/>Proxmox Host]
            ProxMgr[Proxmox API Client]
            LinuxVM[Linux VM]
            WinVM[Windows VM]
        end
    end

    %% GitHub connections
    GHA -->|picks up jobs| TartVM1
    GHA -->|picks up jobs| LinuxVM
    GHA -->|picks up jobs| WinVM
    GHAPI <-->|runner tokens<br/>runner cleanup| GHClient
    GHWebhook -.->|workflow_job.queued<br/>webhook mode| WebhookEP

    %% Agent registration flow
    TartAgent -->|1. Register token| RegSvc
    RegSvc -->|2. Issue cert| TartAgent
    TartAgent <-->|3. Bidirectional stream<br/>mTLS| AgentSvc

    ProxmoxAgent -->|1. Register token| RegSvc
    RegSvc -->|2. Issue cert| ProxmoxAgent
    ProxmoxAgent <-->|3. Bidirectional stream<br/>mTLS| AgentSvc

    %% Fleet manager commands
    FleetMgr -->|CreateRunner cmd| AgentSvc
    AgentSvc -->|Runner events| FleetMgr

    %% Styling
    style GitHub fill:#f5f5f5,stroke:#333
    style Coordinator fill:#e3f2fd,stroke:#1565c0
    style Agents fill:#e8f5e9,stroke:#2e7d32
    style Server fill:#bbdefb,stroke:#1565c0
    style TartAgent fill:#c8e6c9,stroke:#2e7d32
    style ProxmoxAgent fill:#c8e6c9,stroke:#2e7d32

    style GHA fill:#fff,stroke:#333
    style GHAPI fill:#fff,stroke:#333
    style GHWebhook fill:#fff,stroke:#333

    style TartVM1 fill:#fff9c4,stroke:#f9a825
    style TartVM2 fill:#fff9c4,stroke:#f9a825
    style LinuxVM fill:#fff9c4,stroke:#f9a825
    style WinVM fill:#fff9c4,stroke:#f9a825
```

## Data Flow

1. **Agent Registration**: Agent exchanges one-time token for mTLS certificate
2. **Agent Stream**: Bidirectional gRPC stream for commands and events
3. **Runner Creation**: Fleet manager sends `CreateRunner` command to agent
4. **Job Execution**: Ephemeral VM registers with GitHub, runs job, self-destructs
5. **Cleanup**: Agent reports completion, coordinator removes runner from GitHub
