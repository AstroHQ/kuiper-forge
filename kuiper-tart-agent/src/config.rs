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
    /// Host environment settings (optional)
    #[serde(default)]
    pub host: HostConfig,
    /// Logging settings (optional)
    #[serde(default)]
    pub logging: LoggingConfig,
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
    /// Path to server CA certificate file (optional - not needed with native roots)
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
    /// Runners to keep for this mapping in fixed-capacity mode. Setting it on any mapping means unset ones get none
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool: Option<u32>,
}

impl kuiper_agent_lib::labels::LabelMapping for ImageMapping {
    fn labels(&self) -> &[String] {
        &self.labels
    }

    fn pool(&self) -> Option<u32> {
        self.pool
    }

    fn id(&self) -> String {
        self.image.clone()
    }
}

/// Tart-specific configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TartConfig {
    /// Default base image for VMs (used when no image mapping matches)
    pub base_image: String,
    /// Max macOS guests at once, 1 up to `MACOS_GUEST_LIMIT`
    #[serde(default = "default_max_macos_vms", alias = "max_concurrent_vms")]
    pub max_macos_vms: u32,
    /// Max VMs of any OS at once
    #[serde(default = "default_max_total_vms")]
    pub max_total_vms: u32,
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

impl TartConfig {
    fn limit_errors(&self) -> Vec<&'static str> {
        let mut errors = Vec::new();
        if !(1..=MACOS_GUEST_LIMIT).contains(&self.max_macos_vms) {
            errors.push(
                "tart.max_macos_vms: must be 1 or 2, macOS won't run more than 2 macOS VMs at once",
            );
        }
        if self.max_total_vms == 0 {
            errors.push("tart.max_total_vms: must be at least 1");
        }
        errors
    }
}

fn default_runner_version() -> String {
    "latest".to_string()
}

/// SSH authentication configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct SshAuthConfig {
    /// SSH username (default: "admin")
    pub username: String,
    /// Authentication method: "password", "key", or "default" (try default keys)
    pub auth_method: String,
    /// Password for password authentication
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// Path to private key for key-based authentication
    #[serde(skip_serializing_if = "Option::is_none")]
    pub private_key: Option<PathBuf>,
    /// SSH connection timeout in seconds
    pub timeout_secs: u64,
}

impl Default for SshAuthConfig {
    fn default() -> Self {
        Self {
            username: "admin".to_string(),
            auth_method: "default".to_string(),
            password: None,
            private_key: None,
            timeout_secs: 30,
        }
    }
}

/// macOS only runs this many macOS guests per host (a license term, so a future macOS could change it)
pub const MACOS_GUEST_LIMIT: u32 = 2;

fn default_max_macos_vms() -> u32 {
    MACOS_GUEST_LIMIT
}

fn default_max_total_vms() -> u32 {
    5
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

/// Host environment configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HostConfig {
    /// Check for short DHCP lease time on startup.
    /// Options: "error" (exit if wrong), "warn" (log warning), "ignore" (skip check)
    #[serde(default = "default_dhcp_lease_check")]
    pub dhcp_lease_check: String,
}

fn default_dhcp_lease_check() -> String {
    "error".to_string()
}

impl Default for HostConfig {
    fn default() -> Self {
        Self {
            dhcp_lease_check: default_dhcp_lease_check(),
        }
    }
}

/// Logging configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LoggingConfig {
    /// Number of days to retain log files (agent and runner logs)
    #[serde(default = "default_log_retention_days")]
    pub retention_days: u32,
    /// Upload this agent's own log lines to the coordinator (shown in its admin UI). Runner logs are never sent
    #[serde(default = "default_true")]
    pub upload: bool,
}

fn default_true() -> bool {
    true
}

fn default_log_retention_days() -> u32 {
    14
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            retention_days: default_log_retention_days(),
            upload: true,
        }
    }
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
        let config = Self::read(path)?;

        // Validate required fields
        let mut errors = Vec::new();

        if config.agent.labels.is_empty() {
            errors
                .push("agent.labels: Labels to identify this agent (e.g., [\"macos\", \"arm64\"])");
        }

        if config.tart.base_image.is_empty() {
            errors.push("tart.base_image: Tart image to use for VMs (e.g., \"ghcr.io/cirruslabs/macos-sequoia-base:latest\")");
        }

        errors.extend(config.tart.limit_errors());

        if !errors.is_empty() {
            let error_msg = format!(
                "Configuration incomplete\n\nPlease edit {} and set:\n  - {}\n\nOr run `kuiper-tart-agent setup`, then start the agent:\n  kuiper-tart-agent",
                path.display(),
                errors.join("\n  - ")
            );
            return Err(Error::Config(error_msg));
        }

        Ok(config)
    }

    /// Read a config file without validating it, so `setup` can start from a half-filled one.
    pub fn read(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path).map_err(|e| {
            Error::Config(format!(
                "Failed to read config file {}: {}",
                path.display(),
                e
            ))
        })?;

        let mut config: Config = toml::from_str(&content).map_err(|e| {
            Error::Config(format!(
                "Failed to parse config file {}: {}",
                path.display(),
                e
            ))
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

    /// Save configuration to a TOML file.
    pub fn save(&self, path: &Path) -> Result<()> {
        // Create parent directory if needed
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::Config(format!("Failed to create config directory: {e}")))?;
        }

        let content = toml::to_string_pretty(self)
            .map_err(|e| Error::Config(format!("Failed to serialize config: {e}")))?;

        write_private(path, content.as_bytes())
            .map_err(|e| Error::Config(format!("Failed to write config file: {e}")))?;

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

/// Write `content` readable by the owner only, since the config can hold the VM SSH password.
fn write_private(path: &Path, content: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;

    // `mode` only applies when the file is created, so tighten an existing one before the secret goes in
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(content)
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
        assert_eq!(
            config.coordinator.url,
            "https://coordinator.example.com:9443"
        );
        assert_eq!(config.tart.max_macos_vms, 2);
        assert_eq!(config.tart.max_total_vms, 5);
        assert_eq!(config.cleanup.max_vm_age_hours, 2);
    }

    #[test]
    fn test_write_private_tightens_existing_file() {
        use std::os::unix::fs::PermissionsExt;

        let path = std::env::temp_dir().join(format!("kta-config-{}.toml", uuid::Uuid::new_v4()));
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        write_private(&path, b"new").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        let content = std::fs::read_to_string(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        assert_eq!(mode, 0o600);
        assert_eq!(content, "new");
    }

    #[test]
    fn test_old_max_concurrent_vms_key() {
        let toml = r#"
base_image = "macos-runner"
max_concurrent_vms = 1
"#;
        let tart: TartConfig = toml::from_str(toml).expect("Failed to parse config");
        assert_eq!(tart.max_macos_vms, 1);
        assert_eq!(tart.max_total_vms, 5);
    }

    #[test]
    fn test_limit_validation() {
        let tart = |macos, total| {
            toml::from_str::<TartConfig>(&format!(
                "base_image = \"x\"\nmax_macos_vms = {macos}\nmax_total_vms = {total}"
            ))
            .unwrap()
        };
        assert!(tart(1, 5).limit_errors().is_empty());
        assert!(tart(2, 1).limit_errors().is_empty());
        assert_eq!(tart(0, 5).limit_errors().len(), 1);
        assert_eq!(tart(3, 5).limit_errors().len(), 1);
        assert_eq!(tart(2, 0).limit_errors().len(), 1);
    }
}
