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

## Comparison

| Aspect | Fixed Capacity | Webhook |
|--------|---------------|---------|
| **Trigger** | 30s reconciliation loop | GitHub webhook event |
| **Pre-provisioning** | Yes - maintains pool | No - just-in-time |
| **Job latency** | Low (runners ready) | Higher (VM boot time) |
| **Resource usage** | Constant | On-demand |
| **Configuration** | `[[runners]]` with `count` | `[[label_mappings]]` |
