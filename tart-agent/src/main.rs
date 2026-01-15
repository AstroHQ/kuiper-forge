//! Tart Agent - manages macOS VMs via Tart CLI for CI runners.
//!
//! This daemon runs on each Mac host and:
//! - Connects outbound to the coordinator via gRPC with mTLS
//! - Receives commands to create/destroy runner VMs
//! - Manages VM lifecycle using the Tart CLI
//! - Configures GitHub Actions runners via SSH
//! - Reports status updates to the coordinator
//!
//! # Bootstrap Mode
//!
//! First run with registration token:
//! ```
//! tart-agent --coordinator-url https://coordinator:9443 \
//!            --token reg_xxxxx \
//!            --ca-cert /path/to/ca.crt \
//!            --labels macos,arm64 \
//!            --base-image ghcr.io/cirruslabs/macos-sequoia-base:latest
//! ```
//!
//! Subsequent runs (uses saved config):
//! ```
//! tart-agent
//! ```

mod config;
mod error;
mod ssh;
mod vm_manager;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use agent_lib::{AgentCertStore, AgentConfig, AgentConnector};
use agent_proto::{
    AgentMessage, AgentPayload, AgentStatus, CommandResult, CommandResultPayload,
    CoordinatorPayload, CreateRunnerResult, DestroyRunnerResult, Pong,
};
use clap::Parser;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tracing::{error, info, warn};
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use config::Config;
use error::{Error, Result};
use ssh::SshConfig;
use vm_manager::VmManager;

/// Tart VM Agent for CI Runner Coordination
#[derive(Parser, Debug)]
#[command(name = "tart-agent", version, about, long_about = None)]
struct Args {
    /// Path to configuration file (default: platform-specific config dir)
    #[arg(short, long)]
    config: Option<PathBuf>,

    // === Bootstrap arguments (for first-time registration) ===
    /// Coordinator URL (e.g., https://coordinator:9443)
    /// Required for first-time registration, ignored if config exists
    #[arg(long, requires = "token")]
    coordinator_url: Option<String>,

    /// Registration token from coordinator (single-use)
    #[arg(long, requires = "coordinator_url")]
    token: Option<String>,

    /// Path to CA certificate from coordinator
    #[arg(long, requires = "coordinator_url")]
    ca_cert: Option<PathBuf>,

    /// Labels for this agent (comma-separated)
    #[arg(long, value_delimiter = ',', default_value = "self-hosted,macos,arm64")]
    labels: Vec<String>,

    /// Base VM image for runners
    #[arg(long, default_value = "ghcr.io/cirruslabs/macos-sequoia-base:latest")]
    base_image: String,

    /// Maximum concurrent VMs (default: 2, Apple limit)
    #[arg(long)]
    max_vms: Option<u32>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let config_path = args.config.clone().unwrap_or_else(Config::default_path);

    // Determine data directory for logging
    let data_dir = Config::default_data_dir();

    // Initialize logging with file output
    init_logging(&data_dir)?;

    // Determine if we're in bootstrap mode or normal mode
    let (config, registration_token) = if let Some(coordinator_url) = args.coordinator_url {
        // Bootstrap mode: create config from CLI arguments
        let ca_cert = args.ca_cert.ok_or_else(|| {
            anyhow::anyhow!("--ca-cert is required for bootstrap (get it from coordinator)")
        })?;

        if !ca_cert.exists() {
            anyhow::bail!("CA certificate not found: {}", ca_cert.display());
        }

        info!("Bootstrap mode: registering with coordinator");
        info!("  Coordinator: {}", coordinator_url);
        info!("  Labels: {:?}", args.labels);
        info!("  Base image: {}", args.base_image);

        let config = Config::from_bootstrap(
            coordinator_url,
            ca_cert,
            args.labels,
            args.base_image,
            args.max_vms,
        );

        // Save config for future runs
        config.save(&config_path)?;
        info!("Saved config to: {}", config_path.display());

        (config, args.token)
    } else if config_path.exists() {
        // Normal mode: load existing config
        info!("Loading config from: {}", config_path.display());
        let config = Config::load(&config_path)?;
        (config, None)
    } else {
        // No config and no bootstrap args
        eprintln!("No configuration found at: {}", config_path.display());
        eprintln!();
        eprintln!("To register this agent for the first time, run:");
        eprintln!();
        eprintln!("  tart-agent --coordinator-url https://YOUR_COORDINATOR:9443 \\");
        eprintln!("             --token REG_TOKEN_FROM_COORDINATOR \\");
        eprintln!("             --ca-cert /path/to/ca.crt \\");
        eprintln!("             --labels macos,arm64");
        eprintln!();
        eprintln!("Get the registration token with: coordinator token create --labels macos,arm64");
        eprintln!("Get the CA cert with: coordinator ca export > ca.crt");
        std::process::exit(1);
    };

    info!("tart-agent starting");
    info!("Coordinator: {}", config.coordinator.url);
    info!("Max concurrent VMs: {}", config.tart.max_concurrent_vms);
    info!("Labels: {:?}", config.agent.labels);

    // Initialize VM manager
    let tart_config = config.tart.clone();
    let ssh_config = SshConfig::default();
    let vm_manager = Arc::new(VmManager::new(tart_config, ssh_config));

    // Initialize certificate store
    let cert_store = AgentCertStore::new(config.tls.certs_dir.clone());

    // Copy CA cert to certs_dir if not already there
    let ca_dest = config.tls.certs_dir.join("ca.crt");
    if !ca_dest.exists() && config.tls.ca_cert.exists() {
        std::fs::create_dir_all(&config.tls.certs_dir)?;
        std::fs::copy(&config.tls.ca_cert, &ca_dest)?;
        info!("Copied CA certificate to {}", ca_dest.display());
    }

    // Build agent connector config
    let agent_config = AgentConfig {
        coordinator_url: config.coordinator.url.clone(),
        coordinator_hostname: config.coordinator.hostname.clone(),
        registration_token, // Use token from CLI args (for bootstrap) or None
        agent_type: "tart".to_string(),
        labels: config.agent.labels.clone(),
        max_vms: config.tart.max_concurrent_vms,
    };

    // Create agent instance
    let agent = TartAgent::new(agent_config, cert_store, vm_manager.clone(), config.clone());

    // Spawn cleanup task
    let cleanup_vm_manager = vm_manager.clone();
    let cleanup_config = config.cleanup.clone();
    tokio::spawn(async move {
        let interval = Duration::from_secs(cleanup_config.cleanup_interval_mins as u64 * 60);
        let max_age = Duration::from_secs(cleanup_config.max_vm_age_hours as u64 * 3600);
        let mut ticker = tokio::time::interval(interval);

        loop {
            ticker.tick().await;
            info!("Running stale VM cleanup");
            cleanup_vm_manager.cleanup_stale_vms(max_age).await;
        }
    });

    // Run the agent (with automatic reconnection)
    agent.run().await?;

    Ok(())
}

/// The main Tart agent that manages the connection to the coordinator.
struct TartAgent {
    agent_config: AgentConfig,
    cert_store: AgentCertStore,
    vm_manager: Arc<VmManager>,
    config: Config,
}

impl TartAgent {
    fn new(
        agent_config: AgentConfig,
        cert_store: AgentCertStore,
        vm_manager: Arc<VmManager>,
        config: Config,
    ) -> Self {
        Self {
            agent_config,
            cert_store,
            vm_manager,
            config,
        }
    }

    /// Run the agent, automatically reconnecting on disconnect.
    async fn run(&self) -> Result<()> {
        let mut reconnect_delay = Duration::from_secs(self.config.reconnect.initial_delay_secs);
        let max_delay = Duration::from_secs(self.config.reconnect.max_delay_secs);

        loop {
            let mut connector =
                AgentConnector::new(self.agent_config.clone(), self.cert_store.clone());

            match connector.connect().await {
                Ok(client) => {
                    info!("Connected to coordinator");
                    // Reset reconnect delay on successful connection
                    reconnect_delay = Duration::from_secs(self.config.reconnect.initial_delay_secs);

                    if let Err(e) = self.run_stream(client).await {
                        warn!("Stream error: {}", e);
                    }
                }
                Err(e) => {
                    error!("Connection failed: {}", e);
                }
            }

            // Wait before reconnecting
            info!("Reconnecting in {:?}...", reconnect_delay);
            tokio::time::sleep(reconnect_delay).await;

            // Exponential backoff
            reconnect_delay = std::cmp::min(reconnect_delay * 2, max_delay);
        }
    }

    /// Run the bidirectional gRPC stream.
    async fn run_stream(
        &self,
        mut client: agent_proto::AgentServiceClient<tonic::transport::Channel>,
    ) -> Result<()> {
        // Create channels for sending messages to coordinator
        let (tx, rx) = mpsc::channel::<AgentMessage>(32);

        // Start the bidirectional stream
        let response = client
            .agent_stream(ReceiverStream::new(rx))
            .await?;

        let mut inbound = response.into_inner();

        // Send initial status
        let status = self.build_status().await;
        tx.send(AgentMessage {
            payload: Some(AgentPayload::Status(status)),
        })
        .await
        .map_err(|_| Error::ChannelSend)?;

        info!("Stream established, processing commands...");

        // Process messages from coordinator
        while let Some(msg_result) = inbound.message().await.transpose() {
            match msg_result {
                Ok(msg) => {
                    if let Some(payload) = msg.payload {
                        self.handle_coordinator_message(payload, &tx).await;
                    }
                }
                Err(e) => {
                    error!("Error receiving message: {}", e);
                    return Err(Error::Status(e));
                }
            }
        }

        warn!("Stream closed by coordinator");
        Ok(())
    }

    /// Handle a message from the coordinator.
    async fn handle_coordinator_message(
        &self,
        payload: CoordinatorPayload,
        tx: &mpsc::Sender<AgentMessage>,
    ) {
        match payload {
            CoordinatorPayload::CreateRunner(cmd) => {
                info!(
                    "Received CreateRunner command: vm={}, template={}",
                    cmd.vm_name, cmd.template
                );

                let result = self.handle_create_runner(&cmd).await;

                if let Err(e) = tx
                    .send(AgentMessage {
                        payload: Some(AgentPayload::Result(result)),
                    })
                    .await
                {
                    error!("Failed to send result: {}", e);
                }
            }

            CoordinatorPayload::DestroyRunner(cmd) => {
                info!("Received DestroyRunner command: vm={}", cmd.vm_id);

                let result = self.handle_destroy_runner(&cmd).await;

                if let Err(e) = tx
                    .send(AgentMessage {
                        payload: Some(AgentPayload::Result(result)),
                    })
                    .await
                {
                    error!("Failed to send result: {}", e);
                }
            }

            CoordinatorPayload::Ping(_) => {
                if let Err(e) = tx
                    .send(AgentMessage {
                        payload: Some(AgentPayload::Pong(Pong {})),
                    })
                    .await
                {
                    error!("Failed to send pong: {}", e);
                }
            }
        }
    }

    /// Handle CreateRunner command.
    async fn handle_create_runner(
        &self,
        cmd: &agent_proto::CreateRunnerCommand,
    ) -> CommandResult {
        let command_id = cmd.command_id.clone();
        let vm_name = cmd.vm_name.clone();
        let template = if cmd.template.is_empty() {
            &self.config.tart.base_image
        } else {
            &cmd.template
        };

        let result: std::result::Result<(String, String), Error> = async {
            // 1. Clone and start VM
            let vm_id = self.vm_manager.create_vm(&vm_name, template).await?;

            // 2. Wait for IP and SSH
            let ip = self
                .vm_manager
                .wait_for_ready(&vm_id, Duration::from_secs(120))
                .await?;

            // 3. Configure GitHub runner
            self.vm_manager
                .configure_runner(
                    &vm_id,
                    &cmd.registration_token,
                    &cmd.labels,
                    &cmd.runner_scope_url,
                )
                .await?;

            // 4. Wait for runner to complete (this blocks until the job finishes)
            self.vm_manager.wait_for_runner_exit(&vm_id).await?;

            // 5. Cleanup
            self.vm_manager.destroy_vm(&vm_id).await?;

            Ok((vm_id, ip.to_string()))
        }
        .await;

        match result {
            Ok((vm_id, ip)) => {
                info!("CreateRunner completed: vm={}, ip={}", vm_id, ip);
                CommandResult {
                    command_id,
                    success: true,
                    error: String::new(),
                    result: Some(CommandResultPayload::CreateRunner(CreateRunnerResult {
                        vm_id,
                        ip_address: ip,
                    })),
                }
            }
            Err(e) => {
                error!("CreateRunner failed: {}", e);
                // Attempt cleanup on failure
                let _ = self.vm_manager.destroy_vm(&vm_name).await;

                CommandResult {
                    command_id,
                    success: false,
                    error: e.to_string(),
                    result: None,
                }
            }
        }
    }

    /// Handle DestroyRunner command.
    async fn handle_destroy_runner(
        &self,
        cmd: &agent_proto::DestroyRunnerCommand,
    ) -> CommandResult {
        let command_id = cmd.command_id.clone();
        let vm_id = cmd.vm_id.clone();

        match self.vm_manager.destroy_vm(&vm_id).await {
            Ok(()) => {
                info!("DestroyRunner completed: vm={}", vm_id);
                CommandResult {
                    command_id,
                    success: true,
                    error: String::new(),
                    result: Some(CommandResultPayload::DestroyRunner(DestroyRunnerResult {})),
                }
            }
            Err(e) => {
                error!("DestroyRunner failed: {}", e);
                CommandResult {
                    command_id,
                    success: false,
                    error: e.to_string(),
                    result: None,
                }
            }
        }
    }

    /// Build current agent status.
    async fn build_status(&self) -> AgentStatus {
        let hostname = gethostname::gethostname()
            .to_string_lossy()
            .to_string();

        AgentStatus {
            active_vms: self.vm_manager.active_count().await as u32,
            available_slots: self.vm_manager.available_slots().await,
            vms: self.vm_manager.get_vms().await,
            // Identity fields - required for first message
            agent_id: self.cert_store.get_agent_id().unwrap_or_default(),
            hostname,
            agent_type: self.agent_config.agent_type.clone(),
            labels: self.agent_config.labels.clone(),
            max_vms: self.agent_config.max_vms,
        }
    }
}

/// Initialize logging with file output and stdout.
fn init_logging(data_dir: &PathBuf) -> anyhow::Result<()> {
    let log_dir = data_dir.join("logs");
    std::fs::create_dir_all(&log_dir)?;

    // Create a daily rotating file appender (e.g., tart-agent.2026-01-15.log)
    let file_appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("tart-agent")
        .filename_suffix("log")
        .build(&log_dir)?;

    // Non-blocking writer for the file
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    // Leak the guard to keep the writer alive for the lifetime of the program
    std::mem::forget(_guard);

    let filter = EnvFilter::from_default_env()
        .add_directive(tracing::Level::INFO.into());

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_target(false)) // stdout
        .with(fmt::layer().with_target(true).with_ansi(false).with_writer(non_blocking)) // file
        .init();

    info!("Logging to: {}", log_dir.display());
    Ok(())
}
