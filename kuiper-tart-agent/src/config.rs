//! Configuration for the Tart agent.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// Main configuration structure loaded from TOML file.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    /// Coordinator connection settings
    pub coordinator: CoordinatorConfig,
    /// TLS/Certificate settings
    pub tls: TlsConfig,
    /// Agent settings
    pub agent: AgentConfig,
    /// Tart-specific settings
    pub tart: TartConfig,
    /// Cleanup settings (optional)
    #[serde(default)]
    pub cleanup: CleanupConfig,
    /// Reconnection settings (optional)
    #[serde(default)]
    pub reconnect: ReconnectConfig,
}

/// Coordinator connection configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CoordinatorConfig {
    /// gRPC endpoint URL (e.g., "https://coordinator.example.com:9443")
    pub url: String,
    /// Hostname for TLS verification
    pub hostname: String,
}

/// TLS/Certificate configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TlsConfig {
    /// Path to CA certificate file (optional - not needed with TOFU mode)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_cert: Option<PathBuf>,
    /// Directory for client certificates
    pub certs_dir: PathBuf,
}

/// Agent-specific configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AgentConfig {
    /// Labels this agent advertises to the coordinator
    #[serde(default)]
    pub labels: Vec<String>,
}

/// A label-to-image mapping rule for selecting VM images based on job labels.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ImageMapping {
    /// Labels that must ALL be present in job labels for this mapping to match
    pub labels: Vec<String>,
    /// The Tart image to use when this mapping matches
    pub image: String,
}

/// Tart-specific configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TartConfig {
    /// Default base image for VMs (used when no image mapping matches)
    pub base_image: String,
    /// Maximum concurrent VMs (Apple Virtualization Framework limit is 2)
    #[serde(default = "default_max_concurrent_vms")]
    pub max_concurrent_vms: u32,
    /// Shared cache directory for VMs
    pub shared_cache_dir: Option<PathBuf>,
    /// SSH configuration for connecting to VMs
    #[serde(default)]
    pub ssh: SshAuthConfig,
    /// GitHub Actions runner version to install (e.g., "2.321.0")
    /// See: https://github.com/actions/runner/releases
    #[serde(default = "default_runner_version")]
    pub runner_version: String,
    /// Image mappings for label-based selection (first match wins)
    #[serde(default)]
    pub image_mappings: Vec<ImageMapping>,
}

fn default_runner_version() -> String {
    "latest".to_string()
}

/// SSH authentication configuration.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct SshAuthConfig {
    /// SSH username (default: "admin")
    #[serde(default = "default_ssh_username")]
    pub username: String,
    /// Authentication method: "password", "key", or "default" (try default keys)
    #[serde(default = "default_ssh_auth_method")]
    pub auth_method: String,
    /// Password for password authentication
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// Path to private key for key-based authentication
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private_key: Option<PathBuf>,
    /// SSH connection timeout in seconds
    #[serde(default = "default_ssh_timeout")]
    pub timeout_secs: u64,
}

fn default_ssh_username() -> String {
    "admin".to_string()
}

fn default_ssh_auth_method() -> String {
    "default".to_string()
}

fn default_ssh_timeout() -> u64 {
    30
}

fn default_max_concurrent_vms() -> u32 {
    2
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

/// Reconnection configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ReconnectConfig {
    /// Initial delay before reconnecting (seconds)
    #[serde(default = "default_initial_delay")]
    pub initial_delay_secs: u64,
    /// Maximum delay between reconnection attempts (seconds)
    #[serde(default = "default_max_delay")]
    pub max_delay_secs: u64,
}

fn default_initial_delay() -> u64 {
    1
}

fn default_max_delay() -> u64 {
    60
}

impl Default for ReconnectConfig {
    fn default() -> Self {
        Self {
            initial_delay_secs: default_initial_delay(),
            max_delay_secs: default_max_delay(),
        }
    }
}

impl Config {
    /// Load configuration from a TOML file.
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path).map_err(|e| {
            Error::Config(format!("Failed to read config file {}: {}", path.display(), e))
        })?;

        let mut config: Config = toml::from_str(&content).map_err(|e| {
            Error::Config(format!("Failed to parse config file {}: {}", path.display(), e))
        })?;

        // Expand ~ in paths
        config.tls.ca_cert = config.tls.ca_cert.map(|p| expand_tilde(&p));
        config.tls.certs_dir = expand_tilde(&config.tls.certs_dir);
        if let Some(ref cache_dir) = config.tart.shared_cache_dir {
            config.tart.shared_cache_dir = Some(expand_tilde(cache_dir));
        }
        if let Some(ref key_path) = config.tart.ssh.private_key {
            config.tart.ssh.private_key = Some(expand_tilde(key_path));
        }

        Ok(config)
    }

    /// Create a config from bootstrap arguments.
    pub fn from_bootstrap(
        coordinator_url: String,
        ca_cert: Option<PathBuf>,
        labels: Vec<String>,
        base_image: String,
        max_vms: Option<u32>,
    ) -> Self {
        // Extract hostname from URL for TLS verification
        let hostname = url::Url::parse(&coordinator_url)
            .ok()
            .and_then(|u| u.host_str().map(String::from))
            .unwrap_or_else(|| "localhost".to_string());

        let data_dir = Self::default_data_dir();
        let certs_dir = data_dir.join("certs");

        Config {
            coordinator: CoordinatorConfig {
                url: coordinator_url,
                hostname,
            },
            tls: TlsConfig {
                ca_cert,
                certs_dir,
            },
            agent: AgentConfig { labels },
            tart: TartConfig {
                base_image,
                max_concurrent_vms: max_vms.unwrap_or(2),
                shared_cache_dir: None,
                ssh: SshAuthConfig::default(),
                runner_version: default_runner_version(),
                image_mappings: Vec::new(),
            },
            cleanup: CleanupConfig::default(),
            reconnect: ReconnectConfig::default(),
        }
    }

    /// Save configuration to a TOML file.
    pub fn save(&self, path: &Path) -> Result<()> {
        // Create parent directory if needed
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                Error::Config(format!("Failed to create config directory: {}", e))
            })?;
        }

        let content = toml::to_string_pretty(self).map_err(|e| {
            Error::Config(format!("Failed to serialize config: {}", e))
        })?;

        std::fs::write(path, content).map_err(|e| {
            Error::Config(format!("Failed to write config file: {}", e))
        })?;

        Ok(())
    }

    /// Get the default configuration file path.
    ///
    /// - macOS: `~/Library/Application Support/kuiper-tart-agent/config.toml`
    /// - Linux: `~/.config/kuiper-tart-agent/config.toml`
    pub fn default_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("kuiper-tart-agent")
            .join("config.toml")
    }

    /// Get the default data directory.
    ///
    /// - macOS: `~/Library/Application Support/kuiper-tart-agent/`
    /// - Linux: `~/.local/share/kuiper-tart-agent/`
    pub fn default_data_dir() -> PathBuf {
        dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("kuiper-tart-agent")
    }
}

/// Expand ~ to the user's home directory.
fn expand_tilde(path: &Path) -> PathBuf {
    if let Some(path_str) = path.to_str() {
        if path_str.starts_with("~/") {
            if let Some(home) = dirs::home_dir() {
                return home.join(&path_str[2..]);
            }
        }
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_expand_tilde() {
        let path = Path::new("~/foo/bar");
        let expanded = expand_tilde(path);
        assert!(!expanded.to_string_lossy().contains('~'));
    }

    #[test]
    fn test_parse_minimal_config() {
        let toml = r#"
[coordinator]
url = "https://coordinator.example.com:9443"
hostname = "coordinator.example.com"

[tls]
ca_cert = "~/certs/ca.crt"
certs_dir = "~/certs"

[agent]
labels = ["macos", "arm64"]

[tart]
base_image = "macos-runner"
"#;

        let config: Config = toml::from_str(toml).expect("Failed to parse config");
        assert_eq!(config.coordinator.url, "https://coordinator.example.com:9443");
        assert_eq!(config.tart.max_concurrent_vms, 2);
        assert_eq!(config.cleanup.max_vm_age_hours, 2);
    }
}
