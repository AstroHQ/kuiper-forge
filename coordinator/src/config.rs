//! Configuration loading for the coordinator daemon.
//!
//! Loads TOML configuration including GitHub App credentials, TLS settings,
//! and runner configurations.


use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Main configuration for the coordinator daemon.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    /// GitHub App configuration
    pub github: GitHubConfig,

    /// gRPC server settings
    #[serde(default)]
    pub grpc: GrpcConfig,

    /// TLS certificate paths
    pub tls: TlsConfig,

    /// Runner configurations
    #[serde(default)]
    pub runners: Vec<RunnerConfig>,
}

/// GitHub App authentication configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GitHubConfig {
    /// GitHub App ID
    pub app_id: String,

    /// Path to the GitHub App private key PEM file
    pub private_key_path: PathBuf,

    /// GitHub App installation ID
    pub installation_id: String,
}

/// gRPC server configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GrpcConfig {
    /// Address to listen on
    #[serde(default = "default_listen_addr")]
    pub listen_addr: String,
}

impl Default for GrpcConfig {
    fn default() -> Self {
        Self {
            listen_addr: default_listen_addr(),
        }
    }
}

fn default_listen_addr() -> String {
    "0.0.0.0:9443".to_string()
}

/// TLS certificate configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TlsConfig {
    /// Path to the CA certificate
    pub ca_cert: PathBuf,

    /// Path to the CA private key (for signing agent certificates)
    pub ca_key: PathBuf,

    /// Path to the server certificate
    pub server_cert: PathBuf,

    /// Path to the server private key
    pub server_key: PathBuf,
}

impl TlsConfig {
    /// Create TLS config with default paths in a given directory
    #[allow(dead_code)] // Useful for programmatic config setup
    pub fn with_defaults(dir: &Path) -> Self {
        Self {
            ca_cert: dir.join("ca.crt"),
            ca_key: dir.join("ca.key"),
            server_cert: dir.join("server.crt"),
            server_key: dir.join("server.key"),
        }
    }
}

/// Runner configuration for a specific set of labels.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RunnerConfig {
    /// Labels that agents must have to run these jobs
    pub labels: Vec<String>,

    /// GitHub runner scope (organization or repository)
    pub runner_scope: RunnerScope,

    /// VM template to use
    pub template: String,

    /// Number of runners to keep ready (default: 1)
    #[serde(default = "default_runner_count")]
    pub count: u32,

    /// Optional runner group
    #[serde(default)]
    pub runner_group: Option<String>,
}

fn default_runner_count() -> u32 {
    1
}

/// GitHub runner registration scope.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum RunnerScope {
    /// Organization-level runner
    Organization { name: String },

    /// Repository-level runner
    Repository { owner: String, repo: String },
}

impl RunnerScope {
    /// Get the URL for this runner scope
    pub fn to_url(&self) -> String {
        match self {
            RunnerScope::Organization { name } => {
                format!("https://github.com/{}", name)
            }
            RunnerScope::Repository { owner, repo } => {
                format!("https://github.com/{}/{}", owner, repo)
            }
        }
    }

    /// Get the API path for getting registration token
    pub fn registration_token_path(&self) -> String {
        match self {
            RunnerScope::Organization { name } => {
                format!("/orgs/{}/actions/runners/registration-token", name)
            }
            RunnerScope::Repository { owner, repo } => {
                format!(
                    "/repos/{}/{}/actions/runners/registration-token",
                    owner, repo
                )
            }
        }
    }
}

impl Config {
    /// Load configuration from a TOML file
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read config file: {}", path.display()))?;

        let config: Config = toml::from_str(&content)
            .with_context(|| format!("Failed to parse config file: {}", path.display()))?;

        Ok(config)
    }

    /// Get the default config file path
    /// - macOS: ~/Library/Application Support/ci-runner-coordinator/config.toml
    /// - Linux: ~/.config/ci-runner-coordinator/config.toml
    pub fn default_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("ci-runner-coordinator")
            .join("config.toml")
    }

    /// Get the default data directory (for certs, auth store, etc.)
    /// - macOS: ~/Library/Application Support/ci-runner-coordinator/
    /// - Linux: ~/.local/share/ci-runner-coordinator/
    pub fn default_data_dir() -> PathBuf {
        dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("ci-runner-coordinator")
    }
}

/// Create a default configuration template
pub fn default_config_template() -> String {
    let data_dir = Config::default_data_dir();
    let data_dir_str = data_dir.display();

    format!(
        r#"# CI Runner Coordinator Configuration
# Data directory: {data_dir_str}

[github]
app_id = "123456"
private_key_path = "{data_dir_str}/github-app.pem"
installation_id = "12345678"

[grpc]
listen_addr = "0.0.0.0:9443"

[tls]
# Run `coordinator ca init` to generate these certificates
ca_cert = "{data_dir_str}/ca.crt"
ca_key = "{data_dir_str}/ca.key"
server_cert = "{data_dir_str}/server.crt"
server_key = "{data_dir_str}/server.key"

# Runner configurations by label
# Agents self-register and provide their labels
# Fleet manager matches runner requests to available agents by label

[[runners]]
labels = ["self-hosted", "macOS", "ARM64"]
template = "macos-runner"

[runners.runner_scope]
type = "organization"
name = "my-org"

[[runners]]
labels = ["self-hosted", "Windows", "X64"]
template = "windows-runner"

[runners.runner_scope]
type = "organization"
name = "my-org"

[[runners]]
labels = ["self-hosted", "Linux", "X64"]
template = "linux-runner"

[runners.runner_scope]
type = "repository"
owner = "my-org"
repo = "my-repo"
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_config() {
        let config_str = r#"
[github]
app_id = "123456"
private_key_path = "/etc/ci-runner/github-app.pem"
installation_id = "12345678"

[grpc]
listen_addr = "0.0.0.0:9443"

[tls]
ca_cert = "/etc/ci-runner/ca.crt"
ca_key = "/etc/ci-runner/ca.key"
server_cert = "/etc/ci-runner/server.crt"
server_key = "/etc/ci-runner/server.key"

[[runners]]
labels = ["self-hosted", "macOS", "ARM64"]
template = "macos-runner"

[runners.runner_scope]
type = "organization"
name = "my-org"
"#;

        let config: Config = toml::from_str(config_str).unwrap();
        assert_eq!(config.github.app_id, "123456");
        assert_eq!(config.grpc.listen_addr, "0.0.0.0:9443");
        assert_eq!(config.runners.len(), 1);
    }

    #[test]
    fn test_runner_scope_url() {
        let org_scope = RunnerScope::Organization {
            name: "my-org".to_string(),
        };
        assert_eq!(org_scope.to_url(), "https://github.com/my-org");

        let repo_scope = RunnerScope::Repository {
            owner: "my-org".to_string(),
            repo: "my-repo".to_string(),
        };
        assert_eq!(repo_scope.to_url(), "https://github.com/my-org/my-repo");
    }
}
