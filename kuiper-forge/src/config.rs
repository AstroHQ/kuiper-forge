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
//! - `KUIPER_GITHUB__PRIVATE_KEY` → `github.private_key` (raw PEM content)
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
    Figment,
    providers::{Env, Format, Toml},
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

    /// Global runner scope - where all runners are registered in GitHub.
    ///
    /// In fixed capacity mode, agents report their labels and capacity,
    /// and all runners are registered to this scope.
    pub runner_scope: RunnerScope,

    /// Optional global runner group (GitHub enterprise feature).
    ///
    /// If specified, all runners will be registered to this runner group.
    #[serde(default)]
    pub runner_group: Option<String>,
}

/// GitHub App authentication configuration.
///
/// The private key can be provided either as a file path (`private_key_path`)
/// or as raw PEM content (`private_key`). The inline form is useful for
/// container deployments where environment variables are easier than file mounts:
///
/// ```bash
/// export KUIPER_GITHUB__PRIVATE_KEY="-----BEGIN RSA PRIVATE KEY-----\n...\n-----END RSA PRIVATE KEY-----"
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GitHubConfig {
    /// GitHub App ID
    pub app_id: u64,

    /// Path to the GitHub App private key PEM file.
    #[serde(default)]
    pub private_key_path: Option<PathBuf>,

    /// Raw PEM content of the GitHub App private key.
    /// Useful for container deployments via `KUIPER_GITHUB__PRIVATE_KEY` env var.
    /// Supports `\n` escape sequences for single-line env var values.
    #[serde(default)]
    pub private_key: Option<String>,
}

impl GitHubConfig {
    /// Resolve the private key content, reading from file if needed.
    pub fn private_key_content(&self) -> Result<String> {
        if let Some(ref key) = self.private_key {
            // Support \n escape sequences (common when passing PEM via env vars)
            Ok(key.replace("\\n", "\n"))
        } else if let Some(ref path) = self.private_key_path {
            std::fs::read_to_string(path).with_context(|| {
                format!("Failed to read GitHub App private key: {}", path.display())
            })
        } else {
            Err(anyhow::anyhow!(
                "Either github.private_key or github.private_key_path must be set"
            ))
        }
    }
}

/// gRPC server configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GrpcConfig {
    /// Address to listen on
    #[serde(default = "default_listen_addr")]
    pub listen_addr: String,

    /// Enable PROXY protocol support (v1 and v2).
    ///
    /// When enabled, the server expects incoming connections to begin with a
    /// PROXY protocol header (sent by load balancers like HAProxy, AWS NLB,
    /// DigitalOcean LB). The real client IP is extracted from this header.
    ///
    /// WARNING: Only enable this if ALL traffic comes through a proxy that
    /// sends PROXY protocol headers. Direct connections will fail.
    #[serde(default)]
    pub proxy_protocol: bool,
}

impl Default for GrpcConfig {
    fn default() -> Self {
        Self {
            listen_addr: default_listen_addr(),
            proxy_protocol: false,
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

    /// Optional CA certificate for verifying the coordinator server cert (agents)
    #[serde(default)]
    pub server_ca_cert: Option<PathBuf>,

    /// If true, agents should use native OS roots for server TLS validation
    #[serde(default)]
    pub server_use_native_roots: bool,

    /// Server trust mode for agent connections ("ca" or "chain")
    #[serde(default = "default_server_trust_mode")]
    pub server_trust_mode: ServerTrustMode,
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
            server_ca_cert: None,
            server_use_native_roots: false,
            server_trust_mode: ServerTrustMode::Ca,
        }
    }
}

/// Server trust mode for agent TLS verification.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServerTrustMode {
    /// Validate TLS chain + hostname using the configured CA bundle.
    Ca,
    /// Validate normal TLS chain + hostname only (no pin).
    Chain,
}

fn default_server_trust_mode() -> ServerTrustMode {
    ServerTrustMode::Ca
}

// =============================================================================
// Database Configuration (compile-time feature selection)
// =============================================================================

/// SQLite database configuration.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct SqliteDatabaseConfig {
    /// Path to the SQLite database file.
    /// If not specified, defaults to `coordinator.db` in the data directory.
    #[serde(default)]
    pub path: Option<PathBuf>,
}

/// PostgreSQL database configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct PostgresDatabaseConfig {
    /// Database host
    pub host: String,

    /// Database port
    pub port: u16,

    /// Database user
    pub user: String,

    /// Database password
    pub password: String,

    /// Database name
    pub database: String,
}

impl Default for PostgresDatabaseConfig {
    fn default() -> Self {
        Self {
            host: "localhost".to_string(),
            port: 5432,
            user: String::new(),
            password: String::new(),
            database: "kuiper_forge".to_string(),
        }
    }
}

// Type alias to select the active database configuration based on features
#[cfg(feature = "sqlite")]
pub type DatabaseConfig = SqliteDatabaseConfig;

#[cfg(all(feature = "postgres", not(feature = "sqlite")))]
pub type DatabaseConfig = PostgresDatabaseConfig;

// Compile-time assertions for database features
#[cfg(not(any(feature = "sqlite", feature = "postgres")))]
compile_error!("At least one database feature must be enabled: 'sqlite' or 'postgres'");

#[cfg(all(feature = "sqlite", feature = "postgres"))]
compile_error!("Cannot enable both 'sqlite' and 'postgres' features simultaneously. Choose one.");

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
                format!("https://github.com/{name}")
            }
            RunnerScope::Repository { owner, repo } => {
                format!("https://github.com/{owner}/{repo}")
            }
        }
    }

    /// Get the API path for getting registration token
    pub fn registration_token_path(&self) -> String {
        match self {
            RunnerScope::Organization { name } => {
                format!("/orgs/{name}/actions/runners/registration-token")
            }
            RunnerScope::Repository { owner, repo } => {
                format!("/repos/{owner}/{repo}/actions/runners/registration-token")
            }
        }
    }

    /// Get the API path for listing runners
    pub fn runners_list_path(&self) -> String {
        match self {
            RunnerScope::Organization { name } => {
                format!("/orgs/{name}/actions/runners")
            }
            RunnerScope::Repository { owner, repo } => {
                format!("/repos/{owner}/{repo}/actions/runners")
            }
        }
    }

    /// Get the API path for deleting a specific runner
    pub fn runner_delete_path(&self, runner_id: u64) -> String {
        match self {
            RunnerScope::Organization { name } => {
                format!("/orgs/{name}/actions/runners/{runner_id}")
            }
            RunnerScope::Repository { owner, repo } => {
                format!("/repos/{owner}/{repo}/actions/runners/{runner_id}")
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

        let config: Config = figment.extract().with_context(|| {
            format!(
                "Failed to load config from {} and environment",
                path.display()
            )
        })?;

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

        // Create minimal config for dry-run mode with global runner scope
        tracing::info!("Using default configuration for dry-run mode");
        tracing::info!("Using agent-driven pool discovery");
        Ok(Config {
            github: GitHubConfig {
                app_id: 0,
                private_key_path: None,
                private_key: None,
            },
            grpc: GrpcConfig::default(),
            tls: TlsConfig::with_defaults(data_dir),
            database: DatabaseConfig::default(),
            provisioning: ProvisioningConfig::default(),
            runner_scope: RunnerScope::Organization {
                name: "test-org".into(),
            },
            runner_group: None,
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
app_id = 123456
private_key_path = "{data_dir_str}/github-app.pem"
# Or provide the PEM content directly (useful for containers):
# private_key = "-----BEGIN RSA PRIVATE KEY-----\\n...\\n-----END RSA PRIVATE KEY-----"
# Environment variable: KUIPER_GITHUB__PRIVATE_KEY
# Note: installation_id is auto-discovered from your GitHub App installations

[grpc]
listen_addr = "0.0.0.0:9443"

[tls]
# Run `coordinator ca init` to generate these certificates
ca_cert = "{data_dir_str}/ca.crt"
ca_key = "{data_dir_str}/ca.key"
server_cert = "{data_dir_str}/server.crt"
server_key = "{data_dir_str}/server.key"
# Optional: CA bundle for agents to validate the coordinator server cert.
# If omitted, agents default to ca_cert unless server_use_native_roots = true.
# server_ca_cert = "{data_dir_str}/ca.crt"

# Use OS native roots for coordinator server cert validation (e.g., Let's Encrypt)
# server_use_native_roots = true

# Server trust mode for agents:
# - "ca": validate chain + hostname using the configured CA bundle (default)
# - "chain": validate chain + hostname only (public roots)
# server_trust_mode = "ca"

# =============================================================================
# Runner Scope
# =============================================================================
#
# Define where all runners should be registered in GitHub.
# Agents report their labels and capacity; pools are automatically created
# based on connected agents.

[runner_scope]
type = "organization"
name = "my-org"

# Optional: GitHub Enterprise runner group
# runner_group = "my-runner-group"

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
app_id = 123456
private_key_path = "/etc/ci-runner/github-app.pem"

[grpc]
listen_addr = "0.0.0.0:9443"

[tls]
ca_cert = "/etc/ci-runner/ca.crt"
ca_key = "/etc/ci-runner/ca.key"
server_cert = "/etc/ci-runner/server.crt"
server_key = "/etc/ci-runner/server.key"

[runner_scope]
type = "organization"
name = "my-org"
"#;

        let config = parse_config(config_str);
        assert_eq!(config.github.app_id, 123456);
        assert_eq!(config.grpc.listen_addr, "0.0.0.0:9443");
        assert_eq!(
            config.runner_scope,
            RunnerScope::Organization {
                name: "my-org".to_string()
            }
        );
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
app_id = 123456
private_key_path = "/etc/ci-runner/github-app.pem"

[tls]
ca_cert = "/etc/ci-runner/ca.crt"
ca_key = "/etc/ci-runner/ca.key"
server_cert = "/etc/ci-runner/server.crt"
server_key = "/etc/ci-runner/server.key"

[runner_scope]
type = "organization"
name = "test-org"
"#;

        let config = parse_config(config_str);
        assert_eq!(config.provisioning.mode, ProvisioningMode::FixedCapacity);
    }

    #[test]
    fn test_provisioning_mode_fixed_capacity() {
        let config_str = r#"
[github]
app_id = 123456
private_key_path = "/etc/ci-runner/github-app.pem"

[tls]
ca_cert = "/etc/ci-runner/ca.crt"
ca_key = "/etc/ci-runner/ca.key"
server_cert = "/etc/ci-runner/server.crt"
server_key = "/etc/ci-runner/server.key"

[provisioning]
mode = "fixed_capacity"

[runner_scope]
type = "organization"
name = "test-org"
"#;

        let config = parse_config(config_str);
        assert_eq!(config.provisioning.mode, ProvisioningMode::FixedCapacity);
    }

    #[test]
    fn test_provisioning_mode_webhook() {
        let config_str = r#"
[github]
app_id = 123456
private_key_path = "/etc/ci-runner/github-app.pem"

[tls]
ca_cert = "/etc/ci-runner/ca.crt"
ca_key = "/etc/ci-runner/ca.key"
server_cert = "/etc/ci-runner/server.crt"
server_key = "/etc/ci-runner/server.key"

[provisioning]
mode = "webhook"

[runner_scope]
type = "organization"
name = "test-org"

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
        assert_eq!(
            webhook.label_mappings[0].labels,
            vec!["self-hosted", "macOS"]
        );
    }

    #[test]
    fn test_webhook_config_default_path() {
        let config_str = r#"
[github]
app_id = 123456
private_key_path = "/etc/ci-runner/github-app.pem"

[tls]
ca_cert = "/etc/ci-runner/ca.crt"
ca_key = "/etc/ci-runner/ca.key"
server_cert = "/etc/ci-runner/server.crt"
server_key = "/etc/ci-runner/server.key"

[provisioning]
mode = "webhook"

[runner_scope]
type = "organization"
name = "test-org"

[provisioning.webhook]
secret = "test-secret"
"#;

        let config = parse_config(config_str);
        let webhook = config.provisioning.webhook.unwrap();
        assert_eq!(webhook.path, "/webhook"); // default path
    }
}
