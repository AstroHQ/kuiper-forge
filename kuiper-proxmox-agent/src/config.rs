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
    /// Path to server CA certificate from coordinator (optional)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_cert: Option<PathBuf>,
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

/// A label-to-template mapping rule for selecting VM templates based on job labels.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TemplateMapping {
    /// Labels that must ALL be present in job labels for this mapping to match
    pub labels: Vec<String>,
    /// The Proxmox template VMID to use when this mapping matches
    pub template_vmid: u32,
}

/// VM configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VmConfig {
    /// Default template VM ID to clone from (used when no template mapping matches)
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
    /// Template mappings for label-based selection (first match wins)
    #[serde(default)]
    pub template_mappings: Vec<TemplateMapping>,
    /// GitHub Actions runner version to install (e.g., "2.321.0")
    #[serde(default = "default_runner_version")]
    pub runner_version: String,
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

fn default_runner_version() -> String {
    "latest".to_string()
}

/// SSH configuration for connecting to VMs.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SshConfig {
    /// Path to SSH private key (optional if password is provided)
    #[serde(default)]
    pub private_key_path: Option<PathBuf>,
    /// SSH password (optional if private_key_path is provided)
    #[serde(default)]
    pub password: Option<String>,
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

        let mut config: Config = toml::from_str(&content)
            .map_err(|e| ConfigError::ParseError(format!("Failed to parse config: {e}")))?;

        // Expand ~ in paths
        config.tls.ca_cert = config.tls.ca_cert.map(|p| expand_tilde(&p));
        config.tls.certs_dir = expand_tilde(&config.tls.certs_dir);
        config.ssh.private_key_path = config.ssh.private_key_path.map(|p| expand_tilde(&p));

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
            "No config file found. Searched: {candidates:?}"
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

    /// Get the default configuration file path.
    pub fn default_config_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("kuiper-proxmox-agent")
            .join("config.toml")
    }

    /// Generate a template configuration for initial registration.
    pub fn generate_template(
        coordinator_url: String,
        coordinator_hostname: String,
        certs_dir: PathBuf,
    ) -> Self {
        Self {
            coordinator: CoordinatorConfig {
                url: coordinator_url,
                hostname: coordinator_hostname,
            },
            tls: TlsConfig {
                ca_cert: Some(certs_dir.join("ca.crt")),
                certs_dir,
            },
            agent: AgentConfig { labels: vec![] },
            proxmox: ProxmoxConfig {
                api_url: String::new(), // User must set
                node: "pve".to_string(),
                token_id: String::new(),     // User must set
                token_secret: String::new(), // User must set
                accept_invalid_certs: false,
            },
            vm: VmConfig {
                template_vmid: 9000,
                storage: "local-lvm".to_string(),
                linked_clone: true,
                concurrent_vms: 5,
                ip_timeout_secs: default_ip_timeout(),
                clone_timeout_secs: default_clone_timeout(),
                template_mappings: vec![],
                runner_version: default_runner_version(),
            },
            ssh: SshConfig {
                private_key_path: None, // User must set
                password: None,
                username: "ci".to_string(),
                port: default_ssh_port(),
                timeout_secs: default_ssh_timeout(),
                retries: default_ssh_retries(),
            },
            cleanup: CleanupConfig::default(),
        }
    }

    /// Save configuration to a TOML file.
    pub fn save<P: AsRef<Path>>(&self, path: P) -> Result<(), ConfigError> {
        // Create parent directory if needed
        if let Some(parent) = path.as_ref().parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                ConfigError::IoError(format!("Failed to create config directory: {e}"))
            })?;
        }

        let content = toml::to_string_pretty(self)
            .map_err(|e| ConfigError::ParseError(format!("Failed to serialize config: {e}")))?;

        std::fs::write(path.as_ref(), content).map_err(|e| {
            ConfigError::IoError(format!(
                "Failed to write config file {:?}: {e}",
                path.as_ref()
            ))
        })?;

        Ok(())
    }

    /// Validate the configuration.
    fn validate(&self) -> Result<(), ConfigError> {
        let mut errors = Vec::new();

        // Check required fields
        if self.agent.labels.is_empty() {
            errors.push(
                "agent.labels: Labels to identify this agent (e.g., [\"proxmox\", \"x86_64\"])",
            );
        }

        if self.proxmox.api_url.is_empty() {
            errors
                .push("proxmox.api_url: Proxmox API URL (e.g., \"https://pve.example.com:8006\")");
        }

        if self.proxmox.token_id.is_empty() {
            errors.push("proxmox.token_id: API token ID (e.g., \"ci-runner@pve!runner\")");
        }

        if self.proxmox.token_secret.is_empty() {
            errors.push("proxmox.token_secret: API token secret");
        }

        // Check SSH authentication
        if self.ssh.private_key_path.is_none() && self.ssh.password.is_none() {
            errors.push("ssh.private_key_path or ssh.password: SSH authentication method required");
        }

        if !errors.is_empty() {
            return Err(ConfigError::ValidationError(format!(
                "Configuration incomplete\n\nPlease edit the config file and set:\n  - {}\n\nThen start the agent:\n  kuiper-proxmox-agent",
                errors.join("\n  - ")
            )));
        }

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

/// Expand ~ to the user's home directory.
fn expand_tilde(path: &Path) -> PathBuf {
    if let Some(path_str) = path.to_str()
        && path_str.starts_with("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(&path_str[2..]);
    }
    path.to_path_buf()
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

    #[test]
    fn test_parse_password_auth_config() {
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
password = "my-secret-password"
username = "vagrant"
"#;

        let config: Config = toml::from_str(toml).expect("Failed to parse config");
        config.validate().expect("Validation should pass");
        assert!(config.ssh.private_key_path.is_none());
        assert_eq!(config.ssh.password, Some("my-secret-password".to_string()));
        assert_eq!(config.ssh.username, "vagrant");
    }

    #[test]
    fn test_parse_both_auth_methods() {
        let toml = r#"
[coordinator]
url = "https://coordinator.example.com:9443"
hostname = "coordinator.example.com"

[tls]
ca_cert = "/etc/kuiper-proxmox-agent/certs/ca.crt"
certs_dir = "/etc/kuiper-proxmox-agent/certs"

[agent]
labels = ["self-hosted"]

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
password = "fallback-password"
"#;

        let config: Config = toml::from_str(toml).expect("Failed to parse config");
        config.validate().expect("Validation should pass");
        assert!(config.ssh.private_key_path.is_some());
        assert!(config.ssh.password.is_some());
    }

    #[test]
    fn test_validation_fails_without_ssh_auth() {
        let toml = r#"
[coordinator]
url = "https://coordinator.example.com:9443"
hostname = "coordinator.example.com"

[tls]
ca_cert = "/etc/kuiper-proxmox-agent/certs/ca.crt"
certs_dir = "/etc/kuiper-proxmox-agent/certs"

[agent]
labels = ["self-hosted"]

[proxmox]
api_url = "https://proxmox.local:8006"
node = "pve"
token_id = "ci-runner@pve!runner"
token_secret = "secret"

[vm]
template_vmid = 9000
storage = "local-lvm"

[ssh]
username = "vagrant"
"#;

        let config: Config = toml::from_str(toml).expect("Failed to parse config");
        let result = config.validate();
        assert!(result.is_err());
        let error_msg = result.unwrap_err().to_string();
        assert!(
            error_msg.contains("ssh.private_key_path or ssh.password"),
            "Expected error message about SSH auth, got: {error_msg}"
        );
    }
}
