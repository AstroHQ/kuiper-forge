# Provisioning Flow

Sequence diagrams showing how runners are created in each provisioning mode.

## Fixed Capacity Mode

Maintains a constant pool of runners. The coordinator reconciles every 30 seconds.

```mermaid
sequenceDiagram
    participant Coord as Coordinator
    participant GH as GitHub API
    participant Agent as Agent
    participant VM as Ephemeral VM

    rect rgb(232, 245, 233)
        note over Coord: Reconciliation Loop (every 30s)
        Coord->>Coord: Check pool: pending < target?
        Coord->>Coord: Find agent with matching labels
        Coord->>GH: Get registration token
        GH-->>Coord: Token
        Coord->>Agent: CreateRunner command
        Agent->>VM: Clone base image & start
        Agent-->>Coord: Ack (accepted)
        VM->>GH: Register as runner
    end

    rect rgb(255, 249, 196)
        note over GH: Job Execution
        GH->>VM: Assign workflow job
        VM->>VM: Execute job
        VM-->>GH: Job complete
    end

    rect rgb(227, 242, 253)
        note over VM: Cleanup
        VM->>VM: Self-destruct
        Agent-->>Coord: RunnerEvent (completed)
        Coord->>GH: Remove runner
        note over Coord: Pool now below target<br/>Next reconcile creates replacement
    end
```

## Webhook Mode (Dynamic)

Runners created on-demand when GitHub sends `workflow_job.queued` events.

```mermaid
sequenceDiagram
    participant GH as GitHub
    participant Coord as Coordinator
    participant Agent as Agent
    participant VM as Ephemeral VM

    rect rgb(243, 229, 245)
        note over GH: Job Queued
        GH->>Coord: POST /webhook<br/>workflow_job.queued
        Coord->>Coord: Validate signature (HMAC-SHA256)
        Coord->>Coord: Match labels to LabelMapping
        alt No matching mapping
            Coord-->>GH: 200 OK (ignored)
        else Labels match
            Coord->>Coord: Find agent with matching labels
            Coord->>GH: Get registration token
            GH-->>Coord: Token
            Coord->>Agent: CreateRunner command
            Agent->>VM: Clone base image & start
            Agent-->>Coord: Ack (accepted)
            VM->>GH: Register as runner
            Coord-->>GH: 200 OK
        end
    end

    rect rgb(255, 249, 196)
        note over GH: Job Execution
        GH->>VM: Assign workflow job
        VM->>VM: Execute job
        VM-->>GH: Job complete
    end

    rect rgb(227, 242, 253)
        note over VM: Cleanup
        VM->>VM: Self-destruct
        Agent-->>Coord: RunnerEvent (completed)
        Coord->>GH: Remove runner
        note over Coord: No replacement needed<br/>Next webhook triggers next runner
    end
```

## Polling Mode (Dynamic)

Runners created on-demand for queued jobs found by scanning the GitHub API every
`interval_secs` (default 120). Same pending queue and runner flow as webhook mode, but
nothing has to reach the coordinator from outside.

```mermaid
sequenceDiagram
    participant Coord as Coordinator
    participant GH as GitHub API
    participant Agent as Agent
    participant VM as Ephemeral VM

    rect rgb(243, 229, 245)
        note over Coord: Poll (every interval_secs)
        Coord->>GH: List queued jobs (recently pushed repos)
        GH-->>Coord: Queued jobs + labels
        Coord->>Coord: Filter required_labels, match LabelMapping
        Coord->>Coord: Skip jobs already pending or with a runner
        Coord->>Coord: Add new jobs to pending queue
        Coord->>GH: Check status of pending jobs the scan no longer listed
        note over Coord: Drop the ones no longer queued
    end

    rect rgb(232, 245, 233)
        note over Coord: Pending queue (same as webhook mode)
        Coord->>Coord: Find agent with matching labels
        Coord->>GH: Generate JIT config
        GH-->>Coord: JIT config
        Coord->>Agent: CreateRunner command
        Agent->>VM: Clone base image & start
        VM->>GH: Run job
    end
```

Each scan lists workflow runs per recently pushed repo, so short intervals across many
repos can exhaust the GitHub App's API rate limit; the coordinator warns below 60s.

## Comparison

| Aspect | Fixed Capacity | Webhook | Polling |
|--------|---------------|---------|---------|
| **Trigger** | 30s reconciliation loop | GitHub webhook event | API scan every `interval_secs` |
| **Pre-provisioning** | Yes - maintains pool | No - just-in-time | No - just-in-time |
| **Job latency** | Low (runners ready) | Higher (VM boot time) | Higher (scan interval + VM boot) |
| **Resource usage** | Constant | On-demand | On-demand |
| **Inbound access** | Not needed | Webhook endpoint | Not needed |
| **Configuration** | `[[runners]]` with `count` | `[[label_mappings]]` | `[[label_mappings]]` |
