# Tart VM Provider Plan

The Tart provider uses a **distributed agent architecture** because Tart runs locally on each Mac host. This requires two components:
- **tart-agent**: Daemon running on each Mac host (controls local Tart CLI)
- **tart-client**: Library in coordinator that talks to remote agents

## Architecture Overview

```
┌─────────────────────────────────────────────────────────────────┐
│                         Coordinator                              │
│                                                                  │
│  ┌──────────────┐    ┌──────────────┐                           │
│  │ TartProvider │    │Agent Registry│  (manages connections)    │
│  └──────┬───────┘    └──────▲───────┘                           │
│         │                   │                                    │
│         └───────────────────┘                                    │
│                   │                                              │
│         ┌─────────▼─────────┐                                   │
│         │  Agent Connection │  (gRPC + mTLS)                    │
│         │      Handler      │                                   │
│         └─────────▲─────────┘                                   │
└───────────────────┼─────────────────────────────────────────────┘
                    │
                    │ Agents connect OUTBOUND to coordinator
                    │ (coordinator does NOT initiate connections)
                    │
┌───────────────────┴─────────────────────────────────────────────┐
│                      Mac Host Fleet                              │
│                                                                  │
│  ┌────────────────────┐    ┌────────────────────┐              │
│  │    Mac Mini #1     │    │    Mac Mini #2     │              │
│  │                    │    │                    │              │
│  │  ┌──────────────┐  │    │  ┌──────────────┐  │              │
│  │  │  tart-agent  │──┼────┼──│  tart-agent  │──┼───→ to       │
│  │  │  (connects   │  │    │  │  (connects   │  │  coordinator │
│  │  │   outbound)  │  │    │  │   outbound)  │  │              │
│  │  └──────┬───────┘  │    │  └──────┬───────┘  │              │
│  │         │          │    │         │          │              │
│  │  ┌──────▼───────┐  │    │  ┌──────▼───────┐  │              │
│  │  │   Tart CLI   │  │    │  │   Tart CLI   │  │              │
│  │  └──────┬───────┘  │    │  └──────┬───────┘  │              │
│  │         │          │    │         │          │              │
│  │  ┌──────▼───────┐  │    │  ┌──────▼───────┐  │              │
│  │  │   VMs (2)    │  │    │  │   VMs (2)    │  │              │
│  │  └──────────────┘  │    │  └──────────────┘  │              │
│  └────────────────────┘    └────────────────────┘              │
└─────────────────────────────────────────────────────────────────┘
```

**Connection Model**: Agents initiate outbound connections to the coordinator. This allows Mac hosts to sit behind NAT/firewalls without exposing any ports.

**Authentication**: gRPC with mTLS. Agents register using a short-lived token and receive a client certificate from the coordinator's CA. See CoordinatorPlan.md for certificate architecture.

## Crate Structure

```
ci-runner-coordinator/
├── tart-agent/               # Daemon on each Mac host
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs           # Agent entry point
│       ├── config.rs         # Agent configuration
│       ├── coordinator.rs    # Outbound connection to coordinator
│       ├── tart.rs           # Tart CLI wrapper
│       └── vm.rs             # VM lifecycle management
│
├── agent-proto/              # Shared protocol definitions (used by all agents)
│   ├── Cargo.toml
│   └── src/
│       └── lib.rs            # Message types, serialization
│
└── agent-lib/                # Shared agent utilities (used by all agents)
    ├── Cargo.toml
    └── src/
        ├── lib.rs
        ├── connector.rs      # gRPC connection handling
        └── reconnect.rs      # Reconnection logic with backoff
```

**Note**: The `agent-proto` and `agent-lib` crates are shared between `tart-agent` and `proxmox-agent`. See CoordinatorPlan.md for the unified agent protocol.

## Communication Model (gRPC + mTLS)

Agents connect outbound to the coordinator using gRPC with mTLS:

1. **Transport**: gRPC over HTTP/2 with TLS
2. **Authentication**: mTLS - agent presents client certificate signed by coordinator CA
3. **Protocol**: Bidirectional streaming for real-time command/response

**Protocol Definition**: See `agent-proto/proto/agent.proto` in CoordinatorPlan.md for the shared protobuf definition used by both tart-agent and proxmox-agent.

Key services:
- `RegistrationService.Register` - Exchange registration token for client certificate (no mTLS required)
- `AgentService.AgentStream` - Bidirectional streaming for commands (mTLS required)

## Tart Agent Implementation

### Main Entry Point

```rust
// tart-agent/src/main.rs

#[tokio::main]
async fn main() -> Result<()> {
    let config = Config::load()?;
    
    // Initialize Tart CLI wrapper
    let tart = TartCli::new(&config.tart)?;
    
    // Start VM manager (handles lifecycle, cleanup)
    let vm_manager = Arc::new(VmManager::new(tart, config.max_concurrent_vms));
    
    // Connect to coordinator (outbound connection)
    let connector = CoordinatorConnector::new(
        &config.coordinator_url,
        &config.agent_id,
        vm_manager.clone(),
    );
    
    info!("tart-agent connecting to {}", config.coordinator_url);
    
    // Run connection loop (reconnects on disconnect)
    connector.run_forever().await?;
    
    Ok(())
}
```

### Tart CLI Wrapper

```rust
// tart-agent/src/tart.rs

pub struct TartCli {
    tart_path: PathBuf,
}

impl TartCli {
    pub async fn clone(&self, source: &str, target: &str) -> Result<()> {
        let output = Command::new(&self.tart_path)
            .args(["clone", source, target])
            .output()
            .await?;
        
        if !output.status.success() {
            return Err(Error::CloneFailed(
                String::from_utf8_lossy(&output.stderr).to_string()
            ));
        }
        Ok(())
    }
    
    pub async fn run(&self, name: &str, options: &RunOptions) -> Result<Child> {
        let mut cmd = Command::new(&self.tart_path);
        cmd.args(["run", name, "--no-graphics"]);
        
        if let Some(dir) = &options.shared_dir {
            cmd.args(["--dir", &format!("cache:{}", dir.display())]);
        }
        
        let child = cmd
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        
        Ok(child)
    }
    
    pub async fn stop(&self, name: &str) -> Result<()> {
        let _ = Command::new(&self.tart_path)
            .args(["stop", name])
            .output()
            .await;
        Ok(())  // Ignore errors - VM may already be stopped
    }
    
    pub async fn delete(&self, name: &str) -> Result<()> {
        let output = Command::new(&self.tart_path)
            .args(["delete", name])
            .output()
            .await?;
        
        if !output.status.success() {
            return Err(Error::DeleteFailed(
                String::from_utf8_lossy(&output.stderr).to_string()
            ));
        }
        Ok(())
    }
    
    pub async fn ip(&self, name: &str) -> Result<Option<String>> {
        let output = Command::new(&self.tart_path)
            .args(["ip", name])
            .output()
            .await?;
        
        if output.status.success() {
            let ip = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if !ip.is_empty() {
                return Ok(Some(ip));
            }
        }
        Ok(None)
    }
    
    pub async fn list(&self) -> Result<Vec<TartVmInfo>> {
        let output = Command::new(&self.tart_path)
            .args(["list", "--format", "json"])
            .output()
            .await?;
        
        let vms: Vec<TartVmInfo> = serde_json::from_slice(&output.stdout)?;
        Ok(vms)
    }
}
```

### VM Manager

```rust
// tart-agent/src/vm.rs

pub struct VmManager {
    tart: TartCli,
    max_concurrent: usize,
    active_vms: RwLock<HashMap<String, VmState>>,
}

struct VmState {
    name: String,
    process: Option<Child>,
    ip_address: Option<String>,
    created_at: Instant,
}

impl VmManager {
    pub async fn create_vm(&self, name: &str, template: &str) -> Result<String> {
        // Check capacity
        let active = self.active_vms.read().await;
        if active.len() >= self.max_concurrent {
            return Err(Error::CapacityExceeded);
        }
        drop(active);
        
        // Clone from template
        self.tart.clone(template, name).await?;
        
        // Start VM
        let options = RunOptions {
            shared_dir: self.config.shared_cache_dir.clone(),
            ..Default::default()
        };
        let process = self.tart.run(name, &options).await?;
        
        // Track VM
        let vm_id = name.to_string();
        let state = VmState {
            name: name.to_string(),
            process: Some(process),
            ip_address: None,
            created_at: Instant::now(),
        };
        
        self.active_vms.write().await.insert(vm_id.clone(), state);
        
        Ok(vm_id)
    }
    
    pub async fn wait_for_ready(&self, vm_id: &str, timeout: Duration) -> Result<String> {
        let deadline = Instant::now() + timeout;
        
        // Poll for IP
        while Instant::now() < deadline {
            if let Some(ip) = self.tart.ip(vm_id).await? {
                // Update state
                if let Some(state) = self.active_vms.write().await.get_mut(vm_id) {
                    state.ip_address = Some(ip.clone());
                }
                
                // Wait for SSH
                self.wait_for_ssh(&ip, deadline - Instant::now()).await?;
                
                return Ok(ip);
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        
        Err(Error::Timeout("waiting for VM to be ready"))
    }
    
    pub async fn destroy_vm(&self, vm_id: &str) -> Result<()> {
        // Remove from tracking
        let state = self.active_vms.write().await.remove(vm_id);
        
        // Stop VM process
        if let Some(mut state) = state {
            if let Some(mut process) = state.process.take() {
                let _ = process.kill().await;
            }
        }
        
        // Stop via tart (in case process kill didn't work)
        let _ = self.tart.stop(vm_id).await;
        
        // Delete VM
        self.tart.delete(vm_id).await?;
        
        Ok(())
    }

    pub async fn configure_runner(
        &self,
        ip: &str,
        registration_token: &str,
        labels: &[String],
        runner_scope_url: &str,
    ) -> Result<()> {
        let session = self.ssh_client.connect(ip, &self.ssh_config).await?;

        // Configure runner (macOS example)
        let config_cmd = format!(
            "cd ~/actions-runner && ./config.sh --url {} --token {} --labels {} --ephemeral --unattended",
            runner_scope_url,
            registration_token,
            labels.join(","),
        );
        session.execute(&config_cmd).await?;

        // Start runner (runs in foreground until job completes)
        session.execute("cd ~/actions-runner && ./run.sh").await?;

        Ok(())
    }

    pub async fn wait_for_runner_exit(&self, vm_id: &str) -> Result<()> {
        // Poll until runner process exits (VM becomes idle)
        // For --ephemeral runners, the runner exits when job completes
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;

            // Check if runner process is still running
            // This could be via SSH or by checking VM state
            let state = self.active_vms.read().await;
            if let Some(vm_state) = state.get(vm_id) {
                if vm_state.runner_exited {
                    return Ok(());
                }
            } else {
                return Err(Error::VmNotFound);
            }
        }
    }

    pub async fn get_status(&self) -> AgentStatus {
        let active = self.active_vms.read().await;
        AgentStatus {
            max_vms: self.max_concurrent,
            active_vms: active.len(),
            available_slots: self.max_concurrent.saturating_sub(active.len()),
            vms: active.values().map(|v| v.into()).collect(),
        }
    }

    async fn wait_for_ssh(&self, ip: &str, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        
        while Instant::now() < deadline {
            match TcpStream::connect((ip, 22)).await {
                Ok(_) => return Ok(()),
                Err(_) => tokio::time::sleep(Duration::from_secs(1)).await,
            }
        }
        
        Err(Error::Timeout("waiting for SSH"))
    }
}
```

### Coordinator Connector (gRPC + mTLS)

The tart-agent uses the shared `agent-lib` crate for gRPC connection management:

```rust
// tart-agent/src/main.rs

use agent_lib::{AgentConnector, AgentCertStore};
use agent_proto::agent_service_client::AgentServiceClient;
use agent_proto::{AgentMessage, agent_message, coordinator_message};
use tonic::transport::Channel;

pub struct TartAgent {
    config: AgentConfig,
    cert_store: AgentCertStore,
    vm_manager: Arc<VmManager>,
}

impl TartAgent {
    pub async fn run(&self) -> Result<()> {
        let mut connector = AgentConnector::new(
            self.config.clone(),
            self.cert_store.clone(),
        );

        loop {
            match connector.connect().await {
                Ok(client) => {
                    info!("Connected to coordinator via mTLS");
                    if let Err(e) = self.run_stream(client).await {
                        error!("Stream error: {}", e);
                    }
                }
                Err(e) => {
                    error!("Connection failed: {}", e);
                }
            }
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }

    async fn run_stream(&self, mut client: AgentServiceClient<Channel>) -> Result<()> {
        let (tx, rx) = mpsc::channel(32);

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

        // Process coordinator commands
        while let Some(msg) = inbound.message().await? {
            match msg.payload {
                Some(coordinator_message::Payload::CreateRunner(cmd)) => {
                    let result = self.handle_create_runner(&cmd).await;
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

    async fn handle_create_runner(&self, cmd: &CreateRunnerCommand) -> CommandResult {
        let result = async {
            // 1. Clone and start VM
            let vm_id = self.vm_manager.create_vm(&cmd.vm_name, &cmd.template).await?;

            // 2. Wait for IP and SSH
            let ip = self.vm_manager.wait_for_ready(&vm_id, Duration::from_secs(120)).await?;

            // 3. Configure GitHub runner
            self.vm_manager.configure_runner(
                &ip,
                &cmd.registration_token,
                &cmd.labels,
                &cmd.runner_scope_url,
            ).await?;

            // 4. Wait for runner to complete
            self.vm_manager.wait_for_runner_exit(&vm_id).await?;

            // 5. Cleanup
            self.vm_manager.destroy_vm(&vm_id).await?;

            Ok::<_, Error>((vm_id, ip))
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

**Note**: The `AgentConnector` from `agent-lib` handles:
- Certificate storage and loading
- Registration token → certificate exchange
- mTLS connection setup
- Automatic reconnection with backoff

See CoordinatorPlan.md for the full `AgentConnector` implementation.

## Agent Registry (Coordinator Side)

The coordinator maintains a registry of connected agents and routes commands to them.

### Agent Registry

```rust
// coordinator/src/agent_registry.rs

use tokio::sync::{mpsc, RwLock};

pub struct AgentRegistry {
    agents: RwLock<HashMap<String, ConnectedAgent>>,
}

struct ConnectedAgent {
    agent_id: String,
    hostname: String,
    max_vms: usize,
    active_vms: usize,
    // Channel to send commands to this agent's connection handler
    command_tx: mpsc::Sender<CoordinatorMessage>,
    // Pending command responses
    pending_commands: RwLock<HashMap<String, oneshot::Sender<AgentMessage>>>,
}

impl AgentRegistry {
    /// Called when an agent connects
    pub async fn register_agent(
        &self,
        registration: AgentRegistration,
        command_tx: mpsc::Sender<CoordinatorMessage>,
    ) {
        let agent = ConnectedAgent {
            agent_id: registration.agent_id.clone(),
            hostname: registration.hostname,
            max_vms: registration.max_vms as usize,
            active_vms: 0,
            command_tx,
            pending_commands: RwLock::new(HashMap::new()),
        };
        
        self.agents.write().await.insert(registration.agent_id, agent);
        info!("Agent {} registered", registration.agent_id);
    }
    
    /// Called when an agent disconnects
    pub async fn unregister_agent(&self, agent_id: &str) {
        self.agents.write().await.remove(agent_id);
        warn!("Agent {} disconnected", agent_id);
    }
    
    /// Find an agent with available capacity
    pub async fn find_available_agent(&self) -> Option<String> {
        let agents = self.agents.read().await;
        for (id, agent) in agents.iter() {
            if agent.active_vms < agent.max_vms {
                return Some(id.clone());
            }
        }
        None
    }
    
    /// Send command to specific agent and wait for response
    pub async fn send_command(
        &self,
        agent_id: &str,
        command: CoordinatorMessage,
        command_id: &str,
    ) -> Result<AgentMessage> {
        let agents = self.agents.read().await;
        let agent = agents.get(agent_id).ok_or(Error::AgentNotFound)?;
        
        // Create response channel
        let (tx, rx) = oneshot::channel();
        agent.pending_commands.write().await.insert(command_id.to_string(), tx);
        
        // Send command
        agent.command_tx.send(command).await?;
        
        // Wait for response (with timeout)
        let response = tokio::time::timeout(Duration::from_secs(300), rx).await??;
        
        Ok(response)
    }
    
    /// Called when agent sends a command result
    pub async fn handle_result(&self, agent_id: &str, result: AgentMessage) {
        if let AgentMessage::Result { command_id, .. } = &result {
            let agents = self.agents.read().await;
            if let Some(agent) = agents.get(agent_id) {
                if let Some(tx) = agent.pending_commands.write().await.remove(command_id) {
                    let _ = tx.send(result);
                }
            }
        }
    }
    
    /// Get total available capacity
    pub async fn total_available_capacity(&self) -> usize {
        let agents = self.agents.read().await;
        agents.values()
            .map(|a| a.max_vms.saturating_sub(a.active_vms))
            .sum()
    }
}
```

### gRPC Agent Service Handler (Coordinator side)

```rust
// coordinator/src/agent_handler.rs

use tonic::{Request, Response, Status, Streaming};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use agent_proto::agent_service_server::AgentService;
use agent_proto::{AgentMessage, CoordinatorMessage};

pub struct AgentServiceImpl {
    registry: Arc<AgentRegistry>,
    auth_manager: Arc<AuthManager>,
}

#[tonic::async_trait]
impl AgentService for AgentServiceImpl {
    type AgentStreamStream = ReceiverStream<Result<CoordinatorMessage, Status>>;

    async fn agent_stream(
        &self,
        request: Request<Streaming<AgentMessage>>,
    ) -> Result<Response<Self::AgentStreamStream>, Status> {
        // Extract and validate agent certificate (checks revocation + expiry)
        let agent = self.auth_manager
            .validate_agent_from_request(&request)
            .await
            .map_err(|e| Status::unauthenticated(e.to_string()))?;

        let agent_id = agent.agent_id.clone();

        let mut inbound = request.into_inner();
        let (tx, rx) = mpsc::channel::<Result<CoordinatorMessage, Status>>(32);

        // Register agent with outbound channel
        self.registry.register_agent(&agent_id, tx.clone()).await;

        // Spawn task to handle inbound messages
        let registry = self.registry.clone();
        let agent_id_clone = agent_id.clone();
        tokio::spawn(async move {
            while let Some(result) = inbound.message().await.transpose() {
                match result {
                    Ok(msg) => {
                        match msg.payload {
                            Some(Payload::Status(status)) => {
                                // AgentStatus: capacity update, VM list
                                registry.update_status(&agent_id_clone, status).await;
                            }
                            Some(Payload::Result(result)) => {
                                // CommandResult: response to CreateVm, DestroyVm, etc.
                                registry.handle_command_result(&agent_id_clone, result).await;
                            }
                            Some(Payload::Pong(_)) => {
                                // Response to Ping, update last-seen timestamp
                                registry.update_last_seen(&agent_id_clone).await;
                            }
                        }
                    }
                    Err(e) => {
                        error!("Stream error from agent {}: {}", agent_id_clone, e);
                        break;
                    }
                }
            }

            // Cleanup on disconnect
            registry.unregister_agent(&agent_id_clone).await;
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}
```

### VmProvider Implementation

```rust
// coordinator/src/tart_provider.rs

pub struct TartProvider {
    registry: Arc<AgentRegistry>,
    // Track which agent each VM is on
    vm_agents: RwLock<HashMap<String, String>>,
}

#[async_trait]
impl VmProvider for TartProvider {
    async fn create_runner(
        &self,
        config: &VmConfig,
        runner_token: &str,
        runner_scope_url: &str,
    ) -> Result<VmInstance> {
        // Find an agent with capacity and matching labels
        let agent_id = self.registry
            .find_available_agent(&config.labels)
            .await
            .ok_or(Error::NoCapacityAvailable)?;

        // Send CreateRunner command to agent
        // Agent handles: VM creation, boot, SSH, runner installation
        let command_id = uuid::Uuid::new_v4().to_string();
        let command = CreateRunnerCommand {
            command_id: command_id.clone(),
            vm_name: config.name.clone(),
            template: config.template.clone(),
            registration_token: runner_token.to_string(),
            labels: config.labels.clone(),
            runner_scope_url: runner_scope_url.to_string(),
        };

        let result = self.registry
            .send_command(&agent_id, CoordinatorMessage::CreateRunner(command), &command_id)
            .await?;

        let (vm_id, ip) = match result.result {
            Some(command_result::Result::CreateRunner(r)) => (r.vm_id, r.ip_address),
            _ => return Err(Error::UnexpectedResponse),
        };

        if !result.success {
            return Err(Error::AgentError(result.error));
        }

        // Track which agent this VM is on
        self.vm_agents.write().await.insert(vm_id.clone(), agent_id.clone());

        Ok(VmInstance {
            id: vm_id,
            name: config.name.clone(),
            ip_address: Some(ip),
            host: Some(agent_id),
        })
    }

    async fn destroy_vm(&self, vm: &VmInstance) -> Result<()> {
        let agent_id = self.vm_agents.write().await.remove(&vm.id)
            .ok_or(Error::VmNotFound)?;

        let command_id = uuid::Uuid::new_v4().to_string();
        let command = DestroyRunnerCommand {
            command_id: command_id.clone(),
            vm_id: vm.id.clone(),
        };

        self.registry
            .send_command(&agent_id, CoordinatorMessage::DestroyRunner(command), &command_id)
            .await?;

        Ok(())
    }

    async fn available_capacity(&self) -> Result<usize> {
        Ok(self.registry.total_available_capacity().await)
    }

    fn provider_name(&self) -> &'static str {
        "tart"
    }

    fn os_type(&self) -> OsType {
        OsType::MacOS
    }
}
```

## Agent Configuration

### Config File (on each Mac)

```toml
# ~/Library/Application Support/tart-agent/config.toml

[coordinator]
# gRPC endpoint (note: https, not wss)
url = "https://coordinator.example.com:9443"
hostname = "coordinator.example.com"  # For TLS verification

# For FIRST RUN only: registration token from coordinator
# Generate with: coordinator token create --labels "macos,arm64"
# After successful registration, this can be removed (certificate is stored)
registration_token = "reg_a1b2c3d4e5f6..."

[tls]
# CA certificate from coordinator (required - get with: coordinator ca export)
ca_cert = "~/Library/Application Support/tart-agent/certs/ca.crt"

# Client certificate directory (auto-populated after registration)
# Contains: client.crt, client.key, metadata.json
certs_dir = "~/Library/Application Support/tart-agent/certs"

[agent]
# Labels this agent advertises to the coordinator
labels = ["self-hosted", "macos", "arm64"]

[tart]
base_image = "macos-runner"     # Default template
max_concurrent_vms = 2          # Apple Virtualization framework limit
shared_cache_dir = "~/Library/Caches/tart-agent"

# Optional: Cleanup settings
[cleanup]
max_vm_age_hours = 2            # Force cleanup VMs older than this
cleanup_interval_mins = 15       # How often to run cleanup

# Optional: Reconnection settings
[reconnect]
initial_delay_secs = 1
max_delay_secs = 60
```

### First-Time Setup

```bash
# 1. Create config and certs directories
$ mkdir -p ~/Library/Application\ Support/tart-agent/certs

# 2. Get CA certificate from coordinator
$ scp coordinator.example.com:/etc/ci-runner/ca.crt \
    ~/Library/Application\ Support/tart-agent/certs/ca.crt

# 3. Generate a registration token on the coordinator
$ ssh coordinator.example.com "coordinator token create --labels 'macos,arm64' --expires 1h"
Registration token created:
  Token: reg_a1b2c3d4e5f6...
  Labels: macos, arm64
  Expires: 2024-01-15T15:30:00Z

# 4. Create config file with the token
$ vim ~/Library/Application\ Support/tart-agent/config.toml
# Set registration_token and other settings

# 5. Start the agent (or load LaunchAgent)
$ tart-agent --config ~/Library/Application\ Support/tart-agent/config.toml

# 6. Agent logs show successful registration
INFO Connecting to coordinator for registration...
INFO Successfully registered as agent_tart_mac-mini-1_a1b2c3
INFO Client certificate saved to ~/Library/Application Support/tart-agent/certs/
INFO Certificate expires: 2025-01-15T14:30:00Z
INFO Connected to coordinator via mTLS, ready for work

# 7. (Optional) Remove registration token from config
# The agent now uses the client certificate for authentication
```

### Certificate Storage

After successful registration, certificates are stored in the `certs_dir`:

```
~/Library/Application Support/tart-agent/certs/
├── ca.crt           # Coordinator CA (you provided this)
├── client.crt       # Agent's client certificate (from registration)
├── client.key       # Agent's private key (mode 0600)
└── metadata.json    # Registration metadata
```

```json
// metadata.json
{
  "agent_id": "agent_tart_mac-mini-1_a1b2c3",
  "registered_at": "2024-01-15T14:30:00Z",
  "expires_at": "2025-01-15T14:30:00Z"
}
```

The agent will:
1. Check for valid client certificate on startup
2. Use mTLS for all gRPC communication
3. Fall back to registration token if certificate is missing/expired/revoked
4. Automatically clear certificates if revoked by coordinator

### LaunchAgent (User-level daemon on macOS)

For running as a user-level daemon (recommended):

```xml
<!-- ~/Library/LaunchAgents/com.ci-runner.tart-agent.plist -->
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.ci-runner.tart-agent</string>
    <key>ProgramArguments</key>
    <array>
        <string>/usr/local/bin/tart-agent</string>
        <string>--config</string>
        <string>~/Library/Application Support/tart-agent/config.toml</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>~/Library/Logs/tart-agent.log</string>
    <key>StandardErrorPath</key>
    <string>~/Library/Logs/tart-agent.log</string>
    <key>WorkingDirectory</key>
    <string>~</string>
</dict>
</plist>
```

**Load/Unload commands:**
```bash
# Load (start at login)
launchctl load ~/Library/LaunchAgents/com.ci-runner.tart-agent.plist

# Unload (stop)
launchctl unload ~/Library/LaunchAgents/com.ci-runner.tart-agent.plist

# Check status
launchctl list | grep tart-agent
```

**Note**: LaunchAgents run as the current user, which is required for Tart since it uses the user's Virtualization framework entitlements. LaunchDaemons (system-level) won't work for Tart.

## macOS Template Requirements

### Base Image Setup

- [ ] **macOS 13+ (Ventura or later)**
- [ ] **User account created** with known credentials
- [ ] **SSH enabled** (System Settings → General → Sharing → Remote Login)
- [ ] **authorized_keys configured** for coordinator SSH key
- [ ] **Password authentication disabled** (optional)

### GitHub Runner Setup

- [ ] **Runner binaries downloaded** to `~/actions-runner/`
- [ ] **Xcode installed** (if building iOS/macOS apps)
- [ ] **Xcode Command Line Tools** installed
- [ ] **Homebrew installed** (optional)
- [ ] **Common dependencies pre-installed**

### Performance Optimizations

- [ ] **Disable Spotlight indexing** on build directories
- [ ] **Disable Time Machine**
- [ ] **Disable automatic updates**
- [ ] **Configure appropriate memory/CPU**

## Limitations

### Apple Virtualization Framework
- **Maximum 2 concurrent VMs** per Mac host
- Only runs on Apple Silicon (M1/M2/M3/M4)
- Only virtualizes macOS and Linux

### Tart-Specific
- No snapshot support (clones are full copies)
- Clone time depends on disk size (~10-30 seconds for 50GB image)
- VMs must be named uniquely per host

## Timing Expectations

| Step | Duration |
|------|----------|
| Clone VM | 10-30 sec |
| VM boot | 15-25 sec |
| SSH available | 5-10 sec |
| Runner config | 5-10 sec |
| **Total** | ~35-75 sec |

## Error Handling

### Agent Errors

```rust
#[derive(Debug, thiserror::Error)]
pub enum TartAgentError {
    #[error("Capacity exceeded: max {0} VMs")]
    CapacityExceeded(usize),
    
    #[error("Clone failed: {0}")]
    CloneFailed(String),
    
    #[error("VM not found: {0}")]
    VmNotFound(String),
    
    #[error("Timeout: {0}")]
    Timeout(&'static str),
    
    #[error("Tart CLI error: {0}")]
    TartError(String),
}
```

### Client Errors

```rust
#[derive(Debug, thiserror::Error)]
pub enum TartClientError {
    #[error("No hosts with available capacity")]
    NoCapacityAvailable,
    
    #[error("Host {0} unreachable")]
    HostUnreachable(String),
    
    #[error("All hosts unhealthy")]
    AllHostsUnhealthy,
    
    #[error("VM {0} not found")]
    VmNotFound(String),
    
    #[error("Request failed: {0}")]
    RequestFailed(#[from] reqwest::Error),
}
```

## Security Considerations

### Agent Security
- Bind to internal network only (not public internet)
- Consider mTLS between coordinator and agents
- No sensitive data stored on agent
- Audit logging for all VM operations

### Network Security
- Use private network/VLAN for agent communication
- Firewall rules to restrict access to port 9090
- SSH from coordinator to VMs only (not through agent)

## Monitoring

### Agent Metrics

```
# VM lifecycle
tart_agent_vms_created_total
tart_agent_vms_destroyed_total
tart_agent_vm_creation_duration_seconds

# Capacity
tart_agent_active_vms
tart_agent_available_slots
tart_agent_max_vms

# Health
tart_agent_healthy
tart_agent_uptime_seconds
```

### Logging

Agent should log:
- All VM operations (create, destroy, wait_for_ready)
- Errors and retries
- Periodic status snapshots
- Cleanup operations

## Implementation Phases

### Phase 1: Tart CLI Wrapper
- [ ] Clone, run, stop, delete operations
- [ ] IP address detection
- [ ] List VMs (JSON parsing)

### Phase 2: VM Manager
- [ ] Capacity tracking (max 2 VMs)
- [ ] SSH availability check
- [ ] GitHub runner configuration via SSH

### Phase 3: Agent Daemon
- [ ] gRPC + mTLS connection to coordinator
- [ ] Agent registration with certificate exchange
- [ ] Bidirectional stream message handling
- [ ] Automatic reconnection with backoff

### Phase 4: Runner Lifecycle
- [ ] VM creation on coordinator request
- [ ] Runner completion detection
- [ ] VM cleanup after job
- [ ] Zombie VM cleanup

### Phase 5: Deployment & Monitoring
- [ ] LaunchAgent plist
- [ ] Metrics endpoints
- [ ] Health checks
- [ ] Graceful shutdown
