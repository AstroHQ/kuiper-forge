# KuiperForge Coordinator Plan

The coordinator is the main daemon that manages ephemeral GitHub Actions runners across multiple VM providers (Tart for macOS, Proxmox for Windows/Linux).

## High-Level Architecture

Following the [Tartelet](https://github.com/shapehq/tartelet) pattern - **no webhooks required**:

```
┌─────────────────────────────────────────────────────────────────────────────┐
│                            KuiperForge                                      │
│                        (centralized daemon)                                 │
├─────────────────────────────────────────────────────────────────────────────┤
│  ┌─────────────────┐  ┌─────────────────┐  ┌─────────────────┐              │
│  │  GitHub Client  │  │  Fleet Manager  │  │  Agent Registry │              │
│  │  (REST API)     │  │  (Task Pool)    │  │  (connections)  │              │
│  └────────┬────────┘  └────────┬────────┘  └────────┬────────┘              │
│           │                    │                    │                       │
│           └──────────┬─────────┴────────────────────┘                       │
│                      │                                                      │
│           ┌──────────▼──────────┐                                           │
│           │   Agent Provider    │  (unified interface to all agents)        │
│           └──────────▲──────────┘                                           │
│                      │                                                      │
└──────────────────────┼──────────────────────────────────────────────────────┘
                       │
                       │ ALL agents connect OUT to coordinator
                       │ (gRPC with mTLS)
                       │
┌──────────────────────┴───────────────────────────────────────────────────────┐
│                           Agent Fleet                                         │
│              (all agents initiate outbound connections)                       │
│                                                                               │
│  ┌─────────────────────────────────┐  ┌─────────────────────────────────┐   │
│  │         Mac Hosts               │  │        Proxmox Hosts            │   │
│  │                                 │  │                                 │   │
│  │  ┌───────────┐  ┌───────────┐  │  │  ┌─────────────┐                │   │
│  │  │tart-agent │  │tart-agent │──┼──┼──│proxmox-agent│────────────────┼───│
│  │  │  :mac-1   │  │  :mac-2   │  │  │  │ (connects   │  → to          │   │
│  │  └─────┬─────┘  └─────┬─────┘  │  │  │  outbound)  │  coordinator   │   │
│  │        │              │        │  │  └──────┬──────┘                │   │
│  │  ┌─────▼─────┐  ┌─────▼─────┐  │  │         │                       │   │
│  │  │ Tart CLI  │  │ Tart CLI  │  │  │  ┌──────▼──────┐                │   │
│  │  └─────┬─────┘  └─────┴─────┘  │  │  │ Proxmox API │                │   │
│  │        │              │        │  │  └──────┬──────┘                │   │
│  │  ┌─────▼─────┐  ┌─────▼─────┐  │  │         │                       │   │
│  │  │  VMs (2)  │  │  VMs (2)  │  │  │  ┌──────▼──────┐                │   │
│  │  └───────────┘  └───────────┘  │  │  │  VMs (N)    │                │   │
│  │                                 │  │  └─────────────┘                │   │
│  └─────────────────────────────────┘  └─────────────────────────────────┘   │
└──────────────────────────────────────────────────────────────────────────────┘
```

**Connection Model**: ALL agents connect outbound to coordinator (not the reverse). This allows agents to sit behind NAT/firewalls without exposing ports.

**Unified Agent Protocol**: Both tart-agent and proxmox-agent use the same protocol to communicate with the coordinator. The agent type and capabilities are identified during registration.

**Transport**: gRPC with mTLS (mutual TLS) - agents present client certificates signed by coordinator's CA.

**Authentication**: Registration token model (like GitHub runners) - short-lived registration token exchanged for a client certificate.

## Agent Types

| Aspect | proxmox-agent | tart-agent |
|--------|---------------|------------|
| **Runs on** | Any host with Proxmox API access | Each Mac host |
| **Controls** | Proxmox VE via REST API | Tart CLI locally |
| **VM Types** | Windows, Linux | macOS |
| **Max VMs** | Configurable (Proxmox limits) | 2 (Apple Virtualization limit) |
| **Connection** | Outbound to coordinator | Outbound to coordinator |

## Crate Structure

```
kuiper-forge/
├── Cargo.toml                 # Workspace root
│
├── coordinator/               # Centralized daemon (runs on server)
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs           # Daemon entry point
│       ├── config.rs         # Configuration loading
│       ├── github.rs         # GitHub API client
│       ├── fleet.rs          # Fleet management
│       ├── runner.rs         # Runner setup logic
│       ├── agent_registry.rs # Connected agent management
│       └── agent_handler.rs  # gRPC agent service handler
│
├── agent-proto/              # Shared protocol definitions (both agent types)
│   ├── Cargo.toml
│   └── src/
│       └── lib.rs            # Message types, serialization
│
├── tart-agent/               # Agent daemon (runs on each Mac)
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs           # Agent entry point
│       ├── connector.rs      # Outbound connection to coordinator
│       ├── tart.rs           # Tart CLI wrapper
│       └── vm.rs             # VM lifecycle management
│
├── proxmox-agent/            # Agent daemon (runs with Proxmox API access)
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs           # Agent entry point
│       ├── connector.rs      # Outbound connection to coordinator
│       ├── proxmox.rs        # Proxmox REST API client
│       └── vm.rs             # VM lifecycle management
│
└── agent-lib/                # Shared agent code (connection, protocol handling)
    ├── Cargo.toml
    └── src/
        └── lib.rs            # Common connector, message handling
```

## VM Provider Trait

The coordinator uses a common trait for VM providers:

```rust
use async_trait::async_trait;

/// Represents a running VM instance
pub struct VmInstance {
    pub id: String,           // Unique identifier
    pub name: String,         // Human-readable name
    pub ip_address: Option<String>,
    pub host: Option<String>, // For Tart: which Mac host this runs on
}

/// Configuration for creating a new VM
pub struct VmConfig {
    pub name: String,         // Name for the new VM
    pub template: String,     // Base template/image to clone from
    pub labels: Vec<String>,  // Runner labels
}

#[async_trait]
pub trait VmProvider: Send + Sync {
    /// Request agent to create a VM and configure runner
    /// Agent handles: VM creation, SSH setup, runner installation
    async fn create_runner(
        &self,
        config: &VmConfig,
        runner_token: &str,
        runner_scope_url: &str,  // GitHub org/repo URL for runner registration
    ) -> Result<VmInstance>;

    /// Request agent to destroy a VM
    async fn destroy_vm(&self, vm: &VmInstance) -> Result<()>;

    /// Get provider name (for logging)
    fn provider_name(&self) -> &'static str;

    /// Get OS type (for runner binary selection)
    fn os_type(&self) -> OsType;

    /// Get current capacity (available slots across connected agents)
    async fn available_capacity(&self) -> Result<usize>;
}

pub enum OsType {
    MacOS,
    Windows,
    Linux,
}
```

## Agent Registry

The coordinator maintains a registry of all connected agents (both Tart and Proxmox):

```rust
pub struct AgentRegistry {
    agents: RwLock<HashMap<String, ConnectedAgent>>,
}

pub struct ConnectedAgent {
    pub agent_id: String,
    pub agent_type: AgentType,     // Tart or Proxmox
    pub hostname: String,
    pub os_type: OsType,           // macOS, Windows, Linux
    pub max_vms: usize,
    pub active_vms: usize,
    pub labels: Vec<String>,       // e.g., ["macos", "arm64"] or ["windows", "x64"]
    pub command_tx: mpsc::Sender<CoordinatorMessage>,
    pub pending_commands: RwLock<HashMap<String, oneshot::Sender<AgentMessage>>>,
}

pub enum AgentType {
    Tart,
    Proxmox,
}

impl AgentRegistry {
    /// Find an agent with capacity matching the required labels
    pub async fn find_available_agent(&self, labels: &[String]) -> Option<String> {
        let agents = self.agents.read().await;
        for (id, agent) in agents.iter() {
            // Check capacity
            if agent.active_vms >= agent.max_vms {
                continue;
            }
            // Check label match
            if labels.iter().all(|l| agent.labels.contains(l)) {
                return Some(id.clone());
            }
        }
        None
    }

    /// Send command to agent and wait for response
    pub async fn send_command(
        &self,
        agent_id: &str,
        command: CoordinatorMessage,
    ) -> Result<AgentMessage> {
        let command_id = uuid::Uuid::new_v4().to_string();
        // ... send command, wait for response with timeout
    }

    /// Get total capacity by label
    pub async fn available_capacity(&self, labels: &[String]) -> usize {
        let agents = self.agents.read().await;
        agents.values()
            .filter(|a| labels.iter().all(|l| a.labels.contains(l)))
            .map(|a| a.max_vms.saturating_sub(a.active_vms))
            .sum()
    }
}
```

## Agent Authentication (gRPC + mTLS)

Using gRPC with mutual TLS (mTLS) for secure agent-coordinator communication:

1. **Registration Token**: Short-lived token (default: 1 hour) generated by coordinator
2. **Certificate Issuance**: Agent presents registration token, receives signed client certificate
3. **mTLS Connection**: Agent uses client certificate for all future gRPC connections

### Certificate Architecture

```
┌─────────────────────────────────────────────────────────────────────┐
│                    Coordinator CA (Certificate Authority)            │
│                                                                      │
│  ┌─────────────────────────────┐                                    │
│  │  CA Private Key             │  (never leaves coordinator)        │
│  │  /etc/ci-runner/ca.key      │                                    │
│  └─────────────────────────────┘                                    │
│                 │                                                    │
│                 │ Signs                                              │
│                 ▼                                                    │
│  ┌─────────────────────────────┐    ┌─────────────────────────────┐│
│  │  Coordinator Server Cert    │    │  Agent Client Certs         ││
│  │  coordinator.crt            │    │  agent_xxx.crt (per agent)  ││
│  └─────────────────────────────┘    └─────────────────────────────┘│
└─────────────────────────────────────────────────────────────────────┘
```

### Authentication Flow

```
┌─────────────────┐                              ┌─────────────────┐
│   Coordinator   │                              │     Agent       │
└────────┬────────┘                              └────────┬────────┘
         │                                                │
         │  1. Admin generates registration token         │
         │     $ coordinator token create --labels ...    │
         │  ─────────────────────────────────────────►    │
         │     reg_token (expires in 1h)                  │
         │                                                │
         │  2. Agent calls Register RPC over TLS           │
         │     (server-authenticated, no client cert yet) │
         │  ◄─────────────────────────────────────────    │
         │     gRPC: Register(token, hostname, labels)    │
         │                                                │
         │  3. Coordinator validates & CONSUMES token     │
         │     (token now invalid, cannot be reused)      │
         │  ─────────────────────────────────────────►    │
         │     { agent_id, client_cert, client_key }      │
         │                                                │
         │  4. Agent stores certificate locally           │
         │     ~/Library/Application Support/.../certs/   │
         │                                                │
         │  ═══════════════════════════════════════════   │
         │           SUBSEQUENT CONNECTIONS (mTLS)        │
         │  ═══════════════════════════════════════════   │
         │                                                │
         │  5. Agent connects with client certificate     │
         │  ◄─────────────────────────────────────────    │
         │     gRPC + mTLS (client cert in TLS handshake) │
         │                                                │
         │  6. Coordinator validates cert, extracts ID    │
         │     (agent_id embedded in cert CN/SAN)         │
         │                                                │
         │  7. Bidirectional streaming begins             │
         │  ◄────────────────────────────────────────►    │
         │     AgentStream RPC                            │
         │                                                │
```

### Coordinator CA Setup

```bash
# Initialize the CA (one-time setup)
$ coordinator ca init
Generated CA certificate and key:
  CA cert: /etc/ci-runner/ca.crt
  CA key:  /etc/ci-runner/ca.key (keep secure!)

# Generate server certificate for coordinator
$ coordinator ca server-cert --hostname coordinator.example.com
Generated server certificate:
  Server cert: /etc/ci-runner/server.crt
  Server key:  /etc/ci-runner/server.key

# Export CA cert for agents (they need to trust it)
$ coordinator ca export > ca.crt
# Distribute ca.crt to agent machines
```

### Registration Token & Certificate Issuance

```rust
// coordinator/src/auth.rs

use rcgen::{Certificate, CertificateParams, DistinguishedName, DnType, KeyPair};
use chrono::{Duration, Utc};

pub struct AuthManager {
    db: Database,
    ca_cert: Certificate,
    ca_key: KeyPair,
}

#[derive(Debug, Clone)]
pub struct RegistrationToken {
    pub token: String,
    pub labels: Vec<String>,
    pub expires_at: DateTime<Utc>,
    pub created_by: String,
}

#[derive(Debug, Clone)]
pub struct AgentCertificate {
    pub agent_id: String,
    pub labels: Vec<String>,
    pub cert_pem: String,      // X.509 certificate (PEM)
    pub key_pem: String,       // Private key (PEM) - only returned once
    pub expires_at: DateTime<Utc>,
    pub serial_number: String,
}

#[derive(Debug, Clone)]
pub struct RegisteredAgent {
    pub agent_id: String,
    pub labels: Vec<String>,
    pub serial_number: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub revoked: bool,
}

impl AuthManager {
    /// Generate a new registration token (admin action)
    pub async fn create_registration_token(
        &self,
        labels: Vec<String>,
        ttl: Duration,
        created_by: &str,
    ) -> Result<RegistrationToken> {
        let token = format!("reg_{}", generate_random_string(32));
        let expires_at = Utc::now() + ttl;

        let reg_token = RegistrationToken {
            token: token.clone(),
            labels,
            expires_at,
            created_by: created_by.to_string(),
        };

        self.db.store_registration_token(&reg_token).await?;
        Ok(reg_token)
    }

    /// Exchange registration token for client certificate
    pub async fn exchange_token_for_certificate(
        &self,
        token: &str,
        agent_hostname: &str,
        agent_type: &str,
    ) -> Result<AgentCertificate> {
        // 1. Validate registration token
        let reg_token = self.db.get_registration_token(token).await?
            .ok_or(AuthError::InvalidToken)?;

        if reg_token.expires_at < Utc::now() {
            return Err(AuthError::TokenExpired);
        }

        // 2. Consume the token (one-time use)
        self.db.delete_registration_token(token).await?;

        // 3. Generate agent ID
        let agent_id = format!("agent_{}_{}_{}",
            agent_type,
            sanitize_hostname(agent_hostname),
            generate_random_string(8)
        );

        // 4. Generate client certificate
        let cert_validity = Duration::days(365); // 1 year
        let expires_at = Utc::now() + cert_validity;

        let mut params = CertificateParams::default();
        params.distinguished_name = DistinguishedName::new();
        params.distinguished_name.push(DnType::CommonName, &agent_id);
        params.distinguished_name.push(DnType::OrganizationName, "CI Runner Agent");

        // Embed labels in certificate (as custom extension or SAN)
        params.subject_alt_names = vec![
            rcgen::SanType::DnsName(agent_id.clone()),
        ];

        // Set validity period
        params.not_before = Utc::now();
        params.not_after = expires_at;

        // Generate key pair
        let key_pair = KeyPair::generate(&rcgen::PKCS_ECDSA_P256_SHA256)?;
        params.key_pair = Some(key_pair);

        let cert = Certificate::from_params(params)?;
        let cert_pem = cert.serialize_pem_with_signer(&self.ca_cert)?;
        let key_pem = cert.serialize_private_key_pem();
        let serial = hex::encode(cert.get_serial_number());

        // 5. Record agent in database
        let registered = RegisteredAgent {
            agent_id: agent_id.clone(),
            labels: reg_token.labels.clone(),
            serial_number: serial.clone(),
            created_at: Utc::now(),
            expires_at,
            revoked: false,
        };
        self.db.store_registered_agent(&registered).await?;

        Ok(AgentCertificate {
            agent_id,
            labels: reg_token.labels,
            cert_pem,
            key_pem,
            expires_at,
            serial_number: serial,
        })
    }

    /// Validate agent from mTLS connection (called during TLS handshake)
    pub async fn validate_agent_certificate(
        &self,
        cert: &x509_parser::certificate::X509Certificate,
    ) -> Result<RegisteredAgent> {
        // Extract agent_id from CN
        let cn = cert.subject()
            .iter_common_name()
            .next()
            .and_then(|cn| cn.as_str().ok())
            .ok_or(AuthError::InvalidCertificate)?;

        let agent = self.db.get_registered_agent(cn).await?
            .ok_or(AuthError::AgentNotFound)?;

        // Check if revoked
        if agent.revoked {
            return Err(AuthError::CertificateRevoked);
        }

        // Check expiry (TLS layer should also check, but double-verify)
        if agent.expires_at < Utc::now() {
            return Err(AuthError::CertificateExpired);
        }

        Ok(agent)
    }

    /// Revoke agent certificate (admin action)
    pub async fn revoke_agent(&self, agent_id: &str) -> Result<()> {
        self.db.mark_agent_revoked(agent_id).await?;
        // Force disconnect if currently connected
        Ok(())
    }

    /// List all registered agents
    pub async fn list_agents(&self) -> Result<Vec<RegisteredAgent>> {
        self.db.list_registered_agents().await
    }
}

fn generate_random_string(len: usize) -> String {
    rand::thread_rng()
        .sample_iter(&rand::distributions::Alphanumeric)
        .take(len)
        .map(char::from)
        .collect()
}
```

### CLI Commands

```bash
# === CA Management ===

# Initialize CA (first-time setup)
$ coordinator ca init
Generated CA certificate and key:
  CA cert: /etc/ci-runner/ca.crt
  CA key:  /etc/ci-runner/ca.key

# Generate server certificate
$ coordinator ca server-cert --hostname coordinator.example.com
Generated server certificate:
  Cert: /etc/ci-runner/server.crt
  Key:  /etc/ci-runner/server.key

# Export CA cert (distribute to agents)
$ coordinator ca export
-----BEGIN CERTIFICATE-----
MIIBkTCB+wIJAK... (copy this to agents)
-----END CERTIFICATE-----

# === Registration Tokens ===

# Generate a registration token
$ coordinator token create --labels "macos,arm64" --expires 1h
Registration token created:
  Token: reg_a1b2c3d4e5f6...
  Labels: macos, arm64
  Expires: 2024-01-15T15:30:00Z (in 1 hour)

Copy this token to the agent configuration to register it.
⚠️  Token is single-use and expires in 1 hour. Generate new tokens as needed.

# List pending registration tokens
$ coordinator token list
TOKEN                    LABELS           EXPIRES              CREATED BY
reg_a1b2c3...           macos,arm64      2024-01-15T15:30:00  admin
reg_x7y8z9...           windows,x64      2024-01-15T16:00:00  admin

# Revoke a registration token
$ coordinator token revoke reg_a1b2c3...

# === Agent Management ===

# List registered agents
$ coordinator agent list
AGENT ID                      LABELS           CERT EXPIRES         STATUS
agent_tart_mac-mini-1_a1b2    macos,arm64      2025-01-15           online
agent_tart_mac-mini-2_c3d4    macos,arm64      2025-01-15           online
agent_proxmox_pve_e5f6        windows,x64      2025-01-15           offline (2m)

# Revoke agent certificate (force re-registration)
$ coordinator agent revoke agent_tart_mac-mini-1_a1b2
Agent certificate revoked. Agent will be disconnected and must re-register.

# Renew agent certificate (before expiry)
$ coordinator agent renew agent_tart_mac-mini-1_a1b2 --days 365
Certificate renewed. Agent will receive new cert on next connection.
```

### Agent-Side Certificate Storage

```rust
// agent-lib/src/certs.rs

use std::path::PathBuf;

pub struct AgentCertStore {
    base_dir: PathBuf,
}

impl AgentCertStore {
    pub fn new(base_dir: PathBuf) -> Self {
        Self { base_dir }
    }

    /// Default cert store location
    pub fn default_path() -> PathBuf {
        // macOS: ~/Library/Application Support/tart-agent/certs/
        // Linux: ~/.config/proxmox-agent/certs/
        dirs::config_dir()
            .unwrap()
            .join(env!("CARGO_PKG_NAME"))
            .join("certs")
    }

    fn cert_path(&self) -> PathBuf {
        self.base_dir.join("client.crt")
    }

    fn key_path(&self) -> PathBuf {
        self.base_dir.join("client.key")
    }

    fn ca_path(&self) -> PathBuf {
        self.base_dir.join("ca.crt")
    }

    fn metadata_path(&self) -> PathBuf {
        self.base_dir.join("metadata.json")
    }

    /// Check if we have valid stored certificates
    pub fn has_certificates(&self) -> bool {
        self.cert_path().exists() && self.key_path().exists()
    }

    /// Load certificate and key for mTLS connection
    pub fn load_identity(&self) -> Result<tonic::transport::Identity> {
        let cert = std::fs::read_to_string(self.cert_path())?;
        let key = std::fs::read_to_string(self.key_path())?;
        Ok(tonic::transport::Identity::from_pem(cert, key))
    }

    /// Load CA certificate for server verification
    pub fn load_ca(&self) -> Result<tonic::transport::Certificate> {
        let ca = std::fs::read_to_string(self.ca_path())?;
        Ok(tonic::transport::Certificate::from_pem(ca))
    }

    /// Save certificates received during registration
    pub fn save(&self, cert_pem: &str, key_pem: &str, agent_id: &str) -> Result<()> {
        // Create directory with restricted permissions
        std::fs::create_dir_all(&self.base_dir)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&self.base_dir,
                std::fs::Permissions::from_mode(0o700))?;
        }

        // Write certificate (can be readable)
        std::fs::write(self.cert_path(), cert_pem)?;

        // Write private key with restricted permissions
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .mode(0o600)
                .open(self.key_path())?;
            std::io::Write::write_all(&mut file, key_pem.as_bytes())?;
        }

        #[cfg(not(unix))]
        std::fs::write(self.key_path(), key_pem)?;

        // Write metadata
        let metadata = serde_json::json!({
            "agent_id": agent_id,
            "registered_at": chrono::Utc::now().to_rfc3339(),
        });
        std::fs::write(self.metadata_path(), metadata.to_string())?;

        Ok(())
    }

    /// Save CA certificate (provided during initial setup)
    pub fn save_ca(&self, ca_pem: &str) -> Result<()> {
        std::fs::create_dir_all(&self.base_dir)?;
        std::fs::write(self.ca_path(), ca_pem)?;
        Ok(())
    }

    /// Clear stored certificates (on revocation)
    pub fn clear(&self) -> Result<()> {
        if self.base_dir.exists() {
            std::fs::remove_dir_all(&self.base_dir)?;
        }
        Ok(())
    }

    /// Get agent ID from stored metadata
    pub fn get_agent_id(&self) -> Option<String> {
        let content = std::fs::read_to_string(self.metadata_path()).ok()?;
        let metadata: serde_json::Value = serde_json::from_str(&content).ok()?;
        metadata["agent_id"].as_str().map(String::from)
    }
}
```

### Agent Connection Logic (gRPC)

```rust
// agent-lib/src/connector.rs

use tonic::transport::{Channel, ClientTlsConfig, Identity, Certificate};
use crate::proto::agent_service_client::AgentServiceClient;
use crate::proto::registration_service_client::RegistrationServiceClient;

pub struct AgentConnector {
    config: AgentConfig,
    cert_store: AgentCertStore,
}

impl AgentConnector {
    /// Connect to coordinator, registering if needed
    pub async fn connect(&mut self) -> Result<AgentServiceClient<Channel>> {
        // Check for stored certificates
        if self.cert_store.has_certificates() {
            match self.connect_with_mtls().await {
                Ok(client) => return Ok(client),
                Err(e) if e.is_auth_error() => {
                    // Certificate revoked or expired, need to re-register
                    warn!("Certificate rejected: {}, clearing...", e);
                    self.cert_store.clear()?;
                }
                Err(e) => return Err(e),
            }
        }

        // No valid certificate, need registration token
        let reg_token = self.config.registration_token.as_ref()
            .ok_or_else(|| Error::NoCredentials(
                "No stored certificate and no registration token provided. \
                 Run: coordinator token create --labels ..."
            ))?;

        self.register_and_connect(reg_token).await
    }

    /// Register with coordinator using registration token
    async fn register_and_connect(&mut self, token: &str) -> Result<AgentServiceClient<Channel>> {
        // Connect to registration endpoint (server TLS only, no client cert)
        let ca_cert = self.cert_store.load_ca()?;
        let tls = ClientTlsConfig::new()
            .ca_certificate(ca_cert)
            .domain_name(&self.config.coordinator_hostname);

        let channel = Channel::from_shared(self.config.coordinator_url.clone())?
            .tls_config(tls)?
            .connect()
            .await?;

        let mut reg_client = RegistrationServiceClient::new(channel);

        // Call Register RPC
        let request = tonic::Request::new(RegisterRequest {
            registration_token: token.to_string(),
            hostname: hostname::get()?.to_string_lossy().to_string(),
            agent_type: self.config.agent_type.clone(),
            labels: self.config.labels.clone(),
            max_vms: self.config.max_vms as u32,
        });

        let response = reg_client.register(request).await?.into_inner();

        // Save the issued certificate
        self.cert_store.save(
            &response.client_cert_pem,
            &response.client_key_pem,
            &response.agent_id,
        )?;

        info!("Successfully registered as {}", response.agent_id);
        info!("Certificate stored at {:?}", self.cert_store.base_dir);

        // Now connect with the new certificate
        self.connect_with_mtls().await
    }

    /// Connect using stored mTLS certificate
    async fn connect_with_mtls(&self) -> Result<AgentServiceClient<Channel>> {
        let identity = self.cert_store.load_identity()?;
        let ca_cert = self.cert_store.load_ca()?;

        let tls = ClientTlsConfig::new()
            .ca_certificate(ca_cert)
            .identity(identity)  // Client certificate for mTLS
            .domain_name(&self.config.coordinator_hostname);

        let channel = Channel::from_shared(self.config.coordinator_url.clone())?
            .tls_config(tls)?
            .connect()
            .await?;

        Ok(AgentServiceClient::new(channel))
    }
}

/// Main agent run loop
pub async fn run_agent(config: AgentConfig) -> Result<()> {
    let cert_store = AgentCertStore::new(config.certs_dir.clone());
    let mut connector = AgentConnector { config, cert_store };

    loop {
        match connector.connect().await {
            Ok(client) => {
                if let Err(e) = run_agent_stream(client).await {
                    error!("Agent stream error: {}", e);
                }
            }
            Err(e) => {
                error!("Connection failed: {}", e);
            }
        }

        // Reconnect with backoff
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

/// Run the bidirectional streaming RPC
async fn run_agent_stream(mut client: AgentServiceClient<Channel>) -> Result<()> {
    let (tx, rx) = mpsc::channel(32);

    // Start bidirectional stream
    let response = client
        .agent_stream(ReceiverStream::new(rx))
        .await?;

    let mut inbound = response.into_inner();

    // Send initial status
    tx.send(AgentMessage {
        payload: Some(agent_message::Payload::Status(AgentStatus {
            active_vms: 0,
            available_slots: 2,
        })),
    }).await?;

    // Process messages from coordinator
    while let Some(msg) = inbound.message().await? {
        match msg.payload {
            Some(coordinator_message::Payload::CreateRunner(req)) => {
                // Handle create runner request
                let result = handle_create_runner(&req).await;
                tx.send(result.into()).await?;
            }
            Some(coordinator_message::Payload::DestroyRunner(req)) => {
                // Handle destroy runner request
                let result = handle_destroy_runner(&req).await;
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
```

### gRPC Protocol Definition

```protobuf
// agent-proto/proto/agent.proto

syntax = "proto3";

package cirunner.agent;

// ============================================================================
// Registration Service
// - Uses server-side TLS (encrypted) but no client certificate required
// - Registration tokens are SINGLE-USE: consumed immediately on success
// - Token validity window is short (default 1 hour)
// - A leaked token can only be used once before it becomes invalid
// ============================================================================

service RegistrationService {
    // Exchange registration token for client certificate
    // Token is invalidated after successful registration
    rpc Register(RegisterRequest) returns (RegisterResponse);
}

message RegisterRequest {
    string registration_token = 1;
    string hostname = 2;
    string agent_type = 3;  // "tart" or "proxmox"
    repeated string labels = 4;
    uint32 max_vms = 5;
}

message RegisterResponse {
    string agent_id = 1;
    string client_cert_pem = 2;  // X.509 certificate signed by coordinator CA
    string client_key_pem = 3;   // Private key (transmitted over TLS, returned only once)
    string expires_at = 4;       // Certificate expiry (ISO 8601)
}

// ============================================================================
// Agent Service (requires mTLS - agent identity from certificate)
// ============================================================================

service AgentService {
    // Bidirectional streaming for agent-coordinator communication
    rpc AgentStream(stream AgentMessage) returns (stream CoordinatorMessage);
}

// Messages FROM agent TO coordinator
message AgentMessage {
    oneof payload {
        AgentStatus status = 1;
        CommandResult result = 2;
        Pong pong = 3;
    }
}

message AgentStatus {
    uint32 active_vms = 1;
    uint32 available_slots = 2;
    repeated VmInfo vms = 3;
}

message VmInfo {
    string vm_id = 1;
    string name = 2;
    string state = 3;  // "creating", "running", "stopping"
    string ip_address = 4;
}

message CommandResult {
    string command_id = 1;
    bool success = 2;
    string error = 3;
    oneof result {
        CreateRunnerResult create_runner = 4;
        DestroyRunnerResult destroy_runner = 5;
    }
}

message CreateRunnerResult {
    string vm_id = 1;
    string ip_address = 2;
}

message DestroyRunnerResult {
    // Empty - just indicates success
}

message Pong {}

// Messages FROM coordinator TO agent
message CoordinatorMessage {
    oneof payload {
        CreateRunnerCommand create_runner = 1;
        DestroyRunnerCommand destroy_runner = 2;
        Ping ping = 3;
    }
}

message CreateRunnerCommand {
    string command_id = 1;
    string vm_name = 2;
    string template = 3;
    string registration_token = 4;  // GitHub runner registration token
    repeated string labels = 5;
    string runner_scope_url = 6;    // GitHub org/repo URL
}

message DestroyRunnerCommand {
    string command_id = 1;
    string vm_id = 2;
}

message Ping {}
```

### Crate Structure for Proto

```
agent-proto/
├── Cargo.toml
├── build.rs           # tonic-build
└── proto/
    └── agent.proto

# Cargo.toml
[dependencies]
prost = "0.12"
tonic = { version = "0.11", features = ["tls"] }

[build-dependencies]
tonic-build = "0.11"

# build.rs
fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::compile_protos("proto/agent.proto")?;
    Ok(())
}
```

## GitHub API Client

Shared GitHub API client used by all providers (lives in coordinator):

### Authentication: GitHub App

```rust
pub struct GitHubClient {
    app_id: String,
    private_key: String,
    installation_id: String,
    http_client: reqwest::Client,
    cached_token: RwLock<Option<CachedToken>>,
}

struct CachedToken {
    token: String,
    expires_at: DateTime<Utc>,
}

impl GitHubClient {
    /// Generate JWT for GitHub App authentication
    fn generate_jwt(&self) -> Result<String>;
    
    /// Get or refresh installation access token
    pub async fn get_access_token(&self) -> Result<String>;
    
    /// Generate runner registration token
    pub async fn get_registration_token(&self, scope: &RunnerScope) -> Result<String>;
    
    /// Get runner download URL for specific OS/arch
    pub async fn get_runner_download_url(
        &self, 
        scope: &RunnerScope,
        os: OsType,
        arch: Architecture,
    ) -> Result<String>;
}

pub enum RunnerScope {
    Organization(String),
    Repository { owner: String, repo: String },
}

pub enum Architecture {
    X64,
    Arm64,
}
```

### REST API Endpoints

**Base URL**: `https://api.github.com`

| Endpoint | Purpose |
|----------|---------|
| `POST /app/installations/{id}/access_tokens` | Get installation token from JWT |
| `POST /orgs/{org}/actions/runners/registration-token` | Get registration token (org) |
| `POST /repos/{owner}/{repo}/actions/runners/registration-token` | Get registration token (repo) |
| `GET /orgs/{org}/actions/runners/downloads` | Get runner download URLs |

**Note**: No deregistration endpoints needed - `--ephemeral` flag handles cleanup

## Fleet Management

The fleet manager handles both provider types with awareness of distributed Tart hosts:

```rust
pub struct FleetManager {
    providers: Vec<Arc<dyn VmProvider>>,
    github_client: Arc<GitHubClient>,
    config: FleetConfig,
}

pub struct FleetConfig {
    pub provider_configs: Vec<ProviderFleetConfig>,
}

pub struct ProviderFleetConfig {
    pub provider_name: String,
    pub concurrency: usize,       // Total VMs across all hosts
    pub template: String,
    pub runner_labels: Vec<String>,
    pub runner_group: Option<String>,
    pub runner_scope: RunnerScope,
}

impl FleetManager {
    pub async fn run(&self) -> Result<()> {
        let mut tasks = vec![];
        
        for (provider, config) in self.providers.iter().zip(&self.config.provider_configs) {
            for task_id in 0..config.concurrency {
                let provider = Arc::clone(provider);
                let github = Arc::clone(&self.github_client);
                let config = config.clone();
                
                let task = tokio::spawn(async move {
                    run_runner_loop(provider, github, config, task_id).await
                });
                tasks.push(task);
            }
        }
        
        futures::future::join_all(tasks).await;
        Ok(())
    }
}
```

### Runner Loop

Each fleet task runs this loop continuously:

```rust
async fn run_runner_loop(
    provider: Arc<dyn VmProvider>,
    github: Arc<GitHubClient>,
    config: ProviderFleetConfig,
    task_id: usize,
) {
    loop {
        let vm_name = format!("{}-{}-{}", config.template, task_id, timestamp());
        
        let result = run_single_runner(
            &provider,
            &github,
            &config,
            &vm_name,
        ).await;
        
        match result {
            Ok(exit_code) => {
                info!("Runner completed with exit code: {}", exit_code);
            }
            Err(e) => {
                error!("Runner failed: {}", e);
                tokio::time::sleep(Duration::from_secs(10)).await;
            }
        }
    }
}

async fn run_single_runner(
    provider: &dyn VmProvider,
    github: &GitHubClient,
    config: &ProviderFleetConfig,
    vm_name: &str,
) -> Result<()> {
    // 1. Get registration token from GitHub
    let runner_token = github.get_registration_token(&config.runner_scope).await?;

    // 2. Request agent to create VM and configure runner
    //    Agent handles: VM creation, boot, SSH, runner installation
    let vm_config = VmConfig {
        name: vm_name.to_string(),
        template: config.template.clone(),
        labels: config.runner_labels.clone(),
    };

    let runner_scope_url = config.runner_scope.to_url();
    let vm = provider.create_runner(&vm_config, &runner_token, &runner_scope_url).await?;

    // VM is now running with configured GitHub runner
    // Agent will notify coordinator when runner completes (via gRPC stream)
    // Agent handles VM cleanup after job completion

    info!(
        vm_id = %vm.id,
        host = ?vm.host,
        "Runner created successfully"
    );

    Ok(())
}
```

## Configuration

### Coordinator Config (TOML)

```toml
[github]
app_id = "123456"
private_key_path = "/etc/ci-runner/github-app.pem"
installation_id = "12345678"

# Runner configurations by label
# Agents self-register and provide their labels
# Fleet manager matches runner requests to available agents by label

[[runners]]
labels = ["self-hosted", "macOS", "ARM64"]
runner_scope = { type = "organization", name = "my-org" }
template = "macos-runner"

[[runners]]
labels = ["self-hosted", "Windows", "X64"]
runner_scope = { type = "organization", name = "my-org" }
template = "windows-runner"

[[runners]]
labels = ["self-hosted", "Linux", "X64"]
runner_scope = { type = "organization", name = "my-org" }
template = "linux-runner"

# gRPC server settings
[grpc]
listen_addr = "0.0.0.0:9443"

# TLS/mTLS certificates (generated with: coordinator ca init)
[tls]
ca_cert = "/etc/ci-runner/ca.crt"
ca_key = "/etc/ci-runner/ca.key"        # For signing agent certificates
server_cert = "/etc/ci-runner/server.crt"
server_key = "/etc/ci-runner/server.key"

# Note: No agent list needed - agents self-register using registration tokens
# Capacity is determined by connected agents at runtime
#
# To add a new agent:
#   1. Export CA cert: coordinator ca export > ca.crt (give to agent)
#   2. Generate registration token: coordinator token create --labels "macos,arm64"
#   3. Configure agent with token and CA cert
#   4. Start agent - it will exchange token for client certificate
```

### Agent Configs

See individual provider plans for agent configuration:
- **TartProviderPlan.md** - tart-agent config (macOS)
- **ProxmoxProviderPlan.md** - proxmox-agent config (Windows/Linux)

## Security Considerations

### Registration Token Security
Registration tokens are the initial bootstrap mechanism. Security properties:
- **Single-use**: Token is consumed (invalidated) immediately upon successful registration
- **Short-lived**: Default expiry of 1 hour; cannot be used after expiry
- **Encrypted transport**: Register RPC uses TLS (server-authenticated), private key never sent in plaintext
- **Revocable**: Unused tokens can be revoked via `coordinator token revoke`
- **Auditable**: Token creation logged with timestamp and creator

**If a token leaks**: An attacker has a short window (until expiry or legitimate use) to register a rogue agent. Mitigations:
1. Keep token lifetime short (default 1h, can be shorter)
2. Register agents promptly after token generation
3. Monitor for unexpected agent registrations
4. Revoke agents that shouldn't exist via `coordinator agent revoke`

### Coordinator Security
- GitHub App private key stored securely
- CA private key protected (600 permissions, never exported)
- mTLS required for all agent connections (post-registration)
- Registration tokens short-lived and single-use

### Agent Security
- Agents handle all direct VM access (SSH, etc.)
- SSH keys stored on agents, not coordinator
- Rate limiting on VM creation
- Audit logging of all VM operations

### Runtime Security
- Ephemeral VMs: clean state for each job
- Agent certificates valid for 1 year, renewable
- Network isolation (consider VLAN per provider)
- Coordinator has no direct network access to VMs

## Error Handling & Recovery

### Per-VM Errors
| Error | Recovery |
|-------|----------|
| VM creation fails | Agent retries locally, then reports failure to coordinator |
| Runner setup fails | Agent destroys VM, coordinator retries with fresh token |
| Runner registration fails | Agent destroys VM, coordinator retries on different agent |
| Agent reports timeout | Coordinator requests cleanup, tries different agent |

### Fleet-Level Recovery
- Individual VM failures don't affect other VMs
- Crashed tasks are restarted automatically
- Graceful shutdown on SIGTERM/SIGINT
- Agent health checks with automatic failover

### Agent Failure Recovery
- Coordinator detects agent offline
- Redistributes work to remaining agents
- Periodic reconnection attempts
- Alert on prolonged agent failure

### Zombie VM Cleanup
- Tag VMs with creation timestamp
- Periodic cleanup job on each agent
- Destroy VMs older than max_age (e.g., 2 hours)
- Coordinator-initiated cleanup on agent reconnect

## Monitoring & Observability

### Coordinator Metrics (Prometheus)
```
# VM lifecycle
kf_vm_created_total{provider="tart", host="mac-mini-1"}
kf_vm_destroyed_total{provider="tart", host="mac-mini-1"}
kf_vm_creation_duration_seconds{provider="tart"}

# Job execution
kf_jobs_completed_total{provider="tart", status="success"}
kf_jobs_completed_total{provider="tart", status="failure"}
kf_job_duration_seconds{provider="tart"}

# Fleet status
kf_active_vms{provider="tart", host="mac-mini-1"}
kf_fleet_tasks{provider="tart", status="running"}

# Agent health
kf_agent_status{host="mac-mini-1", status="online"}
kf_agent_capacity{host="mac-mini-1"}
```

### Agent Metrics
```
# Local VM status
tart_agent_active_vms
tart_agent_available_slots
tart_agent_vm_creation_duration_seconds

# Host resources
tart_agent_host_cpu_usage
tart_agent_host_memory_usage
tart_agent_host_disk_usage
```

### Logging
- Structured JSON logging
- Per-VM correlation IDs
- Host attribution for Tart VMs
- Log levels: ERROR, WARN, INFO, DEBUG, TRACE

## Implementation Phases

### Phase 1: Core Infrastructure
- [ ] Project structure setup (workspace, crates)
- [ ] Configuration loading
- [ ] CA initialization and certificate generation
- [ ] GitHub App authentication

### Phase 2: gRPC + mTLS Setup
- [ ] Define agent-proto protobuf messages
- [ ] tonic server with mTLS
- [ ] Registration service (token → certificate)
- [ ] Agent service (bidirectional streaming)

### Phase 3: Agent Library
- [ ] Certificate storage (agent-lib)
- [ ] gRPC client with mTLS
- [ ] Registration flow
- [ ] Reconnection with backoff

### Phase 4: Tart Agent
- [ ] Tart CLI wrapper
- [ ] VM lifecycle management
- [ ] Integrate agent-lib
- [ ] End-to-end test with Tart

### Phase 5: Proxmox Agent
- [ ] Proxmox REST API client
- [ ] VM lifecycle management
- [ ] Integrate agent-lib
- [ ] End-to-end test with Proxmox

### Phase 6: Fleet Management
- [ ] Agent registry in coordinator
- [ ] Label-based agent matching
- [ ] Runner loop logic
- [ ] Graceful shutdown handling

### Phase 6: Production Readiness
- [ ] Metrics endpoints (coordinator + agents)
- [ ] Health checks
- [ ] Service files (systemd, launchd)
- [ ] Documentation
- [ ] mTLS certificate management (CA setup, agent cert issuance, renewal)
