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
//! - `KUIPER_PROVISIONING__POLLING__INTERVAL_SECS` → `provisioning.polling.interval_secs`
//! - `KUIPER_METRICS__RETENTION_DAYS` → `metrics.retention_days`
//!
//! **Note:** Complex arrays like `runners` and `label_mappings` should be configured
//! via TOML file, not environment variables.
//!
//! # Provisioning Modes
//!
//! The coordinator supports three provisioning modes:
//!
//! - **Fixed Capacity**: Maintains a fixed pool of runners per configuration.
//!   The coordinator ensures the target count of runners is always available.
//!
//! - **Webhook (Dynamic)**: Runners are created on-demand in response to GitHub
//!   webhook events. Labels from the workflow job determine which agent/VM base
//!   to use.
//!
//! - **Polling (Dynamic)**: Like webhook mode, but queued jobs are discovered by
//!   periodically scanning the GitHub API instead of receiving webhook events.

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

    /// Polling dynamic mode: create runners on-demand from an API scan.
    ///
    /// Same on-demand behaviour as webhook mode, but queued jobs are found by
    /// periodically listing them through the API rather than via webhook
    /// delivery. Useful when the coordinator isn't reachable from the forge.
    Polling,
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

    /// Polling configuration (only used when mode is `polling`). Optional:
    /// every field has a default, so `mode = "polling"` alone is enough.
    #[serde(default)]
    pub polling: Option<PollingConfig>,

    /// Seconds a job-bound runner may wait for its job to start before its VM is
    /// destroyed (default: 600, `0` disables). Applies to Forgejo one-job runners,
    /// whose job can stay "waiting" behind a concurrency group; the job is retried
    /// later with a growing delay. GitHub JIT runners aren't bound to one job, so
    /// they're left alone.
    #[serde(default = "default_idle_runner_timeout_secs")]
    pub idle_runner_timeout_secs: u64,
}

fn default_idle_runner_timeout_secs() -> u64 {
    600
}

/// Shortest idle runner timeout: below this a VM may not have booted yet.
pub const MIN_IDLE_RUNNER_TIMEOUT_SECS: u64 = 120;

impl ProvisioningConfig {
    /// The idle runner timeout, or `None` when disabled. Values below
    /// [`MIN_IDLE_RUNNER_TIMEOUT_SECS`] are raised to it.
    pub fn idle_runner_timeout(&self) -> Option<std::time::Duration> {
        match self.idle_runner_timeout_secs {
            0 => None,
            secs => Some(std::time::Duration::from_secs(
                secs.max(MIN_IDLE_RUNNER_TIMEOUT_SECS),
            )),
        }
    }

    /// Job filters for the active on-demand mode, or `None` in fixed-capacity mode
    /// (and in webhook mode without a `[provisioning.webhook]` section).
    pub fn on_demand_filters(&self) -> Option<OnDemandFilters> {
        match self.mode {
            ProvisioningMode::FixedCapacity => None,
            ProvisioningMode::Webhook => self.webhook.as_ref().map(|w| OnDemandFilters {
                required_labels: w.required_labels.clone(),
                label_mappings: w.label_mappings.clone(),
                queued_scan_lookback_hours: w.queued_scan_lookback_hours,
            }),
            ProvisioningMode::Polling => {
                let p = self.polling_config();
                Some(OnDemandFilters {
                    required_labels: p.required_labels,
                    label_mappings: p.label_mappings,
                    queued_scan_lookback_hours: p.queued_scan_lookback_hours,
                })
            }
        }
    }

    /// The polling configuration, falling back to defaults when the section is absent.
    pub fn polling_config(&self) -> PollingConfig {
        self.polling.clone().unwrap_or_default()
    }
}

/// Job filters shared by the on-demand modes (webhook and polling).
#[derive(Debug, Clone)]
pub struct OnDemandFilters {
    pub required_labels: Vec<String>,
    pub label_mappings: Vec<LabelMapping>,
    pub queued_scan_lookback_hours: u64,
}

impl Default for ProvisioningConfig {
    fn default() -> Self {
        Self {
            mode: ProvisioningMode::FixedCapacity,
            webhook: None,
            polling: None,
            idle_runner_timeout_secs: default_idle_runner_timeout_secs(),
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

    /// Required labels that ALL jobs must have to be accepted (default: ["self-hosted"]).
    ///
    /// This is a pre-filter before checking `label_mappings`. Jobs that don't have
    /// ALL of these labels are ignored. This prevents accepting GitHub-hosted runner
    /// jobs by mistake. It applies to jobs from every forge, so workflows on all of
    /// them must carry these labels (Forgejo doesn't add "self-hosted" by itself).
    ///
    /// Set to empty `[]` to disable the filter (not recommended).
    #[serde(default = "default_required_labels")]
    pub required_labels: Vec<String>,

    /// Label mappings: maps workflow labels to runner scopes.
    ///
    /// When a webhook event arrives, the coordinator looks up the job's labels
    /// to determine which runner scope to use for registration.
    ///
    /// If empty and `required_labels` matches, jobs are accepted with the
    /// organization/repository scope from the webhook event.
    #[serde(default)]
    pub label_mappings: Vec<LabelMapping>,

    /// How far back (in hours) the periodic GitHub recovery scan considers a
    /// repository "active" when hunting for queued jobs that were missed
    /// (default: 168 = 7 days). Set to `0` to scan every accessible repo with
    /// no recency filter.
    ///
    /// The scan uses this as a coarse pre-filter on each repo's `pushed_at` to
    /// avoid querying every repo's workflow runs. Note that `pushed_at` only
    /// tracks pushes, so jobs queued by non-push triggers (schedule,
    /// workflow_dispatch, etc.) on a repo that hasn't been pushed recently
    /// would be missed by a narrow window — keep this generous, or `0`.
    #[serde(default = "default_queued_scan_lookback_hours")]
    pub queued_scan_lookback_hours: u64,

    /// Configured name of the GitHub forge these webhooks come from (default:
    /// the first GitHub forge). Webhook deliveries are GitHub-only for now.
    #[serde(default)]
    pub forge: Option<String>,
}

/// Polling configuration for dynamic runner provisioning.
///
/// Used when `provisioning.mode = "polling"`. The coordinator scans the GitHub
/// API for queued jobs every `interval_secs` and creates runners on-demand,
/// exactly as webhook mode does for delivered events. No webhook endpoint is served.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PollingConfig {
    /// Seconds between scans (default: 120, minimum: 10).
    ///
    /// GitHub has no org-wide queued-jobs endpoint, so each scan lists workflow
    /// runs for every recently pushed repo. Short intervals across many repos can
    /// exhaust the GitHub App's API rate limit.
    #[serde(default = "default_poll_interval_secs")]
    pub interval_secs: u64,

    /// Required labels that ALL jobs must have to be accepted (default: ["self-hosted"]).
    /// Same meaning as in webhook mode. Applies to jobs from every forge, so GitHub
    /// and Forgejo workflows must both carry these labels.
    #[serde(default = "default_required_labels")]
    pub required_labels: Vec<String>,

    /// Label mappings: maps workflow labels to runner scopes. Same meaning as in
    /// webhook mode; if empty, jobs are accepted with the scope the scan found them in.
    #[serde(default)]
    pub label_mappings: Vec<LabelMapping>,

    /// How far back (in hours) a repository's last push may be for it to be
    /// scanned (default: 168 = 7 days, `0` = scan every repo). Same as webhook mode.
    #[serde(default = "default_queued_scan_lookback_hours")]
    pub queued_scan_lookback_hours: u64,
}

impl Default for PollingConfig {
    fn default() -> Self {
        Self {
            interval_secs: default_poll_interval_secs(),
            required_labels: default_required_labels(),
            label_mappings: Vec::new(),
            queued_scan_lookback_hours: default_queued_scan_lookback_hours(),
        }
    }
}

/// Shortest allowed polling interval; lower values are raised to this.
pub const MIN_POLL_INTERVAL_SECS: u64 = 10;

impl PollingConfig {
    /// The scan interval, clamped to [`MIN_POLL_INTERVAL_SECS`].
    pub fn interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.interval_secs.max(MIN_POLL_INTERVAL_SECS))
    }
}

fn default_poll_interval_secs() -> u64 {
    120
}

fn default_webhook_path() -> String {
    "/webhook".to_string()
}

fn default_queued_scan_lookback_hours() -> u64 {
    168 // 7 days
}

fn default_required_labels() -> Vec<String> {
    vec!["self-hosted".to_string()]
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

    /// Only match jobs from this forge (by configured name). If not specified,
    /// the mapping matches jobs from any forge.
    #[serde(default)]
    pub forge: Option<String>,
}

/// Main configuration for the coordinator daemon.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    /// GitHub App configuration (legacy single-forge form).
    ///
    /// Equivalent to one `[[forges]]` entry named `github` with `type = "github"`.
    /// Use either this or `[[forges]]`, not both.
    #[serde(default)]
    pub github: Option<GitHubConfig>,

    /// Forges to serve (GitHub, later Forgejo), each under a unique name.
    ///
    /// The first forge is the default: fixed-capacity pools and the global
    /// `runner_scope` register with it.
    #[serde(default)]
    pub forges: Vec<ForgeConfig>,

    /// gRPC server settings
    #[serde(default)]
    pub grpc: GrpcConfig,

    /// TLS certificate paths
    pub tls: TlsConfig,

    /// Database configuration.
    #[serde(default)]
    pub database: DatabaseConfig,

    /// Admin web UI configuration.
    #[serde(default)]
    pub admin: AdminConfig,

    /// Run history kept for the metrics page.
    #[serde(default)]
    pub metrics: MetricsConfig,

    /// Provisioning mode and settings.
    ///
    /// Controls how runners are created:
    /// - `fixed_capacity` (default): Maintain a constant pool of runners
    /// - `webhook`: Create runners on-demand via GitHub webhooks
    #[serde(default)]
    pub provisioning: ProvisioningConfig,

    /// Where fixed-capacity pool runners are registered on the default forge.
    ///
    /// Required in fixed-capacity mode, unused otherwise: on-demand runners are
    /// registered where their job is (the webhook's scope, the scan's, or a label
    /// mapping's).
    #[serde(default)]
    pub runner_scope: Option<RunnerScope>,

    /// Optional runner group ID for JIT config generation (default: 1).
    ///
    /// GitHub's default runner group is always ID 1. Only set this if your
    /// org uses custom runner groups.
    #[serde(default)]
    pub runner_group: Option<String>,
}

/// A forge the coordinator serves, under a unique name.
///
/// ```toml
/// [[forges]]
/// name = "github"
/// type = "github"
/// app_id = 123456
/// private_key_path = "/etc/kuiper/github-app.pem"
/// ```
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ForgeConfig {
    /// Name jobs and runners refer to this forge by. Must be unique and must not
    /// contain `:` (it prefixes job keys like `github:123`).
    pub name: String,

    /// Which forge this is, and its credentials.
    #[serde(flatten)]
    pub kind: ForgeKindConfig,
}

/// Forge type and its settings, tagged by `type`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ForgeKindConfig {
    /// GitHub, authenticated as a GitHub App.
    Github(GitHubConfig),
    /// Forgejo (15 or newer), authenticated with an access token. Polling mode only.
    Forgejo(ForgejoConfig),
}

/// A Forgejo instance, polled for queued jobs.
///
/// ```toml
/// [[forges]]
/// name = "knope"
/// type = "forgejo"
/// url = "https://git.knope.dev"
/// token_env = "KUIPER_KNOPE_TOKEN"
/// scopes = ["lodge", "jfro/kuiper-forge"]
/// ```
///
/// The token's user must own every scope: an org owner with `write:organization`
/// for org scopes, the repo owner with `write:repository` for repo scopes.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ForgejoConfig {
    /// Base URL of the Forgejo instance, like `https://git.example.com`
    pub url: String,

    /// Access token, inline. Prefer `token_env` or `token_path`.
    #[serde(default)]
    pub token: Option<String>,

    /// Path to a file holding the access token.
    #[serde(default)]
    pub token_path: Option<PathBuf>,

    /// Name of an environment variable holding the access token.
    #[serde(default)]
    pub token_env: Option<String>,

    /// Where to poll for queued jobs and register runners. Forgejo tokens can't list
    /// what they can reach, so these must be named:
    /// - `org`: an organization's repositories (token user owns the org,
    ///   `write:organization`)
    /// - `owner/repo`: one repository (token user owns it, `write:repository`)
    /// - `@user`: every repository the user owns, not its orgs' (the token must be
    ///   that user's, `write:user`)
    /// - `*`: every repository on the instance (a site admin's token, `write:admin`)
    ///
    /// Org, user and instance scopes also need `read:repository`, to name the
    /// repositories jobs come from.
    pub scopes: Vec<String>,
}

impl ForgejoConfig {
    /// Resolve the access token from `token_env`, `token_path` or `token`, in that order.
    pub fn token_content(&self) -> Result<String> {
        let token = if let Some(ref var) = self.token_env {
            std::env::var(var).with_context(|| format!("Forgejo token_env '{var}' is not set"))?
        } else if let Some(ref path) = self.token_path {
            std::fs::read_to_string(path)
                .with_context(|| format!("Failed to read Forgejo token: {}", path.display()))?
        } else if let Some(ref token) = self.token {
            token.clone()
        } else {
            anyhow::bail!("One of token_env, token_path or token must be set for a Forgejo forge")
        };
        let token = token.trim().to_string();
        if token.is_empty() {
            anyhow::bail!("Forgejo token is empty");
        }
        Ok(token)
    }

    /// The configured scopes as runner scopes.
    pub fn runner_scopes(&self) -> Result<Vec<RunnerScope>> {
        if self.scopes.is_empty() {
            anyhow::bail!("A Forgejo forge needs at least one entry in scopes");
        }
        self.scopes
            .iter()
            .map(|scope| match scope.split('/').collect::<Vec<_>>()[..] {
                ["*"] => Ok(RunnerScope::Instance),
                [user] if user.len() > 1 && user.starts_with('@') => Ok(RunnerScope::User {
                    name: user[1..].to_string(),
                }),
                [org] if !org.is_empty() && !org.starts_with('@') => {
                    Ok(RunnerScope::Organization {
                        name: org.to_string(),
                    })
                }
                [owner, repo]
                    if !owner.is_empty()
                        && !repo.is_empty()
                        && !owner.starts_with('@')
                        && owner != "*" =>
                {
                    Ok(RunnerScope::Repository {
                        owner: owner.to_string(),
                        repo: repo.to_string(),
                    })
                }
                _ => Err(anyhow::anyhow!(
                    "Invalid Forgejo scope '{scope}': use 'org', 'owner/repo', '@user' or '*'"
                )),
            })
            .collect()
    }
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

impl Config {
    /// The configured forges, with a legacy `[github]` section turned into a
    /// forge named [`LEGACY_GITHUB_FORGE`](crate::forge::LEGACY_GITHUB_FORGE).
    ///
    /// Errors if none are configured, both forms are used, or names are invalid.
    pub fn resolved_forges(&self) -> Result<Vec<ForgeConfig>> {
        let forges = match (&self.github, self.forges.is_empty()) {
            (Some(_), false) => {
                anyhow::bail!("Configure forges with either [github] or [[forges]], not both")
            }
            (Some(github), true) => vec![ForgeConfig {
                name: crate::forge::LEGACY_GITHUB_FORGE.to_string(),
                kind: ForgeKindConfig::Github(github.clone()),
            }],
            (None, false) => self.forges.clone(),
            (None, true) => {
                anyhow::bail!("No forge configured: add a [github] or [[forges]] section")
            }
        };
        for (i, forge) in forges.iter().enumerate() {
            if forge.name.is_empty() || forge.name.contains(':') {
                anyhow::bail!(
                    "Invalid forge name '{}': must be non-empty and must not contain ':'",
                    forge.name
                );
            }
            if forges[..i].iter().any(|f| f.name == forge.name) {
                anyhow::bail!("Forge name '{}' is configured more than once", forge.name);
            }
            if let ForgeKindConfig::Forgejo(forgejo) = &forge.kind {
                let url = reqwest::Url::parse(&forgejo.url).with_context(|| {
                    format!("Invalid url '{}' for forge '{}'", forgejo.url, forge.name)
                })?;
                if !matches!(url.scheme(), "http" | "https") {
                    anyhow::bail!(
                        "Invalid url '{}' for forge '{}': must be http or https",
                        forgejo.url,
                        forge.name
                    );
                }
                forgejo
                    .runner_scopes()
                    .with_context(|| format!("Invalid scopes for forge '{}'", forge.name))?;
            }
        }
        // Fixed-capacity pools register long-lived runners at [runner_scope] on the
        // default forge
        if self.provisioning.mode == ProvisioningMode::FixedCapacity {
            match &self.runner_scope {
                None => {
                    anyhow::bail!("Fixed-capacity mode needs a [runner_scope] for its pool runners")
                }
                Some(scope) if !scope.is_github_scope() => anyhow::bail!(
                    "[runner_scope] must be an organization or repository for fixed-capacity pools"
                ),
                Some(_) => {}
            }
        }
        // ...which Forgejo's one-job runners can't be
        if self.provisioning.mode == ProvisioningMode::FixedCapacity
            && matches!(forges[0].kind, ForgeKindConfig::Forgejo(_))
        {
            anyhow::bail!(
                "Forge '{}' is Forgejo, which needs provisioning mode \"polling\"; \
                 fixed-capacity pools register with the first forge",
                forges[0].name
            );
        }
        Ok(forges)
    }
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
    /// When enabled, the server accepts a PROXY protocol header at the start of
    /// a connection (sent by load balancers like HAProxy, AWS NLB,
    /// DigitalOcean LB) and extracts the real client IP from it. The header is
    /// optional, so direct connections (e.g. in-cluster callers) still work.
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

// =============================================================================
// Admin UI Configuration
// =============================================================================

/// Admin web UI configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct AdminConfig {
    /// Enable the admin web UI (default: false).
    pub enabled: bool,

    /// Session timeout in seconds (default: 3600 = 1 hour).
    pub session_timeout_secs: u64,

    /// Coordinator URL for registration bundles (default: https://localhost:9443).
    /// Used when generating registration tokens in the admin UI.
    pub coordinator_url: String,
}

impl Default for AdminConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            session_timeout_secs: 3600,
            coordinator_url: "https://localhost:9443".to_string(),
        }
    }
}

/// Run history for the admin metrics page.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct MetricsConfig {
    /// Days of runner and job history to keep (default: 90).
    pub retention_days: u32,
}

impl Default for MetricsConfig {
    fn default() -> Self {
        Self { retention_days: 90 }
    }
}

impl MetricsConfig {
    pub fn retention(&self) -> chrono::Duration {
        chrono::Duration::days(i64::from(self.retention_days))
    }
}

/// Where runners are registered on a forge.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum RunnerScope {
    /// Organization-level runner
    Organization { name: String },

    /// Repository-level runner
    Repository { owner: String, repo: String },

    /// User-level runner, for every repository the user owns (Forgejo only; the
    /// token must be that user's)
    User { name: String },

    /// Instance-wide runner, for every repository on the forge (Forgejo only; needs a
    /// site admin's token)
    Instance,
}

impl RunnerScope {
    /// Whether GitHub can register runners here: organizations and repositories only.
    pub fn is_github_scope(&self) -> bool {
        matches!(
            self,
            RunnerScope::Organization { .. } | RunnerScope::Repository { .. }
        )
    }

    /// Get the URL for this runner scope
    pub fn to_url(&self) -> String {
        match self {
            RunnerScope::Organization { name } => {
                format!("https://github.com/{name}")
            }
            RunnerScope::Repository { owner, repo } => {
                format!("https://github.com/{owner}/{repo}")
            }
            RunnerScope::User { name } => format!("https://github.com/{name}"),
            RunnerScope::Instance => "https://github.com".to_string(),
        }
    }

    /// Get the API path for generating a JIT runner config
    pub fn jitconfig_path(&self) -> String {
        match self {
            RunnerScope::Organization { name } => {
                format!("/orgs/{name}/actions/runners/generate-jitconfig")
            }
            RunnerScope::Repository { owner, repo } => {
                format!("/repos/{owner}/{repo}/actions/runners/generate-jitconfig")
            }
            // not GitHub scopes; GitHubClient refuses them before building a path
            RunnerScope::User { .. } | RunnerScope::Instance => String::new(),
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
            RunnerScope::User { .. } | RunnerScope::Instance => String::new(),
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
            RunnerScope::User { .. } | RunnerScope::Instance => String::new(),
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
            RunnerScope::User { .. } | RunnerScope::Instance => String::new(),
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
            github: Some(GitHubConfig {
                app_id: 0,
                private_key_path: None,
                private_key: None,
            }),
            forges: Vec::new(),
            grpc: GrpcConfig::default(),
            tls: TlsConfig::with_defaults(data_dir),
            database: DatabaseConfig::default(),
            admin: AdminConfig::default(),
            metrics: MetricsConfig::default(),
            provisioning: ProvisioningConfig::default(),
            runner_scope: Some(RunnerScope::Organization {
                name: "test-org".into(),
            }),
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

# To serve several forges from one coordinator, replace [github] with a
# [[forges]] list. Each forge has a unique name (jobs are tracked as
# "<name>:<job id>"); the first one is the default, where fixed-capacity pools
# and [runner_scope] register. A [github] section is the same as one forge
# named "github".
#
# [[forges]]
# name = "github"
# type = "github"
# app_id = 123456
# private_key_path = "{data_dir_str}/github-app.pem"
#
# A Forgejo (15+) forge is polled for queued jobs, so it needs
# [provisioning] mode = "polling". Its runners are one-job runners, on agents
# that support them (proxmox Linux/Windows VMs, tart VMs). Scopes, and what the
# token needs:
#   "org"         the org's repos     org owner, write:organization + read:repository
#   "owner/repo"  one repo            repo owner, write:repository
#   "@user"       repos the user owns the token's own user, write:user + read:repository
#   "*"           every repo          site admin, write:admin + read:repository
# required_labels applies to every forge: Forgejo doesn't add "self-hosted" the
# way GitHub does, so put it in Forgejo workflows' runs-on too (or pick labels
# both forges' workflows use), or every Forgejo job is skipped.
#
# [[forges]]
# name = "forgejo"
# type = "forgejo"
# url = "https://git.example.com"
# token_env = "KUIPER_FORGEJO_TOKEN"   # or token_path / token
# scopes = ["my-org", "@me", "someone/some-repo"]   # or ["*"] with an admin token
#
# Forgejo runners whose job hasn't started after [provisioning]
# idle_runner_timeout_secs (default 600, 0 disables) are destroyed, and the job
# retried later; a job held back by a concurrency group stays "waiting" meanwhile.
#
# Label mappings (webhook/polling modes) can be limited to one forge with
# `forge = "<name>"`, and [provisioning.webhook] takes `forge = "<name>"` to
# say which GitHub forge its deliveries come from (default: the first).

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
# Where fixed-capacity pool runners are registered on the default (GitHub)
# forge. Agents report their labels and capacity; pools are automatically
# created based on connected agents. Only fixed-capacity mode uses it: webhook
# and polling modes register each runner where its job is, so leave it out there.

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
# Admin Web UI
# =============================================================================
#
# Optional web UI for managing agents and registration tokens.
# Access at https://<coordinator>:9443/admin/
#
# Create admin users with: coordinator admin create-user --username <name>

[admin]
enabled = false
# session_timeout_secs = 3600  # Session timeout (default: 1 hour)
# coordinator_url = "https://localhost:9443"  # URL for registration bundles

# Runner and job timings for the admin metrics page
# [metrics]
# retention_days = 90                  # days of history to keep (default 90)

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
#
# - "polling": Create runners on-demand by scanning the GitHub API for queued
#   jobs. Like "webhook", but needs no inbound webhook delivery.
#   Use [provisioning.polling] to tune it (optional; all fields have defaults).

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
# # Required labels pre-filter (default: ["self-hosted"])
# # Jobs must have ALL these labels to be considered. This prevents accepting
# # GitHub-hosted runner jobs by mistake. Set to [] to disable (not recommended).
# required_labels = ["self-hosted"]
#
# # Label mappings: map workflow labels to runner scopes (optional)
# # If empty, all jobs passing required_labels are accepted with org scope from webhook.
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

# -----------------------------------------------------------------------------
# Polling Mode Configuration (uncomment to use)
# -----------------------------------------------------------------------------
# Queued jobs are found by scanning the GitHub API; no webhook endpoint is needed.
# Each scan lists workflow runs per recently pushed repo, so keep the interval
# generous with many repos to stay within GitHub API rate limits.
#
# [provisioning]
# mode = "polling"
#
# [provisioning.polling]
# interval_secs = 120                 # seconds between scans (default 120, min 10)
# required_labels = ["self-hosted"]   # same as webhook mode; applies to every forge
# queued_scan_lookback_hours = 168    # only scan repos pushed within this window (0 = all)
#
# [[provisioning.polling.label_mappings]]
# labels = ["self-hosted", "Linux", "X64"]
#
# [provisioning.polling.label_mappings.runner_scope]
# type = "organization"
# name = "my-org"

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
        assert_eq!(config.github.as_ref().unwrap().app_id, 123456);
        assert_eq!(config.grpc.listen_addr, "0.0.0.0:9443");
        assert_eq!(
            config.runner_scope,
            Some(RunnerScope::Organization {
                name: "my-org".to_string()
            })
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
        assert_eq!(webhook.required_labels, vec!["self-hosted"]); // default required labels
    }

    const POLLING_BASE: &str = r#"
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

[provisioning]
mode = "polling"
"#;

    #[test]
    fn test_polling_config_defaults_without_section() {
        let config = parse_config(POLLING_BASE);
        assert_eq!(config.provisioning.mode, ProvisioningMode::Polling);
        assert!(config.provisioning.polling.is_none());

        let polling = config.provisioning.polling_config();
        assert_eq!(polling.interval_secs, 120);
        assert_eq!(polling.required_labels, vec!["self-hosted"]);
        assert!(polling.label_mappings.is_empty());

        let filters = config.provisioning.on_demand_filters().unwrap();
        assert_eq!(filters.required_labels, vec!["self-hosted"]);
        assert_eq!(filters.queued_scan_lookback_hours, 168);
        assert_eq!(config.metrics.retention_days, 90);
    }

    #[test]
    fn test_metrics_retention() {
        let config = parse_config(&format!("{POLLING_BASE}\n[metrics]\nretention_days = 14\n"));
        assert_eq!(config.metrics.retention(), chrono::Duration::days(14));
    }

    #[test]
    fn test_polling_config_section() {
        let config_str = format!(
            "{POLLING_BASE}{}",
            r#"
[provisioning.polling]
interval_secs = 45
required_labels = []
queued_scan_lookback_hours = 0

[[provisioning.polling.label_mappings]]
labels = ["self-hosted", "Linux"]

[provisioning.polling.label_mappings.runner_scope]
type = "repository"
owner = "my-org"
repo = "my-repo"
"#
        );
        let config = parse_config(&config_str);
        let polling = config.provisioning.polling_config();
        assert_eq!(polling.interval_secs, 45);
        assert!(polling.required_labels.is_empty());
        assert_eq!(polling.queued_scan_lookback_hours, 0);
        assert_eq!(polling.label_mappings.len(), 1);

        let filters = config.provisioning.on_demand_filters().unwrap();
        assert_eq!(
            filters.label_mappings[0].labels,
            vec!["self-hosted", "Linux"]
        );
        assert!(filters.required_labels.is_empty());
    }

    #[test]
    fn test_polling_interval_is_clamped() {
        let polling = PollingConfig {
            interval_secs: 1,
            ..PollingConfig::default()
        };
        assert_eq!(
            polling.interval(),
            std::time::Duration::from_secs(MIN_POLL_INTERVAL_SECS)
        );
    }

    const FORGE_BASE: &str = r#"
[tls]
ca_cert = "/etc/ci-runner/ca.crt"
ca_key = "/etc/ci-runner/ca.key"
server_cert = "/etc/ci-runner/server.crt"
server_key = "/etc/ci-runner/server.key"

[runner_scope]
type = "organization"
name = "test-org"
"#;

    #[test]
    fn test_legacy_github_section_is_a_forge() {
        let config = parse_config(&format!(
            "{FORGE_BASE}\n[github]\napp_id = 7\nprivate_key_path = \"/k.pem\"\n"
        ));
        let forges = config.resolved_forges().unwrap();
        assert_eq!(forges.len(), 1);
        assert_eq!(forges[0].name, "github");
        let ForgeKindConfig::Github(github) = &forges[0].kind else {
            panic!("expected a GitHub forge");
        };
        assert_eq!(github.app_id, 7);
    }

    #[test]
    fn test_forges_list() {
        let config = parse_config(&format!(
            "{FORGE_BASE}{}",
            r#"
[[forges]]
name = "gh-main"
type = "github"
app_id = 1
private_key_path = "/a.pem"

[[forges]]
name = "gh-other"
type = "github"
app_id = 2
private_key = "PEM"
"#
        ));
        let forges = config.resolved_forges().unwrap();
        let names: Vec<_> = forges.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, vec!["gh-main", "gh-other"]);
        let ForgeKindConfig::Github(second) = &forges[1].kind else {
            panic!("expected a GitHub forge");
        };
        assert_eq!(second.app_id, 2);
        assert_eq!(second.private_key.as_deref(), Some("PEM"));
    }

    #[test]
    fn test_forge_config_errors() {
        // neither form
        assert!(parse_config(FORGE_BASE).resolved_forges().is_err());

        let entry = |name: &str| {
            format!("\n[[forges]]\nname = \"{name}\"\ntype = \"github\"\napp_id = 1\n")
        };
        // both forms
        let both = format!("{FORGE_BASE}\n[github]\napp_id = 1\n{}", entry("x"));
        assert!(parse_config(&both).resolved_forges().is_err());
        // duplicate names
        let dup = format!("{FORGE_BASE}{}{}", entry("x"), entry("x"));
        assert!(parse_config(&dup).resolved_forges().is_err());
        // ':' in a name
        let colon = format!("{FORGE_BASE}{}", entry("a:b"));
        assert!(parse_config(&colon).resolved_forges().is_err());
    }

    const FORGEJO_ENTRY: &str = r#"
[[forges]]
name = "knope"
type = "forgejo"
url = "https://git.knope.dev"
token = "abc"
scopes = ["lodge", "jfro/kuiper-forge"]
"#;

    #[test]
    fn test_forgejo_forge() {
        let config = parse_config(&format!(
            "{FORGE_BASE}\n[provisioning]\nmode = \"polling\"\n{FORGEJO_ENTRY}"
        ));
        let forges = config.resolved_forges().unwrap();
        let ForgeKindConfig::Forgejo(forgejo) = &forges[0].kind else {
            panic!("expected a Forgejo forge");
        };
        assert_eq!(forgejo.token_content().unwrap(), "abc");
        assert_eq!(
            forgejo.runner_scopes().unwrap(),
            vec![
                RunnerScope::Organization {
                    name: "lodge".into()
                },
                RunnerScope::Repository {
                    owner: "jfro".into(),
                    repo: "kuiper-forge".into()
                },
            ]
        );
    }

    #[test]
    fn test_runner_scope_only_needed_for_fixed_capacity() {
        let base = FORGE_BASE.replace(
            "\n[runner_scope]\ntype = \"organization\"\nname = \"test-org\"\n",
            "",
        );
        assert!(!base.contains("[runner_scope]"));
        // polling with a Forgejo forge: no [runner_scope] needed
        let polling = parse_config(&format!(
            "{base}\n[provisioning]\nmode = \"polling\"\n{FORGEJO_ENTRY}"
        ));
        assert_eq!(polling.runner_scope, None);
        assert!(polling.resolved_forges().is_ok());
        // fixed capacity needs one
        let github = "\n[github]\napp_id = 1\nprivate_key_path = \"/k.pem\"\n";
        assert!(
            parse_config(&format!("{base}{github}"))
                .resolved_forges()
                .is_err()
        );
        assert!(
            parse_config(&format!("{FORGE_BASE}{github}"))
                .resolved_forges()
                .is_ok()
        );
    }

    #[test]
    fn test_forgejo_user_and_instance_scopes() {
        let forgejo = ForgejoConfig {
            url: "https://git.example.com".into(),
            token: Some("t".into()),
            token_path: None,
            token_env: None,
            scopes: vec!["*".into(), "@jfro".into(), "lodge".into()],
        };
        assert_eq!(
            forgejo.runner_scopes().unwrap(),
            vec![
                RunnerScope::Instance,
                RunnerScope::User {
                    name: "jfro".into()
                },
                RunnerScope::Organization {
                    name: "lodge".into()
                },
            ]
        );
        // stored runner scopes round-trip, and old rows still parse
        for scope in forgejo.runner_scopes().unwrap() {
            let json = serde_json::to_string(&scope).unwrap();
            assert_eq!(serde_json::from_str::<RunnerScope>(&json).unwrap(), scope);
        }
        assert_eq!(
            serde_json::to_string(&RunnerScope::Instance).unwrap(),
            r#"{"type":"instance"}"#
        );
        assert!(!RunnerScope::Instance.is_github_scope());
    }

    #[test]
    fn test_forgejo_forge_errors() {
        let polling = format!("{FORGE_BASE}\n[provisioning]\nmode = \"polling\"\n");
        // bad scopes
        for bad in [
            "[]",
            "[\"a/b/c\"]",
            "[\"/repo\"]",
            "[\"\"]",
            "[\"@\"]",
            "[\"@me/repo\"]",
            "[\"*/repo\"]",
        ] {
            let entry = FORGEJO_ENTRY.replace(r#"["lodge", "jfro/kuiper-forge"]"#, bad);
            assert!(
                parse_config(&format!("{polling}{entry}"))
                    .resolved_forges()
                    .is_err(),
                "scopes {bad} should be rejected"
            );
        }
        // bad url
        let entry = FORGEJO_ENTRY.replace("https://git.knope.dev", "git.knope.dev");
        assert!(
            parse_config(&format!("{polling}{entry}"))
                .resolved_forges()
                .is_err()
        );
        // fixed-capacity with Forgejo as the default forge
        assert!(
            parse_config(&format!("{FORGE_BASE}{FORGEJO_ENTRY}"))
                .resolved_forges()
                .is_err()
        );
        // missing token
        let entry = FORGEJO_ENTRY.replace("token = \"abc\"\n", "");
        let config = parse_config(&format!("{polling}{entry}"));
        let forges = config.resolved_forges().unwrap();
        let ForgeKindConfig::Forgejo(forgejo) = &forges[0].kind else {
            panic!("expected a Forgejo forge");
        };
        assert!(forgejo.token_content().is_err());
    }

    #[test]
    fn test_idle_runner_timeout() {
        let mut provisioning = ProvisioningConfig::default();
        assert_eq!(
            provisioning.idle_runner_timeout(),
            Some(std::time::Duration::from_secs(600))
        );
        provisioning.idle_runner_timeout_secs = 0;
        assert_eq!(provisioning.idle_runner_timeout(), None);
        provisioning.idle_runner_timeout_secs = 5;
        assert_eq!(
            provisioning.idle_runner_timeout(),
            Some(std::time::Duration::from_secs(MIN_IDLE_RUNNER_TIMEOUT_SECS))
        );
    }

    #[test]
    fn test_on_demand_filters_by_mode() {
        let mut provisioning = ProvisioningConfig::default();
        assert!(provisioning.on_demand_filters().is_none());

        // webhook mode without a section has no filters (and no endpoint)
        provisioning.mode = ProvisioningMode::Webhook;
        assert!(provisioning.on_demand_filters().is_none());
    }
}
