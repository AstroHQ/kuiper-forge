//! CI Runner Coordinator - Main entry point
//!
//! The coordinator daemon manages ephemeral GitHub Actions runners
//! across multiple VM providers (Tart for macOS, Proxmox for Windows/Linux).

use anyhow::{anyhow, Context, Result};
use chrono::Duration;
use clap::{Parser, Subcommand};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::signal;
use tracing::{debug, info, warn};
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use kuiper_forge::agent_registry::AgentRegistry;
use kuiper_forge::auth::{export_ca_cert, generate_server_cert, init_ca, AuthManager, AuthStore};
use kuiper_forge::config::{self, Config, ProvisioningMode};
use kuiper_forge::db::Database;
use kuiper_forge::fleet;
use kuiper_forge::github;
use kuiper_forge::management::{self, ManagementClient};
use kuiper_forge::pending_jobs;
use kuiper_forge::runner_state;
use kuiper_forge::server::{run_server, ServerConfig};

/// CI Runner Coordinator - Manages ephemeral GitHub Actions runners
#[derive(Parser)]
#[command(name = "coordinator")]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Path to configuration file
    #[arg(short, long, default_value_os_t = Config::default_path())]
    config: PathBuf,

    /// Data directory for certificates and state
    #[arg(short, long, default_value_os_t = Config::default_data_dir())]
    data_dir: PathBuf,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run the coordinator daemon
    Serve {
        /// Address to listen on (overrides config)
        #[arg(long)]
        listen: Option<SocketAddr>,

        /// Dry-run mode: skip GitHub integration (for testing agent registration)
        #[arg(long)]
        dry_run: bool,

        /// Disable file logging (log to stdout/stderr only)
        #[arg(long, env = "KUIPER_NO_LOG_FILE")]
        no_log_file: bool,
    },

    /// Certificate Authority management
    Ca {
        #[command(subcommand)]
        command: CaCommands,
    },

    /// Registration token management
    Token {
        #[command(subcommand)]
        command: TokenCommands,
    },

    /// Agent management
    Agent {
        #[command(subcommand)]
        command: AgentCommands,
    },

    /// Generate a default configuration file
    InitConfig {
        /// Output path (defaults to stdout if not specified)
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum CaCommands {
    /// Initialize the Certificate Authority
    Init {
        /// Organization name for the CA certificate
        #[arg(long, default_value = "CI Runner")]
        org: String,
    },

    /// Generate a server certificate
    ServerCert {
        /// Hostname for the server certificate
        #[arg(long)]
        hostname: String,
    },

    /// Export the CA certificate (for distribution to agents)
    Export,
}

#[derive(Subcommand)]
enum TokenCommands {
    /// Create a new registration token
    Create {
        /// Token expiration time (e.g., "1h", "30m", "1d")
        #[arg(long, default_value = "1h")]
        expires: String,
    },

    /// List pending registration tokens
    List,

    /// Revoke a registration token
    Revoke {
        /// Token to revoke
        token: String,
    },
}

#[derive(Subcommand)]
enum AgentCommands {
    /// List registered agents
    List,

    /// Revoke an agent's certificate
    Revoke {
        /// Agent ID to revoke
        agent_id: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Initialize logging based on command type
    // Base filter suppresses noisy libraries, RUST_LOG layers on top (can override if explicit)
    let base = "hyper=warn,reqwest=warn,h2=warn,rustls=warn,tonic=warn";
    let filter = match std::env::var("RUST_LOG") {
        Ok(env) => EnvFilter::new(format!("{base},{env}")),
        Err(_) => EnvFilter::new(format!("{base},info")),
    };

    match cli.command {
        Commands::Serve { listen, dry_run, no_log_file } => {
            if no_log_file {
                init_cli_logging(filter);
            } else {
                init_daemon_logging(&cli.data_dir, filter)?;
            }
            serve(&cli.config, &cli.data_dir, listen, dry_run).await
        }
        Commands::Ca { command } => {
            init_cli_logging(filter);
            handle_ca_command(command, &cli.data_dir).await
        }
        Commands::Token { command } => {
            init_cli_logging(filter);
            handle_token_command(command, &cli.data_dir).await
        }
        Commands::Agent { command } => {
            init_cli_logging(filter);
            handle_agent_command(command, &cli.data_dir).await
        }
        Commands::InitConfig { output } => {
            init_cli_logging(filter);
            generate_config(output)
        }
    }
}

/// Initialize logging for CLI commands (stdout only).
fn init_cli_logging(filter: EnvFilter) {
    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_target(false))
        .init();
}

/// Initialize logging for daemon mode (stdout + rotating file).
fn init_daemon_logging(data_dir: &Path, filter: EnvFilter) -> Result<()> {
    let log_dir = data_dir.join("logs");
    std::fs::create_dir_all(&log_dir)
        .with_context(|| format!("Failed to create log directory: {}", log_dir.display()))?;

    // Create a daily rotating file appender (e.g., coordinator.2026-01-15.log)
    let file_appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("coordinator")
        .filename_suffix("log")
        .build(&log_dir)
        .with_context(|| "Failed to create log file appender")?;

    // Non-blocking writer for the file
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    // Leak the guard to keep the writer alive for the lifetime of the program
    // This is intentional for a long-running daemon
    std::mem::forget(_guard);

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_target(false)) // stdout
        .with(fmt::layer().with_target(true).with_ansi(false).with_writer(non_blocking)) // file
        .init();

    info!("Logging to: {}", log_dir.display());
    Ok(())
}

/// Run the coordinator daemon
async fn serve(config_path: &Path, data_dir: &Path, listen_override: Option<SocketAddr>, dry_run: bool) -> Result<()> {
    ensure_data_dir(data_dir)?;

    // Load configuration (with relaxed validation in dry-run mode)
    let config = if dry_run {
        Config::load_or_default(config_path, data_dir)?
    } else {
        Config::load(config_path)?
    };

    // Determine listen address
    let listen_addr: SocketAddr = listen_override
        .unwrap_or_else(|| {
            config.grpc.listen_addr.parse().expect("Invalid listen address in config")
        });

    // Initialize database (shared across components)
    let db = Arc::new(Database::new(&config.database, data_dir).await?);

    // Initialize auth store using shared database
    let auth_store = Arc::new(AuthStore::new(db.pool().clone()));
    let auth_manager = Arc::new(AuthManager::new(
        auth_store.clone(),
        &config.tls.ca_cert,
        &config.tls.ca_key,
    )?);

    // Initialize agent registry
    let agent_registry = Arc::new(AgentRegistry::new());

    // Initialize persistent runner state for crash recovery (using shared database)
    let runner_state = Arc::new(runner_state::RunnerStateStore::new(db.pool()));
    runner_state.load_and_log().await;

    // Initialize persistent pending job store for webhook mode (using shared database)
    let pending_job_store = Arc::new(pending_jobs::PendingJobStore::new(db.pool()));
    pending_job_store.load_and_log().await;

    // Start management socket server for CLI communication
    let mgmt_socket_path = management::default_socket_path(data_dir);
    let mgmt_socket_path_cleanup = mgmt_socket_path.clone();
    let mgmt_auth = auth_manager.clone();
    tokio::spawn(async move {
        if let Err(e) = management::run_management_server(mgmt_auth, mgmt_socket_path).await {
            tracing::error!("Management server error: {}", e);
        }
    });

    // Initialize token provider and fleet manager
    // In dry-run mode, use mock tokens instead of GitHub API
    let (fleet_manager, fleet_notifier, webhook_notifier) = if dry_run {
        warn!("DRY-RUN MODE: Using mock token provider (fake GitHub registration tokens)");
        let token_provider: Arc<dyn github::RunnerTokenProvider> =
            Arc::new(github::MockTokenProvider);
        let (fm, notifier, wh_notifier) = fleet::FleetManager::new(
            config.clone(),
            token_provider,
            agent_registry.clone(),
            runner_state.clone(),
            pending_job_store.clone(),
        );
        (Some(fm), Some(notifier), wh_notifier)
    } else {
        let private_key = config.github.private_key_content()?;
        let github_client = github::GitHubClient::from_key(
            config.github.app_id,
            private_key,
        )?;

        // Validate GitHub API access before starting
        // This catches configuration issues early rather than failing later
        github_client.validate().await.context(
            "GitHub API validation failed. Fix the configuration or use --dry-run to skip GitHub integration."
        )?;

        let token_provider: Arc<dyn github::RunnerTokenProvider> = Arc::new(github_client);

        let (fm, notifier, wh_notifier) = fleet::FleetManager::new(
            config.clone(),
            token_provider,
            agent_registry.clone(),
            runner_state.clone(),
            pending_job_store.clone(),
        );
        (Some(fm), Some(notifier), wh_notifier)
    };

    info!("CI Runner Coordinator starting...");
    if dry_run {
        info!("MODE: dry-run (fleet manager uses mock tokens, VMs will be created but runner config will fail)");
    }
    info!("Listening on: {}", listen_addr);
    info!("Managing {} runner configurations", config.runners.len());

    // Build webhook config if in webhook mode
    let webhook_config = match (&config.provisioning.mode, &config.provisioning.webhook, webhook_notifier) {
        (ProvisioningMode::Webhook, Some(wh_config), Some(wh_notifier)) => {
            info!("Webhook mode enabled - webhook endpoint at {}", wh_config.path);
            Some((wh_config.clone(), wh_notifier))
        }
        _ => None,
    };

    // Run the gRPC server
    let server_config = ServerConfig {
        listen_addr,
        tls: config.tls.clone(),
    };

    // Spawn stale agent cleanup task
    let cleanup_registry = agent_registry.clone();
    tokio::spawn(async move {
        let cleanup_interval = std::time::Duration::from_secs(60);
        let stale_timeout = std::time::Duration::from_secs(120); // 2 minutes without heartbeat
        let mut ticker = tokio::time::interval(cleanup_interval);

        loop {
            ticker.tick().await;
            let removed = cleanup_registry.remove_stale(stale_timeout).await;
            if !removed.is_empty() {
                warn!("Removed {} stale agents: {:?}", removed.len(), removed);
            }
        }
    });

    // Spawn expired token cleanup task
    let cleanup_auth_store = auth_store.clone();
    tokio::spawn(async move {
        let cleanup_interval = std::time::Duration::from_secs(3600); // hourly
        let mut ticker = tokio::time::interval(cleanup_interval);

        loop {
            ticker.tick().await;
            let removed = cleanup_auth_store.cleanup_expired_tokens().await;
            if removed > 0 {
                info!("Cleaned up {} expired registration token(s)", removed);
            }
        }
    });

    // Spawn status logging task
    let status_registry = agent_registry.clone();
    tokio::spawn(async move {
        let status_interval = std::time::Duration::from_secs(60);
        let mut ticker = tokio::time::interval(status_interval);

        loop {
            ticker.tick().await;
            let count = status_registry.count().await;
            let agents = status_registry.list_all().await;

            debug!("Agent status: {} connected", count);
            for agent in &agents {
                debug!(
                    "  {} ({}) @ {} - {}/{} VMs, labels={:?}, {}",
                    agent.agent_id,
                    agent.agent_type,
                    agent.hostname,
                    agent.active_vms,
                    agent.max_vms,
                    agent.labels,
                    agent.status_display()
                );
            }
        }
    });

    // Run fleet manager (if enabled) and server concurrently, with graceful shutdown
    let result = if let Some(fm) = fleet_manager {
        tokio::select! {
            result = run_server(server_config, auth_manager, agent_registry, fleet_notifier, Some(runner_state), Some(pending_job_store), webhook_config) => {
                result
            }
            _ = fm.run() => {
                // Fleet manager runs forever, shouldn't exit
                Ok(())
            }
            _ = shutdown_signal() => {
                info!("Shutdown signal received");
                Ok(())
            }
        }
    } else {
        tokio::select! {
            result = run_server(server_config, auth_manager, agent_registry, None, None, None, None) => {
                result
            }
            _ = shutdown_signal() => {
                info!("Shutdown signal received");
                Ok(())
            }
        }
    };

    // Cleanup: remove the management socket
    if mgmt_socket_path_cleanup.exists() {
        if let Err(e) = std::fs::remove_file(&mgmt_socket_path_cleanup) {
            warn!("Failed to remove management socket: {}", e);
        } else {
            debug!("Removed management socket: {}", mgmt_socket_path_cleanup.display());
        }
    }

    info!("Coordinator shutdown complete");
    result
}

/// Wait for shutdown signal (Ctrl+C or SIGTERM)
async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}

/// Ensure data directory exists
fn ensure_data_dir(data_dir: &Path) -> Result<()> {
    if !data_dir.exists() {
        std::fs::create_dir_all(data_dir)
            .with_context(|| format!("Failed to create data directory: {}", data_dir.display()))?;
        info!("Created data directory: {}", data_dir.display());
    }
    Ok(())
}

/// Handle CA subcommands
async fn handle_ca_command(command: CaCommands, data_dir: &Path) -> Result<()> {
    ensure_data_dir(data_dir)?;

    match command {
        CaCommands::Init { org } => {
            let ca_cert_path = data_dir.join("ca.crt");
            let ca_key_path = data_dir.join("ca.key");

            init_ca(&ca_cert_path, &ca_key_path, &org)?;

            println!("Generated CA certificate and key:");
            println!("  CA cert: {}", ca_cert_path.display());
            println!("  CA key:  {} (keep secure!)", ca_key_path.display());

            Ok(())
        }

        CaCommands::ServerCert { hostname } => {
            let ca_cert_path = data_dir.join("ca.crt");
            let ca_key_path = data_dir.join("ca.key");
            let server_cert_path = data_dir.join("server.crt");
            let server_key_path = data_dir.join("server.key");

            generate_server_cert(
                &ca_cert_path,
                &ca_key_path,
                &server_cert_path,
                &server_key_path,
                &hostname,
            )?;

            println!("Generated server certificate:");
            println!("  Cert: {}", server_cert_path.display());
            println!("  Key:  {}", server_key_path.display());

            Ok(())
        }

        CaCommands::Export => {
            let ca_cert_path = data_dir.join("ca.crt");
            let cert = export_ca_cert(&ca_cert_path)?;
            print!("{cert}");
            Ok(())
        }
    }
}

/// Handle token subcommands
async fn handle_token_command(command: TokenCommands, data_dir: &Path) -> Result<()> {
    let socket_path = management::default_socket_path(data_dir);

    let mut client = ManagementClient::connect(&socket_path).await?;

    match command {
        TokenCommands::Create { expires } => {
            let ttl = parse_duration(&expires)?;
            let expires_secs = ttl.num_seconds() as u64;

            let resp = client.create_token(expires_secs, "cli").await?;

            println!("Registration token created:");
            println!("  Token:   {}", resp.token);
            println!("  Expires: {} (in {})", resp.expires_at, expires);
            println!();
            println!("Copy this token to the agent configuration to register it.");
            println!("Warning: Token is single-use and expires in {expires}. Generate new tokens as needed.");

            Ok(())
        }

        TokenCommands::List => {
            let resp = client.list_tokens().await?;

            if resp.tokens.is_empty() {
                println!("No pending registration tokens.");
                return Ok(());
            }

            println!("{:<25} {:<20} {:<20} {:<10}",
                "TOKEN", "CREATED", "EXPIRES", "CREATED BY");
            println!("{}", "-".repeat(80));

            for token in resp.tokens {
                let token_short = if token.token.len() > 20 {
                    format!("{}...", &token.token[..20])
                } else {
                    token.token.clone()
                };
                // Parse RFC3339 timestamps and format them nicely
                let created = chrono::DateTime::parse_from_rfc3339(&token.created_at)
                    .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
                    .unwrap_or(token.created_at.clone());
                let expires = chrono::DateTime::parse_from_rfc3339(&token.expires_at)
                    .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
                    .unwrap_or(token.expires_at.clone());
                println!("{:<25} {:<20} {:<20} {:<10}",
                    token_short, created, expires, token.created_by);
            }

            Ok(())
        }

        TokenCommands::Revoke { token } => {
            let resp = client.delete_token(&token).await?;
            if resp.deleted {
                println!("Token revoked successfully.");
            } else {
                println!("Token not found.");
            }
            Ok(())
        }
    }
}

/// Handle agent subcommands
async fn handle_agent_command(command: AgentCommands, data_dir: &Path) -> Result<()> {
    let socket_path = management::default_socket_path(data_dir);

    let mut client = ManagementClient::connect(&socket_path).await?;

    match command {
        AgentCommands::List => {
            let resp = client.list_agents().await?;

            if resp.agents.is_empty() {
                println!("No registered agents.");
                return Ok(());
            }

            println!("{:<30} {:<12} {:<15} {:<8} {:<12} {:<12} {:<10}",
                "AGENT ID", "TYPE", "HOSTNAME", "MAX_VMS", "CREATED", "EXPIRES", "STATUS");
            println!("{}", "-".repeat(105));

            for agent in &resp.agents {
                let status = if agent.revoked {
                    "revoked"
                } else {
                    // Parse expires_at to check if expired
                    let expires_dt = chrono::DateTime::parse_from_rfc3339(&agent.expires_at)
                        .map(|dt| dt.with_timezone(&chrono::Utc));
                    if let Ok(exp) = expires_dt {
                        if exp < chrono::Utc::now() {
                            "expired"
                        } else {
                            "valid"
                        }
                    } else {
                        "valid"
                    }
                };
                let id_short = if agent.agent_id.len() > 28 {
                    format!("{}...", &agent.agent_id[..28])
                } else {
                    agent.agent_id.clone()
                };
                let hostname_short = if agent.hostname.len() > 13 {
                    format!("{}...", &agent.hostname[..13])
                } else {
                    agent.hostname.clone()
                };
                // Parse RFC3339 timestamps
                let created = chrono::DateTime::parse_from_rfc3339(&agent.created_at)
                    .map(|dt| dt.format("%Y-%m-%d").to_string())
                    .unwrap_or(agent.created_at.clone());
                let expires = chrono::DateTime::parse_from_rfc3339(&agent.expires_at)
                    .map(|dt| dt.format("%Y-%m-%d").to_string())
                    .unwrap_or(agent.expires_at.clone());
                println!("{:<30} {:<12} {:<15} {:<8} {:<12} {:<12} {:<10}",
                    id_short, agent.agent_type, hostname_short, agent.max_vms, created, expires, status);
            }

            // Show labels in a second pass for readability
            println!();
            println!("Labels:");
            for agent in &resp.agents {
                let id_short = if agent.agent_id.len() > 28 {
                    format!("{}...", &agent.agent_id[..28])
                } else {
                    agent.agent_id.clone()
                };
                println!("  {}: {}", id_short, agent.labels.join(", "));
            }

            Ok(())
        }

        AgentCommands::Revoke { agent_id } => {
            let resp = client.revoke_agent(&agent_id).await?;
            if resp.revoked {
                println!("Agent certificate revoked. Agent will be disconnected and must re-register.");
            } else {
                println!("Agent not found.");
            }
            Ok(())
        }
    }
}

/// Generate a default configuration file
fn generate_config(output: Option<PathBuf>) -> Result<()> {
    let config = config::default_config_template();

    match output {
        Some(path) => {
            std::fs::write(&path, &config)?;
            println!("Configuration written to: {}", path.display());
        }
        None => {
            print!("{config}");
        }
    }

    Ok(())
}

/// Parse a duration string like "1h", "30m", "1d"
fn parse_duration(s: &str) -> Result<Duration> {
    let s = s.trim();
    if s.is_empty() {
        return Err(anyhow!("Empty duration string"));
    }

    let (num_str, unit) = s.split_at(s.len() - 1);
    let num: i64 = num_str.parse().context("Invalid duration number")?;

    match unit {
        "s" => Ok(Duration::seconds(num)),
        "m" => Ok(Duration::minutes(num)),
        "h" => Ok(Duration::hours(num)),
        "d" => Ok(Duration::days(num)),
        _ => Err(anyhow!("Unknown duration unit: {}. Use s, m, h, or d", unit)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_duration() {
        assert_eq!(parse_duration("30s").unwrap(), Duration::seconds(30));
        assert_eq!(parse_duration("5m").unwrap(), Duration::minutes(5));
        assert_eq!(parse_duration("1h").unwrap(), Duration::hours(1));
        assert_eq!(parse_duration("2d").unwrap(), Duration::days(2));

        assert!(parse_duration("").is_err());
        assert!(parse_duration("1x").is_err());
        assert!(parse_duration("abc").is_err());
    }
}
