# Ephemeral Windows CI Runner Architecture Plan for Proxmox

## High-Level Architecture

```
GitHub Webhook (workflow_job)
        ↓
Your Rust Orchestrator
        ↓
Proxmox API: Clone Template (linked clone, ~5-15 seconds)
        ↓
VM Boots (~30-45 seconds)
        ↓
Orchestrator connects via SSH
        ↓
Runs runner configuration script on VM
        ↓
Runner registers as ephemeral & executes job
        ↓
Job completes → Runner auto-unregisters
        ↓
Orchestrator destroys VM via Proxmox API
```

## Key Design Decisions

### 1. Clone Strategy: Linked Clones on LVM-Thin

**Why**: Speed is critical for ephemeral runners
- **Linked clones**: Fast to create (typically 10-30 seconds depending on storage)
- **LVM-Thin storage**: Supports linked clones via thin provisioning and snapshots
- **Trade-off**: VMs depend on template, but acceptable since template is stable and ephemeral

**What Storage Do You Have?**
- **LVM-Thin**: Best option for linked clones if available
- **LVM**: Only supports full clones (slower, but still workable - 1-3 minutes)
- **Directory storage**: Only supports full clones of raw disks or qcow2 (slower)

**Recommendation for Your Setup**:
If you don't have LVM-Thin configured, you have two options:

**Option A: Convert existing storage to LVM-Thin** (recommended if possible)
- Best performance for linked clones
- Fast clone creation
- Efficient space usage
- Requires rebuilding storage pool

**Option B: Use full clones with existing storage** (simpler, works today)
- No storage changes needed
- Slower clone time (1-3 minutes instead of 10-30 seconds)
- Still acceptable for CI use - total time to runner ready ~2-4 minutes
- More storage space used per VM

**For this guide, we'll assume you're using LVM-Thin or will set it up.**

**Proxmox Configuration with LVM-Thin**:
```
Storage: LVM-Thin pool (e.g., local-lvm or custom LVM-Thin)
Clone mode: Linked (full=0 in API)
Result: Each runner VM only stores diffs from template
```

**If using regular LVM or Directory storage**:
```
Storage: Your existing storage
Clone mode: Full (full=1 in API)
Result: Each runner VM is independent copy (~1-3 min clone time)
```

### 2. Hostname Strategy: No Reboot Required

**Problem**: Windows traditionally requires reboot after hostname change

**Solutions** (pick one):

**Option A: Accept Random Hostname (Simplest)**
- Let Windows keep its random hostname from template
- GitHub runner identifies by unique runner name, not hostname
- **Pros**: No setup needed, fastest
- **Cons**: Less clear in logs/monitoring

**Option B: Post-Boot Hostname Change (Recommended)**
- Use registry-based hostname change after VM boots
- Updates registry keys without reboot
- Some services won't see new name until reboot, but CI runner doesn't care
- **Pros**: Better identification in Proxmox console
- **Cons**: Adds 5-10 seconds to setup

**Option C: Cloud-Init/Cloudbase-Init (Most Complex)**
- Use Cloudbase-Init to set hostname during first boot
- Requires additional template setup
- **Pros**: Most "proper" approach
- **Cons**: Overkill for ephemeral VMs, slower boot

**Recommendation**: Start with Option A, move to Option B only if needed for debugging

### 3. Runner Configuration Delivery: SSH + Remote Script Execution

**Why SSH**: 
- Already available on Windows 11 (OpenSSH Server)
- Reliable, well-understood protocol
- Easy to script from Rust orchestrator

**Setup Flow**:
```
1. Template has OpenSSH Server installed & enabled
2. Template has authorized_keys configured for orchestrator
3. Orchestrator clones VM and waits for boot
4. Orchestrator gets VM IP from QEMU guest agent
5. Orchestrator SSH's in and runs configuration script
6. Script configures + starts ephemeral GitHub runner
```

### 4. GitHub Runner Pre-staging in Template

**Critical**: Download runner binaries into template
- **Location**: `C:\actions-runner\`
- **Why**: Saves 30-60 seconds per VM (no download time)
- **Trade-off**: Need to rebuild template when runner version updates

**Template Structure**:
```
C:\actions-runner\
  ├── actions-runner-win.exe
  ├── config.cmd
  ├── run.cmd
  └── ... (all runner files pre-extracted)
```

## Template Preparation Checklist

Your Packer build should produce a template with these characteristics:

### Core OS Configuration

- [ ] **Windows 11 Pro/Enterprise** (for volume licensing & sysprep)
- [ ] **Fully updated** (Windows Updates applied during template build)
- [ ] **Generalized** (sysprep'd or equivalent, unique SID on each clone)
- [ ] **VirtIO drivers installed** (disk, network, balloon)
- [ ] **QEMU Guest Agent installed & enabled** (critical for IP detection)

### Performance Optimizations

- [ ] **Unnecessary services disabled** (Windows Search, Print Spooler, etc.)
- [ ] **Windows bloatware removed** (Xbox, Weather, etc.)
- [ ] **Pagefile configured** (or disabled if you have sufficient RAM)
- [ ] **Visual effects minimized** (reduce CPU/GPU overhead)
- [ ] **Windows Defender real-time scanning** (consider disabling for CI)
- [ ] **Fast Startup disabled** (can cause issues with VMs)

### CI-Specific Setup

- [ ] **Development tools pre-installed** (Git, build tools, SDKs, etc.)
- [ ] **GitHub Actions runner downloaded** (NOT configured, just binaries)
- [ ] **Runner location**: `C:\actions-runner\` with binaries extracted
- [ ] **Environment variables** set for common tools

### SSH Access Configuration

- [ ] **OpenSSH Server installed** (`Add-WindowsCapability`)
- [ ] **SSH service set to auto-start**
- [ ] **PowerShell set as default SSH shell** (or cmd, depending on preference)
- [ ] **Firewall rule for port 22** enabled
- [ ] **authorized_keys file created**: `C:\ProgramData\ssh\administrators_authorized_keys`
- [ ] **Orchestrator public key added** to authorized_keys
- [ ] **Proper permissions set** on authorized_keys (SYSTEM and Admins only)
- [ ] **Password authentication disabled** (key-only for security)

### Runtime Configuration Script

Template should include a script that orchestrator will invoke:

**Location**: `C:\scripts\configure-runner.ps1`

**Script responsibilities**:
1. Accept parameters: `$RunnerToken`, `$RunnerName`, `$RepoUrl`, `$Labels`
2. Change hostname (optional, see strategy above)
3. Configure GitHub runner as ephemeral
4. Start runner in foreground
5. Exit when runner completes (ephemeral runners auto-unregister)

**Critical runner config flags**:
- `--ephemeral`: Runner auto-unregisters after one job
- `--unattended`: No interactive prompts
- `--replace`: Replace any existing runner with same name

## Proxmox VM Configuration

### Template VM Settings (Created by Packer)

```
CPU: host (not kvm64 - critical for performance)
Cores: 4-8 (depending on your CI needs)
Memory: 8-16GB
Balloon: 0 (disabled - causes issues on Windows)
Machine Type: q35
BIOS: OVMF (UEFI)
OS Type: win11
SCSI Controller: VirtIO SCSI Single
Network: VirtIO
Agent: Enabled (QEMU Guest Agent)

Disks:
- SCSI0: 100GB, cache=writeback, iothread=on, discard=on, ssd=on
- EFI Disk: 1MB
- TPM State: 4MB (required for Windows 11)
```

### Clone Configuration (Done by Orchestrator)

**Via Proxmox API**:
```
POST /api2/json/nodes/{node}/qemu/{template_id}/clone

Parameters:
- newid: (auto-assign next available)
- name: runner-{job_id} or runner-{timestamp}
- full: 0 (linked clone - LVM-Thin/ZFS) OR 1 (full clone - LVM/Directory)
- target: {node}
- storage: {your-storage-pool-name}
```

**Result with LVM-Thin**: Clone created in 10-30 seconds, ready to boot
**Result with full clones**: Clone created in 1-3 minutes, ready to boot

## Orchestrator Responsibilities

Your Rust orchestrator needs to handle these stages:

### 1. Webhook Reception
- Listen for GitHub `workflow_job` webhook events
- Validate webhook signature
- Filter for `action: queued` with matching labels
- Extract job ID and metadata

### 2. VM Creation
- Call Proxmox API to get next available VM ID
- Clone template using Proxmox API (linked clone)
- Start the VM
- Wait for boot (polling or timeout-based)

### 3. IP Detection
- Poll QEMU guest agent via Proxmox API
- Endpoint: `/api2/json/nodes/{node}/qemu/{vmid}/agent/network-get-interfaces`
- Extract IPv4 address (ignore loopback and link-local)
- Retry with exponential backoff (agent takes time to start)

### 4. Runner Configuration
- Generate GitHub runner registration token via GitHub API
- SSH to VM using pre-configured key
- Execute configuration script with parameters
- Script configures runner and starts it

### 5. Job Execution
- Runner picks up job from GitHub queue
- Orchestrator can optionally monitor job status via GitHub API
- No intervention needed - runner executes job autonomously

### 6. Cleanup
- **Option A (Webhook-based)**: Wait for `workflow_job` webhook with `action: completed`
- **Option B (Polling)**: Poll GitHub API for job completion
- **Option C (Timeout)**: Cleanup after max job time
- Stop VM via Proxmox API
- Delete VM via Proxmox API (linked clone deletion is fast)

### 7. Error Handling
- VM creation fails → cleanup partial resources
- Boot timeout → force delete VM
- SSH connection fails → retry, then cleanup
- Runner registration fails → cleanup VM

## Timing Expectations

**From webhook to runner ready**:

**With LVM-Thin (Linked Clones)**:
- Linked clone creation: 10-30 seconds
- VM boot: 30-45 seconds
- QEMU agent ready: 5-10 seconds
- SSH connection + config: 10-20 seconds
- Runner registration: 5-10 seconds
- **Total**: ~70-115 seconds from webhook to job start

**With Full Clones (LVM/Directory storage)**:
- Full clone creation: 1-3 minutes
- VM boot: 30-45 seconds
- QEMU agent ready: 5-10 seconds
- SSH connection + config: 10-20 seconds
- Runner registration: 5-10 seconds
- **Total**: ~2-4 minutes from webhook to job start

**Optimization opportunities**:
- Use SSD/NVMe storage for template (critical for full clones)
- Tune VM CPU/memory for faster boot
- Pre-warm template by booting it occasionally
- Consider keeping 1-2 "warm" VMs pre-configured (advanced)

## Storage Considerations

### Template Storage Requirements
- Template disk: ~40-60GB (depending on installed tools)
- **With LVM-Thin**: Each linked clone: ~5-10GB (only stores diffs)
- **Without LVM-Thin (full clones)**: Each clone: Full template size (40-60GB)

### Example Capacity Planning

**With LVM-Thin (Linked Clones)**:
```
Concurrent jobs: 10
Template size: 50GB
Clone overhead: 10GB each (diffs only)

Total storage needed: 50 + (10 * 10) = 150GB
```

**Without LVM-Thin (Full Clones)**:
```
Concurrent jobs: 10
Template size: 50GB
Clone size: 50GB each (full copy)

Total storage needed: 50 + (10 * 50) = 550GB
```

**Setting up LVM-Thin** (if you want to convert):
```bash
# Example: Convert a volume group to support thin provisioning
# WARNING: This destroys existing data, backup first!

# Create thin pool on existing volume group
lvcreate -L 500G -T pve/data

# Add to Proxmox storage
pvesm add lvmthin local-lvm --vgname pve --thinpool data
```

## Security Considerations

### Template Security
- **No secrets in template** (everything passed at runtime)
- **SSH key-only authentication** (no passwords)
- **Minimal services running** (reduce attack surface)
- **Firewall configured** (only necessary ports)

### Runtime Security
- **Ephemeral runners** (clean state for each job)
- **No persistent storage** (everything destroyed after job)
- **Registration tokens** are short-lived (~1 hour)
- **Network isolation** (consider VLAN for runner VMs)

### Orchestrator Security
- **GitHub webhook signature validation**
- **Proxmox API token** (not root password)
- **SSH private key** secured on orchestrator host
- **Rate limiting** (prevent resource exhaustion)

## Failure Scenarios & Recovery

### VM Creation Failure
- **Symptom**: Proxmox API returns error
- **Recovery**: Log error, report to monitoring, skip job
- **Prevention**: Monitor Proxmox storage capacity

### Boot Timeout
- **Symptom**: VM doesn't become reachable in expected time
- **Recovery**: Force stop & delete VM, report failure
- **Prevention**: Ensure template is properly sysprepped

### SSH Connection Failure
- **Symptom**: Cannot connect to VM after boot
- **Recovery**: Retry with backoff, then cleanup VM
- **Prevention**: Validate SSH works in template before converting

### Runner Registration Failure
- **Symptom**: Runner config fails with token error
- **Recovery**: Check if token expired, regenerate, retry
- **Prevention**: Request token just before use

### Zombie VMs
- **Symptom**: VM orphaned, orchestrator crashed
- **Recovery**: Periodic cleanup job to find old runner VMs
- **Prevention**: Tag VMs with creation timestamp, cleanup after max age

## Monitoring & Observability

### Key Metrics to Track
- VM creation time (detect storage performance degradation)
- Boot time (detect template issues)
- Time to runner ready (overall health)
- Job completion rate (detect failures)
- Active runner count (capacity planning)
- Storage usage (prevent exhaustion)

### Logging Requirements
- All Proxmox API calls (for debugging)
- VM lifecycle events (creation, start, stop, delete)
- SSH connection attempts (detect connectivity issues)
- Runner registration (detect GitHub API issues)
- Job assignments (correlation with GitHub)

## Next Steps for Your Rust Implementation

1. **Phase 1**: Get template right
   - Build with Packer
   - Manually test SSH access
   - Manually test runner configuration
   - Convert to template, test clone speed

2. **Phase 2**: Proxmox API integration
   - Connect to Proxmox API from Rust
   - Test clone, start, get IP, delete operations
   - Validate linked clone performance

3. **Phase 3**: SSH automation
   - SSH from Rust to VM
   - Execute runner configuration script
   - Handle errors gracefully

4. **Phase 4**: GitHub integration
   - Implement webhook receiver
   - Generate registration tokens
   - Handle workflow_job events

5. **Phase 5**: Full orchestration
   - Connect all pieces
   - Add monitoring/logging
   - Implement cleanup logic
   - Handle edge cases

Would you like me to dive deeper into any specific area of this plan?
