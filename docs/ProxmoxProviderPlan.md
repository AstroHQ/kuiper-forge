# Proxmox Agent Plan

The `proxmox-agent` is a daemon that runs with access to the Proxmox VE API and connects outbound to the coordinator. It manages Windows/Linux VM lifecycle for ephemeral GitHub Actions runners.

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                     Coordinator (Cloud/Central)                  │
│                                                                  │
│  ┌──────────────┐    ┌──────────────┐    ┌──────────────┐       │
│  │ Agent Server │◄───│ Agent        │    │ GitHub App   │       │
│  │ (gRPC+mTLS)  │    │ Registry     │    │ Client       │       │
│  └──────────────┘    └──────────────┘    └──────────────┘       │
│         ▲                                                        │
└─────────┼────────────────────────────────────────────────────────┘
          │ Outbound Connection
          │ (gRPC + mTLS)
          │
┌─────────┴────────────────────────────────────────────────────────┐
│                     Proxmox Host (On-Premise/NAT)                │
│                                                                  │
│  ┌──────────────────────────────────────────────────────────────┐│
│  │                      proxmox-agent                            ││
│  │  ┌────────────────┐  ┌────────────────┐  ┌────────────────┐  ││
│  │  │ Coordinator    │  │ VM Manager     │  │ Proxmox API    │  ││
│  │  │ Connector      │  │                │  │ Client         │  ││
│  │  └────────────────┘  └────────────────┘  └────────────────┘  ││
│  └──────────────────────────────────────────────────────────────┘│
│                                │                                  │
│                                ▼                                  │
│  ┌──────────────────────────────────────────────────────────────┐│
│  │                    Proxmox VE REST API                        ││
│  │                    (https://proxmox:8006)                     ││
│  └──────────────────────────────────────────────────────────────┘│
│                                │                                  │
│                                ▼                                  │
│  ┌──────────────────────────────────────────────────────────────┐│
│  │                    Ephemeral VMs                              ││
│  │  ┌──────────┐  ┌──────────┐  ┌──────────┐                    ││
│  │  │ Windows  │  │ Windows  │  │ Linux    │  ...               ││
│  │  │ Runner   │  │ Runner   │  │ Runner   │                    ││
│  │  └──────────┘  └──────────┘  └──────────┘                    ││
│  └──────────────────────────────────────────────────────────────┘│
└──────────────────────────────────────────────────────────────────┘
```

## Crate Structure

```
proxmox-agent/
├── Cargo.toml
└── src/
    ├── main.rs          # Agent daemon entry point
    ├── config.rs        # Agent configuration
    ├── coordinator.rs   # Outbound connection to coordinator
    ├── vm_manager.rs    # VM lifecycle management
    ├── proxmox/
    │   ├── mod.rs
    │   ├── client.rs    # Proxmox REST API client
    │   ├── clone.rs     # VM cloning operations
    │   └── guest.rs     # QEMU guest agent interactions
    └── error.rs         # Error types
```

## Agent Configuration

```toml
# /etc/proxmox-agent/config.toml (Linux host running agent)

[coordinator]
# gRPC endpoint (note: https, not wss)
url = "https://coordinator.example.com:9443"
hostname = "coordinator.example.com"  # For TLS verification

# For FIRST RUN only: registration token from coordinator
# Generate with: coordinator token create --labels "windows,x64,proxmox"
# After successful registration, this can be removed (certificate is stored)
registration_token = "reg_a1b2c3d4e5f6..."

[tls]
# CA certificate from coordinator (required - get with: coordinator ca export)
ca_cert = "/etc/proxmox-agent/certs/ca.crt"

# Client certificate directory (auto-populated after registration)
# Contains: client.crt, client.key, metadata.json
certs_dir = "/etc/proxmox-agent/certs"

[agent]
# Labels this agent advertises to the coordinator
# These are matched against runner configurations
labels = ["self-hosted", "windows", "x64", "proxmox"]

[proxmox]
api_url = "https://proxmox.local:8006"
node = "pve"
token_id = "ci-runner@pve!runner"
token_secret = "xxxxxxxx-xxxx-xxxx-xxxx-xxxxxxxxxxxx"
accept_invalid_certs = true  # For self-signed Proxmox certs

[vm]
template_vmid = 9000
storage = "local-lvm"
linked_clone = true
concurrent_vms = 4

[ssh]
private_key_path = "/etc/proxmox-agent/id_ed25519"
username = "ci"
```

### First-Time Setup

```bash
# 1. Create config directory
$ mkdir -p /etc/proxmox-agent/certs

# 2. Get CA certificate from coordinator
$ ssh coordinator.example.com "coordinator ca export" > /etc/proxmox-agent/certs/ca.crt

# 3. Generate a registration token on the coordinator
$ ssh coordinator.example.com "coordinator token create --labels 'windows,x64,proxmox' --expires 1h"
Registration token created:
  Token: reg_a1b2c3d4e5f6...
  Labels: windows, x64, proxmox
  Expires: 2024-01-15T15:30:00Z

# 4. Create config file with the token
$ vim /etc/proxmox-agent/config.toml
# Set registration_token and other settings

# 5. Start the agent
$ systemctl start proxmox-agent

# 6. Agent logs show successful registration
$ journalctl -u proxmox-agent -f
INFO Connecting to coordinator for registration...
INFO Successfully registered as agent_proxmox_pve_x7y8z9
INFO Client certificate saved to /etc/proxmox-agent/certs/
INFO Certificate expires: 2025-01-15T14:30:00Z
INFO Connected to coordinator via mTLS, ready for work

# 7. (Optional) Remove registration token from config
# The agent now uses the client certificate for authentication
```

### Certificate Storage

After successful registration, certificates are stored in the `certs_dir`:

```
/etc/proxmox-agent/certs/
├── ca.crt           # Coordinator CA (you provided this)
├── client.crt       # Agent's client certificate (from registration)
├── client.key       # Agent's private key (mode 0600)
└── metadata.json    # Registration metadata
```

```json
// metadata.json
{
  "agent_id": "agent_proxmox_pve_x7y8z9",
  "registered_at": "2024-01-15T14:30:00Z",
  "expires_at": "2025-01-15T14:30:00Z"
}
```

The agent will:
1. Check for valid client certificate on startup
2. Use mTLS for all gRPC communication
3. Fall back to registration token if certificate is missing/expired/revoked
4. Automatically clear certificates if revoked by coordinator

## Proxmox API Client

### Authentication

Proxmox supports API tokens for authentication:

```rust
pub struct ProxmoxClient {
    base_url: String,      // e.g., "https://proxmox.local:8006"
    node: String,          // e.g., "pve"
    token_id: String,      // e.g., "ci-runner@pve!runner"
    token_secret: String,
    http_client: reqwest::Client,
}

impl ProxmoxClient {
    pub fn new(config: ProxmoxConfig) -> Result<Self> {
        let http_client = reqwest::Client::builder()
            .danger_accept_invalid_certs(config.accept_invalid_certs)
            .build()?;
        // ...
    }
    
    fn auth_header(&self) -> String {
        format!("PVEAPIToken={}={}", self.token_id, self.token_secret)
    }
}
```

### API Endpoints Used

**Base URL**: `https://{host}:8006/api2/json`

| Operation | Endpoint | Method |
|-----------|----------|--------|
| Get next VM ID | `/cluster/nextid` | GET |
| Clone VM | `/nodes/{node}/qemu/{vmid}/clone` | POST |
| Start VM | `/nodes/{node}/qemu/{vmid}/status/start` | POST |
| Stop VM | `/nodes/{node}/qemu/{vmid}/status/stop` | POST |
| Delete VM | `/nodes/{node}/qemu/{vmid}` | DELETE |
| Get VM status | `/nodes/{node}/qemu/{vmid}/status/current` | GET |
| Get network interfaces | `/nodes/{node}/qemu/{vmid}/agent/network-get-interfaces` | GET |
| Wait for task | `/nodes/{node}/tasks/{upid}/status` | GET |

## Agent Implementation

### Coordinator Connection (gRPC + mTLS)

The agent uses gRPC with mTLS for secure communication:

```rust
use agent_lib::{AgentConnector, AgentCertStore};
use agent_proto::agent_service_client::AgentServiceClient;
use agent_proto::{AgentMessage, CoordinatorMessage, agent_message, coordinator_message};
use tokio::sync::mpsc;
use tonic::transport::Channel;

pub struct ProxmoxAgent {
    config: AgentConfig,
    cert_store: AgentCertStore,
    proxmox: ProxmoxClient,
    vm_manager: VmManager,
}

impl ProxmoxAgent {
    pub async fn run(&self) -> Result<()> {
        let mut connector = AgentConnector::new(
            self.config.clone(),
            self.cert_store.clone(),
        );

        loop {
            match connector.connect().await {
                Ok(client) => {
                    info!("Connected to coordinator via mTLS");
                    if let Err(e) = self.run_agent_stream(client).await {
                        error!("Agent stream error: {}", e);
                    }
                }
                Err(e) => {
                    error!("Connection failed: {}", e);
                }
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }

    async fn run_agent_stream(&self, mut client: AgentServiceClient<Channel>) -> Result<()> {
        let (tx, rx) = mpsc::channel(32);

        // Start bidirectional gRPC stream
        let response = client
            .agent_stream(tokio_stream::wrappers::ReceiverStream::new(rx))
            .await?;

        let mut inbound = response.into_inner();

        // Send initial status
        tx.send(AgentMessage {
            payload: Some(agent_message::Payload::Status(AgentStatus {
                active_vms: self.vm_manager.active_count() as u32,
                available_slots: self.vm_manager.available_slots() as u32,
                vms: vec![],
            })),
        }).await?;

        // Process messages from coordinator
        while let Some(msg) = inbound.message().await? {
            match msg.payload {
                Some(coordinator_message::Payload::CreateRunner(cmd)) => {
                    let result = self.handle_create_runner(&cmd, &tx).await;
                    tx.send(result.into()).await?;
                }
                Some(coordinator_message::Payload::DestroyRunner(cmd)) => {
                    let result = self.handle_destroy_runner(&cmd).await;
                    tx.send(result.into()).await?;
                }
                Some(coordinator_message::Payload::Ping(_)) => {
                    tx.send(AgentMessage {
                        payload: Some(agent_message::Payload::Pong(Pong {})),
                    }).await?;
                }
                _ => {}
            }
        }

        Ok(())
    }

    async fn handle_create_runner(
        &self,
        cmd: &CreateRunnerCommand,
        tx: &mpsc::Sender<AgentMessage>,
    ) -> CommandResult {
        let result = async {
            // 1. Clone and start VM
            let vm = self.vm_manager.create_vm(&cmd.vm_name).await?;

            // 2. Wait for VM to be ready (IP + SSH)
            let ip = self.vm_manager.wait_for_ready(&vm).await?;

            // 3. Configure and start GitHub runner
            self.vm_manager.configure_runner(
                &ip,
                &cmd.registration_token,
                &cmd.labels,
                &cmd.runner_scope_url,
            ).await?;

            // 4. Wait for runner to complete (--ephemeral exits when done)
            self.vm_manager.wait_for_runner_exit(&ip).await?;

            // 5. Cleanup VM
            self.vm_manager.destroy_vm(&vm).await?;

            Ok::<_, Error>((vm.id, ip))
        }.await;

        match result {
            Ok((vm_id, ip)) => CommandResult {
                command_id: cmd.command_id.clone(),
                success: true,
                error: String::new(),
                result: Some(command_result::Result::CreateRunner(CreateRunnerResult {
                    vm_id,
                    ip_address: ip,
                })),
            },
            Err(e) => CommandResult {
                command_id: cmd.command_id.clone(),
                success: false,
                error: e.to_string(),
                result: None,
            },
        }
    }
}
```

### VM Manager

The VM manager handles Proxmox-specific operations:

```rust
pub struct VmManager {
    client: ProxmoxClient,
    config: VmConfig,
    ssh_config: SshConfig,
}

impl VmManager {
    pub async fn create_vm(&self, name: &str) -> Result<VmInstance> {
        // 1. Get next available VM ID
        let vmid = self.client.get_next_vmid().await?;

        // 2. Clone template
        let task = self.client.clone_vm(
            self.config.template_vmid,
            vmid,
            name,
            self.config.linked_clone,
            &self.config.storage,
        ).await?;

        // 3. Wait for clone task to complete
        self.client.wait_for_task(&task).await?;

        // 4. Start the VM
        self.client.start_vm(vmid).await?;

        Ok(VmInstance {
            id: vmid.to_string(),
            name: name.to_string(),
            ip_address: None,
        })
    }

    pub async fn wait_for_ready(&self, vm: &VmInstance) -> Result<String> {
        let vmid: u32 = vm.id.parse()?;

        // Poll QEMU guest agent for IP address
        let ip = self.poll_for_ip(vmid).await?;

        // Wait for SSH to be available
        self.wait_for_ssh(&ip).await?;

        Ok(ip)
    }

    pub async fn configure_runner(
        &self,
        ip: &str,
        registration_token: &str,
        labels: &[String],
        runner_scope_url: &str,
    ) -> Result<()> {
        let session = self.connect_ssh(ip).await?;

        // Configure runner (Windows PowerShell example)
        let config_cmd = format!(
            r#"cd C:\actions-runner; .\config.cmd --url {} --token {} --labels {} --ephemeral --unattended"#,
            runner_scope_url,
            registration_token,
            labels.join(","),
        );
        session.execute(&config_cmd).await?;

        // Start runner
        session.execute(r#"cd C:\actions-runner; .\run.cmd"#).await?;

        Ok(())
    }

    pub async fn destroy_vm(&self, vm: &VmInstance) -> Result<()> {
        let vmid: u32 = vm.id.parse()?;

        // Stop VM (ignore errors - may already be stopped)
        let _ = self.client.stop_vm(vmid).await;

        // Wait a moment for clean shutdown
        tokio::time::sleep(Duration::from_secs(5)).await;

        // Delete VM
        self.client.delete_vm(vmid).await?;

        Ok(())
    }
}
```

## Clone Operations

### Linked vs Full Clones

| Clone Type | Storage Required | Clone Time | Space Usage |
|------------|-----------------|------------|-------------|
| **Linked** | LVM-Thin, ZFS | 10-30 sec | ~5-10GB (diffs only) |
| **Full** | Any | 1-3 min | Full template size |

```rust
impl ProxmoxClient {
    pub async fn clone_vm(
        &self,
        source_vmid: u32,
        target_vmid: u32,
        name: &str,
        linked: bool,
        storage: &str,
    ) -> Result<String> {
        let endpoint = format!(
            "/nodes/{}/qemu/{}/clone",
            self.node, source_vmid
        );
        
        let params = CloneParams {
            newid: target_vmid,
            name: name.to_string(),
            full: if linked { 0 } else { 1 },
            storage: storage.to_string(),
            target: self.node.clone(),
        };
        
        let response: TaskResponse = self.post(&endpoint, &params).await?;
        Ok(response.data)  // Returns UPID (task ID)
    }
    
    pub async fn wait_for_task(&self, upid: &str) -> Result<()> {
        loop {
            let status = self.get_task_status(upid).await?;
            
            match status.status.as_str() {
                "stopped" => {
                    if status.exitstatus == "OK" {
                        return Ok(());
                    } else {
                        return Err(Error::TaskFailed(status.exitstatus));
                    }
                }
                "running" => {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                _ => {
                    return Err(Error::UnknownTaskStatus(status.status));
                }
            }
        }
    }
}
```

## QEMU Guest Agent Integration

The guest agent is required for IP detection:

```rust
impl ProxmoxClient {
    pub async fn get_network_interfaces(&self, vmid: u32) -> Result<Vec<NetworkInterface>> {
        let endpoint = format!(
            "/nodes/{}/qemu/{}/agent/network-get-interfaces",
            self.node, vmid
        );
        
        let response: AgentResponse<NetworkInterfaces> = self.get(&endpoint).await?;
        Ok(response.data.result)
    }
}

impl ProxmoxProvider {
    async fn poll_for_ip(&self, vmid: u32) -> Result<String> {
        let timeout = Duration::from_secs(120);
        let start = Instant::now();
        
        while start.elapsed() < timeout {
            match self.client.get_network_interfaces(vmid).await {
                Ok(interfaces) => {
                    // Find first non-loopback IPv4 address
                    for iface in interfaces {
                        if iface.name == "lo" {
                            continue;
                        }
                        for addr in iface.ip_addresses {
                            if addr.ip_address_type == "ipv4" 
                                && !addr.ip_address.starts_with("169.254.") // Skip link-local
                            {
                                return Ok(addr.ip_address);
                            }
                        }
                    }
                }
                Err(e) => {
                    // Guest agent not ready yet
                    debug!("Guest agent not ready: {}", e);
                }
            }
            
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        
        Err(Error::Timeout("waiting for IP address"))
    }
}
```

## Windows Template Requirements

### Core OS Configuration

- [ ] **Windows 11 Pro/Enterprise** (volume licensing & sysprep)
- [ ] **Fully updated** (Windows Updates applied)
- [ ] **Generalized** (sysprep'd, unique SID on each clone)
- [ ] **VirtIO drivers installed** (disk, network, balloon)
- [ ] **QEMU Guest Agent installed & enabled**

### Performance Optimizations

- [ ] **Unnecessary services disabled** (Windows Search, Print Spooler)
- [ ] **Windows bloatware removed** (Xbox, Weather, etc.)
- [ ] **Pagefile configured** (or disabled with sufficient RAM)
- [ ] **Visual effects minimized**
- [ ] **Windows Defender real-time scanning** (consider disabling)
- [ ] **Fast Startup disabled**

### SSH Access Configuration

- [ ] **OpenSSH Server installed** (`Add-WindowsCapability`)
- [ ] **SSH service set to auto-start**
- [ ] **PowerShell as default SSH shell**
- [ ] **Firewall rule for port 22** enabled
- [ ] **authorized_keys file**: `C:\ProgramData\ssh\administrators_authorized_keys`
- [ ] **Orchestrator public key added**
- [ ] **Password authentication disabled**

### CI-Specific Setup

- [ ] **GitHub Actions runner downloaded** (NOT configured)
- [ ] **Runner location**: `C:\actions-runner\`
- [ ] **Development tools pre-installed** (Git, build tools, SDKs)
- [ ] **Environment variables configured**

### Template VM Settings

```
CPU: host (not kvm64 - critical for performance)
Cores: 4-8
Memory: 8-16GB
Balloon: 0 (disabled - causes issues on Windows)
Machine Type: q35
BIOS: OVMF (UEFI)
OS Type: win11
SCSI Controller: VirtIO SCSI Single
Network: VirtIO
Agent: Enabled

Disks:
- SCSI0: 100GB, cache=writeback, iothread=on, discard=on, ssd=on
- EFI Disk: 1MB
- TPM State: 4MB (required for Windows 11)
```

## Linux Template Requirements (Optional)

If using Proxmox for Linux runners instead of Windows:

### Core Configuration
- [ ] **Ubuntu Server 22.04/24.04 LTS** (or preferred distro)
- [ ] **cloud-init configured** for dynamic hostname
- [ ] **QEMU Guest Agent installed**
- [ ] **SSH server configured**
- [ ] **authorized_keys for orchestrator**

### CI Setup
- [ ] **GitHub Actions runner downloaded** to `~/actions-runner/`
- [ ] **Build dependencies installed** (git, docker, etc.)

## Hostname Strategy

**Problem**: Windows requires reboot after hostname change

**Recommendation**: Accept random hostname (simplest)
- Let Windows keep random hostname from template
- GitHub runner identifies by unique runner name, not hostname
- Pros: No setup needed, fastest
- Cons: Less clear in logs/monitoring

**Alternative**: Registry-based hostname change (no reboot)
```powershell
# Changes hostname without reboot (services won't see it until reboot)
$newName = "runner-123"
Set-ItemProperty -Path "HKLM:\SYSTEM\CurrentControlSet\Control\ComputerName\ComputerName" -Name "ComputerName" -Value $newName
Set-ItemProperty -Path "HKLM:\SYSTEM\CurrentControlSet\Control\ComputerName\ActiveComputerName" -Name "ComputerName" -Value $newName
Set-ItemProperty -Path "HKLM:\SYSTEM\CurrentControlSet\Services\Tcpip\Parameters" -Name "Hostname" -Value $newName
Set-ItemProperty -Path "HKLM:\SYSTEM\CurrentControlSet\Services\Tcpip\Parameters" -Name "NV Hostname" -Value $newName
```

## Storage Configuration

### LVM-Thin Setup (Recommended)

For fast linked clones:

```bash
# Create thin pool on existing volume group
lvcreate -L 500G -T pve/data

# Add to Proxmox storage configuration
pvesm add lvmthin local-lvm --vgname pve --thinpool data
```

### Capacity Planning

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
Clone size: 50GB each
Total storage needed: 50 + (10 * 50) = 550GB
```

## Timing Expectations

**With LVM-Thin (Linked Clones)**:
| Step | Duration |
|------|----------|
| Clone creation | 10-30 sec |
| VM boot | 30-45 sec |
| Guest agent ready | 5-10 sec |
| SSH connection + config | 10-20 sec |
| Runner registration | 5-10 sec |
| **Total** | ~70-115 sec |

**With Full Clones**:
| Step | Duration |
|------|----------|
| Clone creation | 1-3 min |
| VM boot | 30-45 sec |
| Guest agent ready | 5-10 sec |
| SSH connection + config | 10-20 sec |
| Runner registration | 5-10 sec |
| **Total** | ~2-4 min |

## Error Handling

### Proxmox-Specific Errors

```rust
#[derive(Debug, thiserror::Error)]
pub enum ProxmoxError {
    #[error("API request failed: {0}")]
    ApiError(String),
    
    #[error("Clone task failed: {0}")]
    CloneTaskFailed(String),
    
    #[error("Guest agent not responding")]
    GuestAgentTimeout,
    
    #[error("No IP address found for VM")]
    NoIpAddress,
    
    #[error("VM {0} not found")]
    VmNotFound(u32),
    
    #[error("Storage pool {0} not available")]
    StorageUnavailable(String),
    
    #[error("Insufficient storage space")]
    InsufficientStorage,
}
```

### Recovery Strategies

| Error | Recovery |
|-------|----------|
| Clone fails | Check storage capacity, retry |
| Guest agent timeout | Force delete VM, retry |
| No IP address | Check network config, retry |
| SSH connection refused | Retry with backoff |

## Proxmox API Token Setup

Create a dedicated API token with minimal permissions:

```bash
# Create user for CI runner
pveum user add ci-runner@pve

# Create API token
pveum user token add ci-runner@pve runner

# Grant permissions (adjust path for your setup)
pveum aclmod /vms -user ci-runner@pve -role PVEVMAdmin
```

## Implementation Phases

### Phase 1: Proxmox API Client
- [ ] HTTP client with API token authentication
- [ ] Basic API operations (get, post, delete)
- [ ] Task waiting mechanism

### Phase 2: VM Operations
- [ ] Clone VM from template
- [ ] Start/stop/delete VM
- [ ] Guest agent IP detection
- [ ] SSH connection and command execution

### Phase 3: Agent Daemon
- [ ] gRPC + mTLS connection to coordinator
- [ ] Agent registration with certificate exchange
- [ ] Bidirectional stream message handling
- [ ] Automatic reconnection

### Phase 4: Runner Lifecycle
- [ ] VM creation on coordinator request
- [ ] GitHub runner configuration via SSH
- [ ] Runner completion detection
- [ ] VM cleanup after job

### Phase 5: Testing & Deployment
- [ ] Integration tests with real Proxmox
- [ ] Template validation script
- [ ] Systemd service unit
- [ ] Performance benchmarks

## Systemd Service

```ini
# /etc/systemd/system/proxmox-agent.service
[Unit]
Description=Proxmox CI Runner Agent
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=/usr/local/bin/proxmox-agent
Restart=always
RestartSec=5
Environment=RUST_LOG=info

[Install]
WantedBy=multi-user.target
```
