//! CI Runner Coordinator - Main entry point
//!
//! The coordinator daemon manages ephemeral GitHub Actions runners
//! across multiple VM providers (Tart for macOS, Proxmox for Windows/Linux).

use anyhow::{anyhow, Context, Result};
use chrono::Duration;
use clap::{Parser, Subcommand};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{debug, info, warn, Level};
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use kuiper_forge::agent_registry::AgentRegistry;
use kuiper_forge::auth::{export_ca_cert, generate_server_cert, init_ca, AuthManager, AuthStore};
use kuiper_forge::config::{self, Config, ProvisioningMode};
use kuiper_forge::fleet;
use kuiper_forge::github;
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

    /// Enable verbose logging
    #[arg(short, long)]
    verbose: bool,

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
    let filter = if cli.verbose {
        EnvFilter::from_default_env().add_directive(Level::DEBUG.into())
    } else {
        EnvFilter::from_default_env().add_directive(Level::INFO.into())
    };

    match cli.command {
        Commands::Serve { listen, dry_run } => {
            // For daemon mode: log to both stdout and file with rotation
            init_daemon_logging(&cli.data_dir, filter)?;
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
fn init_daemon_logging(data_dir: &PathBuf, filter: EnvFilter) -> Result<()> {
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
async fn serve(config_path: &PathBuf, data_dir: &PathBuf, listen_override: Option<SocketAddr>, dry_run: bool) -> Result<()> {
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

    // Initialize auth store and manager (file-backed for persistence)
    let auth_store_path = data_dir.join("auth_store.json");
    let auth_store = Arc::new(AuthStore::new(&auth_store_path)?);
    let auth_manager = Arc::new(AuthManager::new(
        auth_store,
        &config.tls.ca_cert,
        &config.tls.ca_key,
    )?);

    // Initialize agent registry
    let agent_registry = Arc::new(AgentRegistry::new());

    // Initialize persistent runner state for crash recovery
    let runner_state = Arc::new(runner_state::RunnerStateStore::new(&data_dir));

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
        );
        (Some(fm), Some(notifier), wh_notifier)
    } else {
        let github_client = github::GitHubClient::new(
            config.github.app_id.clone(),
            &config.github.private_key_path,
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

    // Run fleet manager (if enabled) and server concurrently
    if let Some(fm) = fleet_manager {
        tokio::select! {
            result = run_server(server_config, auth_manager, agent_registry, fleet_notifier, Some(runner_state), webhook_config) => {
                result
            }
            _ = fm.run() => {
                // Fleet manager runs forever, shouldn't exit
                Ok(())
            }
        }
    } else {
        // No fleet manager, just run the server
        run_server(server_config, auth_manager, agent_registry, None, None, None).await
    }
}

/// Ensure data directory exists
fn ensure_data_dir(data_dir: &PathBuf) -> Result<()> {
    if !data_dir.exists() {
        std::fs::create_dir_all(data_dir)
            .with_context(|| format!("Failed to create data directory: {}", data_dir.display()))?;
        info!("Created data directory: {}", data_dir.display());
    }
    Ok(())
}

/// Handle CA subcommands
async fn handle_ca_command(command: CaCommands, data_dir: &PathBuf) -> Result<()> {
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
            print!("{}", cert);
            Ok(())
        }
    }
}

/// Handle token subcommands
async fn handle_token_command(command: TokenCommands, data_dir: &PathBuf) -> Result<()> {
    ensure_data_dir(data_dir)?;

    // Use the shared file-backed auth store
    let auth_store_path = data_dir.join("auth_store.json");
    let auth_store = Arc::new(AuthStore::new(&auth_store_path)?);

    let ca_cert_path = data_dir.join("ca.crt");
    let ca_key_path = data_dir.join("ca.key");

    let auth_manager = AuthManager::new(
        auth_store.clone(),
        &ca_cert_path,
        &ca_key_path,
    )?;

    match command {
        TokenCommands::Create { expires } => {
            let ttl = parse_duration(&expires)?;

            let token = auth_manager
                .create_registration_token(ttl, "cli")
                .await?;

            println!("Registration token created:");
            println!("  Token:   {}", token.token);
            println!("  Expires: {} (in {})", token.expires_at.format("%Y-%m-%d %H:%M:%S UTC"), expires);
            println!();
            println!("Copy this token to the agent configuration to register it.");
            println!("Warning: Token is single-use and expires in {}. Generate new tokens as needed.", expires);

            Ok(())
        }

        TokenCommands::List => {
            let tokens = auth_manager.list_tokens().await;

            if tokens.is_empty() {
                println!("No pending registration tokens.");
                return Ok(());
            }

            println!("{:<25} {:<20} {:<20} {:<10}",
                "TOKEN", "CREATED", "EXPIRES", "CREATED BY");
            println!("{}", "-".repeat(80));

            for token in tokens {
                let created = token.created_at.format("%Y-%m-%d %H:%M");
                let expires = token.expires_at.format("%Y-%m-%d %H:%M");
                let token_short = if token.token.len() > 20 {
                    format!("{}...", &token.token[..20])
                } else {
                    token.token.clone()
                };
                println!("{:<25} {:<20} {:<20} {:<10}",
                    token_short, created, expires, token.created_by);
            }

            Ok(())
        }

        TokenCommands::Revoke { token } => {
            if auth_manager.delete_token(&token).await? {
                println!("Token revoked successfully.");
            } else {
                println!("Token not found.");
            }
            Ok(())
        }
    }
}

/// Handle agent subcommands
async fn handle_agent_command(command: AgentCommands, data_dir: &PathBuf) -> Result<()> {
    ensure_data_dir(data_dir)?;

    // Use the shared file-backed auth store
    let auth_store_path = data_dir.join("auth_store.json");
    let auth_store = Arc::new(AuthStore::new(&auth_store_path)?);

    let ca_cert_path = data_dir.join("ca.crt");
    let ca_key_path = data_dir.join("ca.key");

    let auth_manager = AuthManager::new(
        auth_store,
        &ca_cert_path,
        &ca_key_path,
    )?;

    match command {
        AgentCommands::List => {
            let agents = auth_manager.list_agents().await;

            if agents.is_empty() {
                println!("No registered agents.");
                return Ok(());
            }

            println!("{:<30} {:<12} {:<15} {:<8} {:<12} {:<12} {:<10}",
                "AGENT ID", "TYPE", "HOSTNAME", "MAX_VMS", "CREATED", "EXPIRES", "STATUS");
            println!("{}", "-".repeat(105));

            for agent in agents {
                let status = if agent.revoked {
                    "revoked"
                } else if agent.expires_at < chrono::Utc::now() {
                    "expired"
                } else {
                    "valid"
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
                let created = agent.created_at.format("%Y-%m-%d");
                let expires = agent.expires_at.format("%Y-%m-%d");
                println!("{:<30} {:<12} {:<15} {:<8} {:<12} {:<12} {:<10}",
                    id_short, agent.agent_type, hostname_short, agent.max_vms, created, expires, status);
            }

            // Show labels in a second pass for readability
            println!();
            println!("Labels:");
            for agent in auth_manager.list_agents().await {
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
            if auth_manager.revoke_agent(&agent_id).await? {
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
            print!("{}", config);
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
