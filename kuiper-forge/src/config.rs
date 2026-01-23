//! Configuration loading for the coordinator daemon.
//!
//! Loads configuration from TOML files and/or environment variables using figment.
//! This makes the coordinator container-friendly by supporting both config files
//! and environment variable overrides.
//!
//! # Configuration Sources (in order of priority, lowest to highest)
//!
//! 1. Default values (from `#[serde(default)]` attributes)
//! 2. TOML config file (if provided)
//! 3. Environment variables (prefix: `KUIPER_`, nested with `__`)
//!
//! # Environment Variable Naming
//!
//! Environment variables use the `KUIPER_` prefix with double-underscore for nesting:
//!
//! - `KUIPER_GITHUB__APP_ID` → `github.app_id`
//! - `KUIPER_GITHUB__PRIVATE_KEY_PATH` → `github.private_key_path`
//! - `KUIPER_GRPC__LISTEN_ADDR` → `grpc.listen_addr`
//! - `KUIPER_TLS__CA_CERT` → `tls.ca_cert`
//! - `KUIPER_PROVISIONING__MODE` → `provisioning.mode`
//! - `KUIPER_PROVISIONING__WEBHOOK__SECRET` → `provisioning.webhook.secret`
//!
//! **Note:** Complex arrays like `runners` and `label_mappings` should be configured
//! via TOML file, not environment variables.
//!
//! # Provisioning Modes
//!
//! The coordinator supports two provisioning modes:
//!
//! - **Fixed Capacity**: Maintains a fixed pool of runners per configuration.
//!   The coordinator ensures the target count of runners is always available.
//!
//! - **Webhook (Dynamic)**: Runners are created on-demand in response to GitHub
//!   webhook events. Labels from the workflow job determine which agent/VM base
//!   to use.

use anyhow::{Context, Result};
use figment::{
    providers::{Env, Format, Toml},
    Figment,
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Provisioning mode for runner management.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProvisioningMode {
    /// Fixed capacity mode: maintain a constant pool of runners.
    ///
    /// The coordinator ensures that `count` runners are always available
    /// for each runner configuration. When runners complete, new ones are
    /// automatically created to maintain the target count.
    #[default]
    FixedCapacity,

    /// Webhook-driven dynamic mode: create runners on-demand.
    ///
    /// Runners are created in response to GitHub webhook events (workflow_job).
    /// The job's labels determine which agent/VM base to use. No runners are
    /// pre-provisioned; they are created just-in-time when jobs are queued.
    Webhook,
}

/// Provisioning configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProvisioningConfig {
    /// The provisioning mode to use.
    #[serde(default)]
    pub mode: ProvisioningMode,

    /// Webhook configuration (only used when mode is `webhook`).
    #[serde(default)]
    pub webhook: Option<WebhookConfig>,
}

impl Default for ProvisioningConfig {
    fn default() -> Self {
        Self {
            mode: ProvisioningMode::FixedCapacity,
            webhook: None,
        }
    }
}

/// Webhook configuration for dynamic runner provisioning.
///
/// This is used when `provisioning.mode = "webhook"`. The coordinator will
/// listen for GitHub webhook events and create runners on-demand.
///
/// Webhooks are served on the same port as the gRPC server (configured in `[grpc]`).
/// The server multiplexes based on content-type:
/// - `application/grpc` → gRPC services (registration, agent streaming)
/// - `application/json` → Webhook endpoint
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct WebhookConfig {
    /// The path to listen for webhook events (default: "/webhook").
    ///
    /// This is served on the same port as the gRPC server.
    #[serde(default = "default_webhook_path")]
    pub path: String,

    /// The webhook secret for validating GitHub webhook signatures.
    ///
    /// GitHub signs webhook payloads with HMAC-SHA256 using this secret.
    /// The coordinator validates the `X-Hub-Signature-256` header.
    pub secret: String,

    /// Label mappings: maps workflow labels to runner scopes.
    ///
    /// When a webhook event arrives, the coordinator looks up the job's labels
    /// to determine which runner scope to use for registration.
    #[serde(default)]
    pub label_mappings: Vec<LabelMapping>,
}

fn default_webhook_path() -> String {
    "/webhook".to_string()
}

/// Maps a set of labels to a runner scope for webhook-driven provisioning.
///
/// When a `workflow_job` webhook event arrives, the coordinator checks each
/// label mapping to find a match. A job matches if ALL labels in this mapping
/// are present in the job's requested labels.
///
/// **Note:** GitHub's `workflow_job` webhook fires for ALL jobs, not just
/// self-hosted. The coordinator automatically filters by checking if the job's
/// labels match any configured mapping. Jobs requesting GitHub-hosted runners
/// (e.g., `ubuntu-latest`) won't match unless you explicitly configure them.
///
/// # Example
///
/// A mapping with `labels = ["self-hosted", "macOS", "ARM64"]` will match:
/// - `runs-on: [self-hosted, macOS, ARM64]` ✓
/// - `runs-on: [self-hosted, macOS, ARM64, gpu]` ✓ (extra labels OK)
/// - `runs-on: [self-hosted, macOS]` ✗ (missing ARM64)
/// - `runs-on: ubuntu-latest` ✗ (no match)
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LabelMapping {
    /// Labels that must ALL be present in the job's requested labels.
    ///
    /// The job may have additional labels beyond these; this is a subset match.
    pub labels: Vec<String>,

    /// The runner scope to use for jobs matching these labels.
    ///
    /// If not specified, defaults to the organization that triggered the webhook.
    #[serde(default)]
    pub runner_scope: Option<RunnerScope>,

    /// Optional runner group for the created runner.
    #[serde(default)]
    pub runner_group: Option<String>,
}

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

    /// Database configuration.
    #[serde(default)]
    pub database: DatabaseConfig,

    /// Provisioning mode and settings.
    ///
    /// Controls how runners are created:
    /// - `fixed_capacity` (default): Maintain a constant pool of runners
    /// - `webhook`: Create runners on-demand via GitHub webhooks
    #[serde(default)]
    pub provisioning: ProvisioningConfig,

    /// Runner configurations (used in fixed_capacity mode).
    ///
    /// Each configuration defines a pool of runners with specific labels
    /// and a target count to maintain.
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

// =============================================================================
// Database Configuration (compile-time feature selection)
// =============================================================================

/// SQLite database configuration (used when compiled with `sqlite` feature).
#[cfg(feature = "sqlite")]
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DatabaseConfig {
    /// Path to the SQLite database file.
    /// If not specified, defaults to `coordinator.db` in the data directory.
    #[serde(default)]
    pub path: Option<PathBuf>,
}

#[cfg(feature = "sqlite")]
impl Default for DatabaseConfig {
    fn default() -> Self {
        Self { path: None }
    }
}

/// PostgreSQL database configuration (used when compiled with `postgres` feature).
#[cfg(feature = "postgres")]
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DatabaseConfig {
    /// Database host (default: "localhost")
    #[serde(default = "default_postgres_host")]
    pub host: String,

    /// Database port (default: 5432)
    #[serde(default = "default_postgres_port")]
    pub port: u16,

    /// Database user
    #[serde(default)]
    pub user: String,

    /// Database password
    #[serde(default)]
    pub password: String,

    /// Database name (default: "kuiper_forge")
    #[serde(default = "default_postgres_database")]
    pub database: String,
}

#[cfg(feature = "postgres")]
fn default_postgres_host() -> String {
    "localhost".to_string()
}

#[cfg(feature = "postgres")]
fn default_postgres_port() -> u16 {
    5432
}

#[cfg(feature = "postgres")]
fn default_postgres_database() -> String {
    "kuiper_forge".to_string()
}

#[cfg(feature = "postgres")]
impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            host: default_postgres_host(),
            port: default_postgres_port(),
            user: String::new(),
            password: String::new(),
            database: default_postgres_database(),
        }
    }
}

/// Runner configuration for a specific set of labels.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RunnerConfig {
    /// Labels that agents must have to run these jobs (also used for GitHub runner registration)
    pub labels: Vec<String>,

    /// GitHub runner scope (organization or repository)
    pub runner_scope: RunnerScope,

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
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
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

    /// Get the API path for listing runners
    pub fn runners_list_path(&self) -> String {
        match self {
            RunnerScope::Organization { name } => {
                format!("/orgs/{}/actions/runners", name)
            }
            RunnerScope::Repository { owner, repo } => {
                format!("/repos/{}/{}/actions/runners", owner, repo)
            }
        }
    }

    /// Get the API path for deleting a specific runner
    pub fn runner_delete_path(&self, runner_id: u64) -> String {
        match self {
            RunnerScope::Organization { name } => {
                format!("/orgs/{}/actions/runners/{}", name, runner_id)
            }
            RunnerScope::Repository { owner, repo } => {
                format!("/repos/{}/{}/actions/runners/{}", owner, repo, runner_id)
            }
        }
    }
}

impl Config {
    /// Load configuration from TOML file and environment variables.
    ///
    /// Configuration sources are merged in order (later sources override earlier):
    /// 1. TOML config file (if it exists)
    /// 2. Environment variables (prefix: `KUIPER_`, nested with `__`)
    ///
    /// # Example
    ///
    /// ```bash
    /// # Override listen address via environment variable
    /// export KUIPER_GRPC__LISTEN_ADDR=0.0.0.0:8080
    /// ```
    pub fn load(path: &Path) -> Result<Self> {
        let mut figment = Figment::new();

        // Add TOML file if it exists
        if path.exists() {
            figment = figment.merge(Toml::file(path));
        }

        // Add environment variables (always, to allow overrides)
        figment = figment.merge(Env::prefixed("KUIPER_").split("__"));

        let config: Config = figment
            .extract()
            .with_context(|| format!("Failed to load config from {} and environment", path.display()))?;

        Ok(config)
    }

    /// Load configuration or create a minimal default for dry-run mode.
    ///
    /// In dry-run mode, we don't need GitHub credentials, so we can create
    /// a minimal config that just has TLS paths.
    pub fn load_or_default(path: &Path, data_dir: &Path) -> Result<Self> {
        if path.exists() {
            // Try to load the existing config (with env var overrides)
            match Self::load(path) {
                Ok(config) => return Ok(config),
                Err(e) => {
                    tracing::warn!("Failed to load config (using defaults for dry-run): {}", e);
                }
            }
        }

        // Try loading from environment variables only
        let figment = Figment::new().merge(Env::prefixed("KUIPER_").split("__"));
        if let Ok(config) = figment.extract::<Config>() {
            tracing::info!("Loaded configuration from environment variables");
            return Ok(config);
        }

        // Create minimal config for dry-run mode with test runners
        tracing::info!("Using default configuration for dry-run mode");
        tracing::info!("Adding default test runners for common label combinations");
        Ok(Config {
            github: GitHubConfig {
                app_id: "dry-run".to_string(),
                private_key_path: PathBuf::from("/dev/null"),
            },
            grpc: GrpcConfig::default(),
            tls: TlsConfig::with_defaults(data_dir),
            database: DatabaseConfig::default(),
            provisioning: ProvisioningConfig::default(),
            // Default test runners for dry-run mode
            runners: vec![
                RunnerConfig {
                    labels: vec!["self-hosted".into(), "macOS".into(), "ARM64".into()],
                    runner_scope: RunnerScope::Organization { name: "test-org".into() },
                    count: 1,
                    runner_group: None,
                },
                RunnerConfig {
                    labels: vec!["self-hosted".into(), "Linux".into(), "X64".into()],
                    runner_scope: RunnerScope::Organization { name: "test-org".into() },
                    count: 1,
                    runner_group: None,
                },
                RunnerConfig {
                    labels: vec!["self-hosted".into(), "Windows".into(), "X64".into()],
                    runner_scope: RunnerScope::Organization { name: "test-org".into() },
                    count: 1,
                    runner_group: None,
                },
            ],
        })
    }

    /// Get the default config file path
    /// - macOS: ~/Library/Application Support/kuiper-forge/config.toml
    /// - Linux: ~/.config/kuiper-forge/config.toml
    pub fn default_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("kuiper-forge")
            .join("config.toml")
    }

    /// Get the default data directory (for certs, auth store, etc.)
    /// - macOS: ~/Library/Application Support/kuiper-forge/
    /// - Linux: ~/.local/share/kuiper-forge/
    pub fn default_data_dir() -> PathBuf {
        dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("kuiper-forge")
    }
}

/// Create a default configuration template
pub fn default_config_template() -> String {
    let data_dir = Config::default_data_dir();
    let data_dir_str = data_dir.display();

    format!(
        r#"# KuiperForge Coordinator Configuration
# Data directory: {data_dir_str}

[github]
app_id = "123456"
private_key_path = "{data_dir_str}/github-app.pem"
# Note: installation_id is auto-discovered from your GitHub App installations

[grpc]
listen_addr = "0.0.0.0:9443"

[tls]
# Run `coordinator ca init` to generate these certificates
ca_cert = "{data_dir_str}/ca.crt"
ca_key = "{data_dir_str}/ca.key"
server_cert = "{data_dir_str}/server.crt"
server_key = "{data_dir_str}/server.key"

# =============================================================================
# Database Configuration
# =============================================================================
#
# The coordinator stores authentication data (tokens, agents) in a database.
# The database backend is selected at compile time via cargo features:
#   - cargo build -p kuiper-forge --features sqlite (default)
#   - cargo build -p kuiper-forge --features postgres --no-default-features
#
# Configuration below depends on which feature was enabled at compile time.

# SQLite configuration (when compiled with --features sqlite)
[database]
# path = "{data_dir_str}/coordinator.db"  # Optional, defaults to data_dir/coordinator.db

# PostgreSQL configuration (when compiled with --features postgres)
# [database]
# host = "localhost"
# port = 5432
# user = "kuiper"
# password = "secret"
# database = "kuiper_forge"

# =============================================================================
# Provisioning Mode
# =============================================================================
#
# Choose how runners are provisioned:
#
# - "fixed_capacity" (default): Maintain a constant pool of runners.
#   Use the [[runners]] sections below to define pools with target counts.
#   The coordinator ensures the target number of runners is always available.
#
# - "webhook": Create runners on-demand via GitHub webhook events.
#   Runners are created just-in-time when workflow jobs are queued.
#   Use [provisioning.webhook] to configure the webhook listener.

[provisioning]
mode = "fixed_capacity"

# -----------------------------------------------------------------------------
# Webhook Mode Configuration (uncomment to use)
# -----------------------------------------------------------------------------
# Webhooks are served on the SAME port as gRPC (configured above in [grpc]).
# The server automatically routes based on content-type:
#   - application/grpc  → gRPC services
#   - application/json  → Webhook endpoint
#
# [provisioning]
# mode = "webhook"
#
# [provisioning.webhook]
# path = "/webhook"                    # Webhook endpoint path (default: /webhook)
# secret = "your-github-webhook-secret" # For validating X-Hub-Signature-256
#
# # Label mappings: map workflow labels to runner scopes
# [[provisioning.webhook.label_mappings]]
# labels = ["self-hosted", "macOS", "ARM64"]
# runner_group = "my-runner-group"  # optional
#
# [provisioning.webhook.label_mappings.runner_scope]
# type = "organization"
# name = "my-org"
#
# [[provisioning.webhook.label_mappings]]
# labels = ["self-hosted", "Linux", "X64"]
#
# [provisioning.webhook.label_mappings.runner_scope]
# type = "repository"
# owner = "my-org"
# repo = "my-repo"

# =============================================================================
# Fixed Capacity Mode: Runner Configurations
# =============================================================================
#
# These are used when provisioning.mode = "fixed_capacity"
# Agents self-register and provide their labels.
# Fleet manager matches runner requests to available agents by label.
# Agents use their own configured base images for VM creation.

[[runners]]
labels = ["self-hosted", "macOS", "ARM64"]
count = 1

[runners.runner_scope]
type = "organization"
name = "my-org"

[[runners]]
labels = ["self-hosted", "Windows", "X64"]
count = 1

[runners.runner_scope]
type = "organization"
name = "my-org"

[[runners]]
labels = ["self-hosted", "Linux", "X64"]
count = 1

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
    use figment::providers::Toml as TomlProvider;

    /// Helper to parse TOML config strings in tests
    fn parse_config(toml_str: &str) -> Config {
        Figment::new()
            .merge(TomlProvider::string(toml_str))
            .extract()
            .expect("Failed to parse test config")
    }

    #[test]
    fn test_parse_config() {
        let config_str = r#"
[github]
app_id = "123456"
private_key_path = "/etc/ci-runner/github-app.pem"

[grpc]
listen_addr = "0.0.0.0:9443"

[tls]
ca_cert = "/etc/ci-runner/ca.crt"
ca_key = "/etc/ci-runner/ca.key"
server_cert = "/etc/ci-runner/server.crt"
server_key = "/etc/ci-runner/server.key"

[[runners]]
labels = ["self-hosted", "macOS", "ARM64"]

[runners.runner_scope]
type = "organization"
name = "my-org"
"#;

        let config = parse_config(config_str);
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

    #[test]
    fn test_provisioning_mode_defaults_to_fixed_capacity() {
        let config_str = r#"
[github]
app_id = "123456"
private_key_path = "/etc/ci-runner/github-app.pem"

[tls]
ca_cert = "/etc/ci-runner/ca.crt"
ca_key = "/etc/ci-runner/ca.key"
server_cert = "/etc/ci-runner/server.crt"
server_key = "/etc/ci-runner/server.key"
"#;

        let config = parse_config(config_str);
        assert_eq!(config.provisioning.mode, ProvisioningMode::FixedCapacity);
    }

    #[test]
    fn test_provisioning_mode_fixed_capacity() {
        let config_str = r#"
[github]
app_id = "123456"
private_key_path = "/etc/ci-runner/github-app.pem"

[tls]
ca_cert = "/etc/ci-runner/ca.crt"
ca_key = "/etc/ci-runner/ca.key"
server_cert = "/etc/ci-runner/server.crt"
server_key = "/etc/ci-runner/server.key"

[provisioning]
mode = "fixed_capacity"
"#;

        let config = parse_config(config_str);
        assert_eq!(config.provisioning.mode, ProvisioningMode::FixedCapacity);
    }

    #[test]
    fn test_provisioning_mode_webhook() {
        let config_str = r#"
[github]
app_id = "123456"
private_key_path = "/etc/ci-runner/github-app.pem"

[tls]
ca_cert = "/etc/ci-runner/ca.crt"
ca_key = "/etc/ci-runner/ca.key"
server_cert = "/etc/ci-runner/server.crt"
server_key = "/etc/ci-runner/server.key"

[provisioning]
mode = "webhook"

[provisioning.webhook]
path = "/github/webhook"
secret = "test-secret"

[[provisioning.webhook.label_mappings]]
labels = ["self-hosted", "macOS"]

[provisioning.webhook.label_mappings.runner_scope]
type = "organization"
name = "my-org"
"#;

        let config = parse_config(config_str);
        assert_eq!(config.provisioning.mode, ProvisioningMode::Webhook);

        let webhook = config.provisioning.webhook.unwrap();
        assert_eq!(webhook.path, "/github/webhook");
        assert_eq!(webhook.secret, "test-secret");
        assert_eq!(webhook.label_mappings.len(), 1);
        assert_eq!(webhook.label_mappings[0].labels, vec!["self-hosted", "macOS"]);
    }

    #[test]
    fn test_webhook_config_default_path() {
        let config_str = r#"
[github]
app_id = "123456"
private_key_path = "/etc/ci-runner/github-app.pem"

[tls]
ca_cert = "/etc/ci-runner/ca.crt"
ca_key = "/etc/ci-runner/ca.key"
server_cert = "/etc/ci-runner/server.crt"
server_key = "/etc/ci-runner/server.key"

[provisioning]
mode = "webhook"

[provisioning.webhook]
secret = "test-secret"
"#;

        let config = parse_config(config_str);
        let webhook = config.provisioning.webhook.unwrap();
        assert_eq!(webhook.path, "/webhook"); // default path
    }
}
