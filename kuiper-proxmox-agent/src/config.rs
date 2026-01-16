//! Configuration loading for kuiper-proxmox-agent.
//!
//! Configuration is loaded from a TOML file. See docs/ProxmoxProviderPlan.md for
//! the full configuration schema.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Main configuration structure for the Proxmox agent.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub coordinator: CoordinatorConfig,
    pub tls: TlsConfig,
    pub agent: AgentConfig,
    pub proxmox: ProxmoxConfig,
    pub vm: VmConfig,
    pub ssh: SshConfig,
    /// Cleanup settings (optional)
    #[serde(default)]
    pub cleanup: CleanupConfig,
}

/// Coordinator connection configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CoordinatorConfig {
    /// gRPC endpoint URL (e.g., "https://coordinator.example.com:9443")
    pub url: String,
    /// Hostname for TLS verification
    pub hostname: String,
}

/// TLS certificate configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TlsConfig {
    /// Path to CA certificate from coordinator
    pub ca_cert: PathBuf,
    /// Directory for client certificates (auto-populated after registration)
    pub certs_dir: PathBuf,
}

/// Agent-specific configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AgentConfig {
    /// Labels this agent advertises to the coordinator
    pub labels: Vec<String>,
}

/// Proxmox API configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProxmoxConfig {
    /// Proxmox API URL (e.g., "https://proxmox.local:8006")
    pub api_url: String,
    /// Proxmox node name
    pub node: String,
    /// API token ID (e.g., "ci-runner@pve!runner")
    pub token_id: String,
    /// API token secret
    pub token_secret: String,
    /// Accept invalid TLS certificates (for self-signed Proxmox certs)
    #[serde(default)]
    pub accept_invalid_certs: bool,
}

/// VM configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VmConfig {
    /// Template VM ID to clone from
    pub template_vmid: u32,
    /// Storage pool for clones (e.g., "local-lvm")
    pub storage: String,
    /// Use linked clones (faster, requires LVM-thin/ZFS)
    #[serde(default = "default_linked_clone")]
    pub linked_clone: bool,
    /// Maximum concurrent VMs
    #[serde(default = "default_concurrent_vms")]
    pub concurrent_vms: u32,
    /// Timeout in seconds for VM to get IP address
    #[serde(default = "default_ip_timeout")]
    pub ip_timeout_secs: u64,
    /// Timeout in seconds for clone operation
    #[serde(default = "default_clone_timeout")]
    pub clone_timeout_secs: u64,
}

fn default_linked_clone() -> bool {
    true
}

fn default_concurrent_vms() -> u32 {
    4
}

fn default_ip_timeout() -> u64 {
    120
}

fn default_clone_timeout() -> u64 {
    300
}

/// SSH configuration for connecting to VMs.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SshConfig {
    /// Path to SSH private key
    pub private_key_path: PathBuf,
    /// SSH username
    #[serde(default = "default_ssh_username")]
    pub username: String,
    /// SSH port
    #[serde(default = "default_ssh_port")]
    pub port: u16,
    /// Connection timeout in seconds
    #[serde(default = "default_ssh_timeout")]
    pub timeout_secs: u64,
    /// Number of connection retries
    #[serde(default = "default_ssh_retries")]
    pub retries: u32,
}

fn default_ssh_username() -> String {
    "ci".to_string()
}

fn default_ssh_port() -> u16 {
    22
}

fn default_ssh_timeout() -> u64 {
    30
}

fn default_ssh_retries() -> u32 {
    10
}

/// Cleanup configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CleanupConfig {
    /// Maximum VM age in hours before forced cleanup
    #[serde(default = "default_max_vm_age_hours")]
    pub max_vm_age_hours: u32,
    /// How often to run cleanup (minutes)
    #[serde(default = "default_cleanup_interval_mins")]
    pub cleanup_interval_mins: u32,
}

fn default_max_vm_age_hours() -> u32 {
    2
}

fn default_cleanup_interval_mins() -> u32 {
    15
}

impl Default for CleanupConfig {
    fn default() -> Self {
        Self {
            max_vm_age_hours: default_max_vm_age_hours(),
            cleanup_interval_mins: default_cleanup_interval_mins(),
        }
    }
}

impl Config {
    /// Load configuration from a TOML file.
    pub fn load<P: AsRef<Path>>(path: P) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path.as_ref()).map_err(|e| {
            ConfigError::IoError(format!(
                "Failed to read config file {:?}: {}",
                path.as_ref(),
                e
            ))
        })?;

        let config: Config = toml::from_str(&content).map_err(|e| {
            ConfigError::ParseError(format!("Failed to parse config: {}", e))
        })?;

        config.validate()?;

        Ok(config)
    }

    /// Load configuration from the default location.
    ///
    /// Searches in order:
    /// 1. `./kuiper-proxmox-agent.toml` (current directory)
    /// 2. `~/.config/kuiper-proxmox-agent/config.toml` (user config)
    /// 3. `/etc/kuiper-proxmox-agent/config.toml` (system config, Linux only)
    pub fn load_default() -> Result<Self, ConfigError> {
        let candidates = Self::config_search_paths();

        for path in &candidates {
            if path.exists() {
                tracing::info!("Loading config from {:?}", path);
                return Self::load(path);
            }
        }

        Err(ConfigError::NotFound(format!(
            "No config file found. Searched: {:?}",
            candidates
        )))
    }

    /// Get the list of paths to search for config files.
    pub fn config_search_paths() -> Vec<PathBuf> {
        let mut paths = vec![PathBuf::from("kuiper-proxmox-agent.toml")];

        if let Some(config_dir) = dirs::config_dir() {
            paths.push(config_dir.join("kuiper-proxmox-agent").join("config.toml"));
        }

        #[cfg(target_os = "linux")]
        paths.push(PathBuf::from("/etc/kuiper-proxmox-agent/config.toml"));

        paths
    }

    /// Get the default data directory.
    ///
    /// - Linux: `~/.local/share/kuiper-proxmox-agent/`
    /// - macOS: `~/Library/Application Support/kuiper-proxmox-agent/`
    pub fn default_data_dir() -> PathBuf {
        dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("kuiper-proxmox-agent")
    }

    /// Validate the configuration.
    fn validate(&self) -> Result<(), ConfigError> {
        // Validate coordinator URL
        if !self.coordinator.url.starts_with("https://") {
            return Err(ConfigError::ValidationError(
                "Coordinator URL must use https://".to_string(),
            ));
        }

        // Validate template VMID
        if self.vm.template_vmid == 0 {
            return Err(ConfigError::ValidationError(
                "Template VMID must be greater than 0".to_string(),
            ));
        }

        // Validate concurrent VMs
        if self.vm.concurrent_vms == 0 {
            return Err(ConfigError::ValidationError(
                "concurrent_vms must be at least 1".to_string(),
            ));
        }

        Ok(())
    }
}

/// Configuration errors.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("Config file not found: {0}")]
    NotFound(String),

    #[error("IO error: {0}")]
    IoError(String),

    #[error("Parse error: {0}")]
    ParseError(String),

    #[error("Validation error: {0}")]
    ValidationError(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_minimal_config() {
        let toml = r#"
[coordinator]
url = "https://coordinator.example.com:9443"
hostname = "coordinator.example.com"

[tls]
ca_cert = "/etc/kuiper-proxmox-agent/certs/ca.crt"
certs_dir = "/etc/kuiper-proxmox-agent/certs"

[agent]
labels = ["self-hosted", "windows", "x64"]

[proxmox]
api_url = "https://proxmox.local:8006"
node = "pve"
token_id = "ci-runner@pve!runner"
token_secret = "secret"

[vm]
template_vmid = 9000
storage = "local-lvm"

[ssh]
private_key_path = "/etc/kuiper-proxmox-agent/id_ed25519"
"#;

        let config: Config = toml::from_str(toml).expect("Failed to parse config");
        assert_eq!(config.vm.template_vmid, 9000);
        assert!(config.vm.linked_clone); // default
        assert_eq!(config.vm.concurrent_vms, 4); // default
        assert_eq!(config.ssh.username, "ci"); // default
        assert_eq!(config.ssh.port, 22); // default
    }

    #[test]
    fn test_parse_full_config() {
        let toml = r#"
[coordinator]
url = "https://coordinator.example.com:9443"
hostname = "coordinator.example.com"

[tls]
ca_cert = "/etc/kuiper-proxmox-agent/certs/ca.crt"
certs_dir = "/etc/kuiper-proxmox-agent/certs"

[agent]
labels = ["self-hosted", "windows", "x64", "proxmox"]

[proxmox]
api_url = "https://proxmox.local:8006"
node = "pve"
token_id = "ci-runner@pve!runner"
token_secret = "secret"
accept_invalid_certs = true

[vm]
template_vmid = 9000
storage = "local-lvm"
linked_clone = false
concurrent_vms = 8
ip_timeout_secs = 180
clone_timeout_secs = 600

[ssh]
private_key_path = "/etc/kuiper-proxmox-agent/id_ed25519"
username = "administrator"
port = 2222
timeout_secs = 60
retries = 5
"#;

        let config: Config = toml::from_str(toml).expect("Failed to parse config");
        assert!(config.proxmox.accept_invalid_certs);
        assert!(!config.vm.linked_clone);
        assert_eq!(config.vm.concurrent_vms, 8);
        assert_eq!(config.ssh.username, "administrator");
        assert_eq!(config.ssh.port, 2222);
    }
}
