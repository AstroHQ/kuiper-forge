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
//! kuiper-tart-agent --coordinator-url https://coordinator:9443 \
//!            --token reg_xxxxx \
//!            --ca-cert /path/to/ca.crt \
//!            --labels macos,arm64 \
//!            --base-image ghcr.io/cirruslabs/macos-sequoia-base:latest
//! ```
//!
//! Subsequent runs (uses saved config):
//! ```
//! kuiper-tart-agent
//! ```

mod config;
mod error;
mod ssh;
mod vm_manager;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use kuiper_agent_lib::{AgentCertStore, AgentConfig, AgentConnector, RegistrationTlsMode};
use kuiper_agent_proto::{
    AgentMessage, AgentPayload, AgentStatus, CommandAck, CoordinatorPayload, Pong, RunnerEvent,
    RunnerEventType,
};
use clap::Parser;
use tokio::signal;
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
#[command(name = "kuiper-tart-agent", version, about, long_about = None)]
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
    let (config, registration_token, use_provided_ca) = if let Some(coordinator_url) = args.coordinator_url {
        // Bootstrap mode: create config from CLI arguments
        // CA cert is optional - if not provided, use TOFU (insecure) mode
        let ca_cert = args.ca_cert.clone();

        if let Some(ref ca) = ca_cert {
            if !ca.exists() {
                anyhow::bail!("CA certificate not found: {}", ca.display());
            }
            info!("Bootstrap mode: registering with coordinator (using provided CA)");
        } else {
            info!("Bootstrap mode: registering with coordinator (TOFU mode - no CA verification)");
        }

        info!("  Coordinator: {}", coordinator_url);
        info!("  Labels: {:?}", args.labels);
        info!("  Base image: {}", args.base_image);

        let config = Config::from_bootstrap(
            coordinator_url,
            ca_cert.clone(),
            args.labels,
            args.base_image,
            args.max_vms,
        );

        // Save config for future runs
        config.save(&config_path)?;
        info!("Saved config to: {}", config_path.display());

        (config, args.token, ca_cert.is_some())
    } else if config_path.exists() {
        // Normal mode: load existing config
        info!("Loading config from: {}", config_path.display());
        let config = Config::load(&config_path)?;
        let has_ca = config.tls.ca_cert.is_some();
        (config, None, has_ca)
    } else {
        // No config and no bootstrap args
        eprintln!("No configuration found at: {}", config_path.display());
        eprintln!();
        eprintln!("To register this agent for the first time, run:");
        eprintln!();
        eprintln!("  kuiper-tart-agent --coordinator-url https://YOUR_COORDINATOR:9443 \\");
        eprintln!("             --token REG_TOKEN_FROM_COORDINATOR \\");
        eprintln!("             --labels macos,arm64");
        eprintln!();
        eprintln!("Optionally provide --ca-cert for stricter TLS verification.");
        eprintln!("Get the registration token with: coordinator token create --labels macos,arm64");
        std::process::exit(1);
    };

    info!("kuiper-tart-agent starting");
    info!("Coordinator: {}", config.coordinator.url);
    info!("Max concurrent VMs: {}", config.tart.max_concurrent_vms);
    info!("Labels: {:?}", config.agent.labels);

    // Initialize VM manager with SSH config from file
    let tart_config = config.tart.clone();
    let ssh_config = SshConfig::from_config(&config.tart.ssh);
    let log_dir = data_dir.join("logs");
    let vm_manager = Arc::new(VmManager::new(tart_config, ssh_config, log_dir));

    // Initialize certificate store
    let cert_store = AgentCertStore::new(config.tls.certs_dir.clone());

    // Copy CA cert to certs_dir if provided
    if let Some(ref ca_cert_path) = config.tls.ca_cert {
        let ca_dest = config.tls.certs_dir.join("ca.crt");
        if !ca_dest.exists() && ca_cert_path.exists() {
            std::fs::create_dir_all(&config.tls.certs_dir)?;
            std::fs::copy(ca_cert_path, &ca_dest)?;
            info!("Copied CA certificate to {}", ca_dest.display());
        }
    }

    // Determine TLS mode based on whether CA cert was provided
    let registration_tls_mode = if use_provided_ca {
        RegistrationTlsMode::ProvidedCa
    } else {
        RegistrationTlsMode::Insecure
    };

    // Build agent connector config
    let agent_config = AgentConfig {
        coordinator_url: config.coordinator.url.clone(),
        coordinator_hostname: config.coordinator.hostname.clone(),
        registration_token, // Use token from CLI args (for bootstrap) or None
        agent_type: "tart".to_string(),
        labels: config.agent.labels.clone(),
        max_vms: config.tart.max_concurrent_vms,
        registration_tls_mode,
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

    // Clone vm_manager for shutdown cleanup
    let shutdown_vm_manager = vm_manager.clone();

    // Run the agent with graceful shutdown handling
    tokio::select! {
        result = agent.run() => {
            // Agent loop exited (shouldn't happen normally)
            result?;
        }
        _ = shutdown_signal() => {
            info!("Shutdown signal received, cleaning up VMs...");
            shutdown_vm_manager.destroy_all_vms().await;
            info!("Graceful shutdown complete");
        }
    }

    Ok(())
}

/// The main Tart agent that manages the connection to the coordinator.
struct TartAgent {
    agent_config: AgentConfig,
    cert_store: AgentCertStore,
    vm_manager: Arc<VmManager>,
    config: Config,
}

async fn send_command_ack(
    tx: &mpsc::Sender<AgentMessage>,
    command_id: String,
    accepted: bool,
    error: String,
) {
    if let Err(e) = tx
        .send(AgentMessage {
            payload: Some(AgentPayload::Ack(CommandAck {
                command_id,
                accepted,
                error,
            })),
        })
        .await
    {
        error!("Failed to send command ack: {}", e);
    }
}

async fn send_runner_event(
    tx: &mpsc::Sender<AgentMessage>,
    runner_name: String,
    vm_id: String,
    event_type: RunnerEventType,
    error: String,
) {
    if let Err(e) = tx
        .send(AgentMessage {
            payload: Some(AgentPayload::RunnerEvent(RunnerEvent {
                runner_name,
                vm_id,
                event_type: event_type as i32,
                error,
            })),
        })
        .await
    {
        error!("Failed to send runner event: {}", e);
    }
}

impl TartAgent {
    fn new(
        agent_config: AgentConfig,
        cert_store: AgentCertStore,
        vm_manager: Arc<VmManager>,
        config: Config,
    ) -> Arc<Self> {
        Arc::new(Self {
            agent_config,
            cert_store,
            vm_manager,
            config,
        })
    }

    /// Run the agent, automatically reconnecting on disconnect.
    async fn run(self: &Arc<Self>) -> Result<()> {
        let mut reconnect_delay = Duration::from_secs(self.config.reconnect.initial_delay_secs);
        let max_delay = Duration::from_secs(self.config.reconnect.max_delay_secs);

        loop {
            let mut connector =
                AgentConnector::new(self.agent_config.clone(), self.cert_store.clone());

            let connect_result = connector.connect().await;
            match connect_result {
                Ok(client) => {
                    info!("Connected to coordinator");
                    // Reset reconnect delay on successful connection
                    reconnect_delay = Duration::from_secs(self.config.reconnect.initial_delay_secs);

                    let stream_result = self.run_stream(client).await;
                    if let Err(e) = stream_result {
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
        self: &Arc<Self>,
        mut client: kuiper_agent_proto::AgentServiceClient<tonic::transport::Channel>,
    ) -> Result<()> {
        // Create channels for sending messages to coordinator
        let (tx, rx) = mpsc::channel::<AgentMessage>(32);

        // Build initial status message BEFORE starting stream
        // The server expects the first message to identify the agent
        let status = self.build_status().await;
        tx.send(AgentMessage {
            payload: Some(AgentPayload::Status(status)),
        })
        .await
        .map_err(|_| Error::ChannelSend)?;

        // Start the bidirectional stream (server will read our initial status)
        let response = client
            .agent_stream(ReceiverStream::new(rx))
            .await?;

        let mut inbound = response.into_inner();

        info!("Stream established, processing commands...");

        // Spawn a task to send periodic status updates
        // This is critical for recovery: when coordinator restarts, it needs to know
        // when VMs complete so it can clean up the associated runners from GitHub
        let status_tx = tx.clone();
        let status_agent = Arc::clone(self);
        let status_handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            // Skip the first tick since we already sent initial status
            interval.tick().await;

            loop {
                interval.tick().await;
                let status = status_agent.build_status().await;
                if status_tx
                    .send(AgentMessage {
                        payload: Some(AgentPayload::Status(status)),
                    })
                    .await
                    .is_err()
                {
                    // Channel closed, stream is ending
                    break;
                }
            }
        });

        // Process messages from coordinator
        loop {
            let msg_result = inbound.message().await.transpose();
            match msg_result {
                Some(Ok(msg)) => {
                    if let Some(payload) = msg.payload {
                        // This spawns tasks for long-running commands, doesn't block
                        self.handle_coordinator_message(payload, &tx);
                    }
                }
                Some(Err(e)) => {
                    error!("Error receiving message: {}", e);
                    status_handle.abort();
                    return Err(Error::Status(e));
                }
                None => break,
            }
        }

        // Clean up status task when stream closes
        status_handle.abort();

        warn!("Stream closed by coordinator");
        Ok(())
    }

    /// Handle a message from the coordinator.
    ///
    /// Long-running commands (CreateRunner, DestroyRunner) are spawned as separate tasks
    /// so we can continue processing messages (like Ping) without blocking.
    fn handle_coordinator_message(
        self: &Arc<Self>,
        payload: CoordinatorPayload,
        tx: &mpsc::Sender<AgentMessage>,
    ) {
        match payload {
            CoordinatorPayload::CreateRunner(cmd) => {
                info!("Received CreateRunner command: vm={}", cmd.vm_name);

                // Spawn as a separate task so we don't block the message loop
                let agent = Arc::clone(self);
                let tx = tx.clone();
                tokio::spawn(async move {
                    send_command_ack(&tx, cmd.command_id.clone(), true, String::new()).await;
                    agent.handle_create_runner(cmd, &tx).await;
                });
            }

            CoordinatorPayload::DestroyRunner(cmd) => {
                info!("Received DestroyRunner command: vm={}", cmd.vm_id);

                // Spawn as a separate task
                let agent = Arc::clone(self);
                let tx = tx.clone();
                tokio::spawn(async move {
                    send_command_ack(&tx, cmd.command_id.clone(), true, String::new()).await;
                    agent.handle_destroy_runner(cmd, &tx).await;
                });
            }

            CoordinatorPayload::Ping(_) => {
                // Pings are quick, handle inline
                let tx = tx.clone();
                let agent = Arc::clone(self);
                tokio::spawn(async move {
                    if let Err(e) = tx
                        .send(AgentMessage {
                            payload: Some(AgentPayload::Pong(Pong {})),
                        })
                        .await
                    {
                        error!("Failed to send pong: {}", e);
                    }
                    let status = agent.build_status().await;
                    if let Err(e) = tx
                        .send(AgentMessage {
                            payload: Some(AgentPayload::Status(status)),
                        })
                        .await
                    {
                        error!("Failed to send status update: {}", e);
                    }
                });
            }
        }
    }

    /// Select the appropriate VM image based on job labels.
    ///
    /// Iterates through `image_mappings` in order and returns the image from the first
    /// mapping where ALL mapping labels are present in the job labels (case-insensitive).
    /// Falls back to `base_image` if no mapping matches.
    fn select_image(&self, job_labels: &[String]) -> String {
        let job_labels_lower: HashSet<String> = job_labels
            .iter()
            .map(|l| l.to_lowercase())
            .collect();

        for mapping in &self.config.tart.image_mappings {
            let all_match = mapping
                .labels
                .iter()
                .all(|ml| job_labels_lower.contains(&ml.to_lowercase()));
            if all_match {
                info!(
                    "Selected image '{}' for labels {:?} (matched mapping labels {:?})",
                    mapping.image, job_labels, mapping.labels
                );
                return mapping.image.clone();
            }
        }

        // Fallback to default
        info!(
            "No image mapping matched labels {:?}, using default '{}'",
            job_labels, self.config.tart.base_image
        );
        self.config.tart.base_image.clone()
    }

    /// Handle CreateRunner command.
    async fn handle_create_runner(
        &self,
        cmd: kuiper_agent_proto::CreateRunnerCommand,
        tx: &mpsc::Sender<AgentMessage>,
    ) {
        let vm_name = cmd.vm_name.clone();

        // Validate inputs early and warn about suspicious values
        if cmd.registration_token.is_empty() {
            warn!(
                "CreateRunner called with empty registration token - \
                 runner configuration will fail. Is the coordinator in dry-run mode?"
            );
        }
        if cmd.runner_scope_url.is_empty() {
            warn!("CreateRunner called with empty runner_scope_url");
        }

        info!(
            "CreateRunner starting: vm={}, labels={:?}, scope={}",
            vm_name,
            cmd.labels,
            if cmd.runner_scope_url.is_empty() {
                "(empty)"
            } else {
                &cmd.runner_scope_url
            }
        );

        // Select image based on job labels
        let selected_image = self.select_image(&cmd.labels);

        let result: std::result::Result<(String, String), Error> = async {
            // 1. Clone and start VM from selected image
            info!(
                "[{}] Step 1/5: Creating VM from template '{}'...",
                vm_name, selected_image
            );
            let vm_id = self
                .vm_manager
                .create_vm(&vm_name, &selected_image)
                .await
                .map_err(|e| {
                    error!("[{}] VM creation failed: {}", vm_name, e);
                    e
                })?;

            // 2. Wait for IP and SSH
            info!("[{}] Step 2/5: Waiting for VM to be ready...", vm_name);
            let ip = self
                .vm_manager
                .wait_for_ready(&vm_id, Duration::from_secs(120))
                .await
                .map_err(|e| {
                    error!("[{}] VM failed to become ready: {}", vm_name, e);
                    e
                })?;
            info!("[{}] VM ready at IP {}", vm_name, ip);

            // 3. Configure GitHub runner
            info!("[{}] Step 3/5: Configuring GitHub runner...", vm_name);
            self.vm_manager
                .configure_runner(&vm_id, &cmd.registration_token, &cmd.labels, &cmd.runner_scope_url)
                .await
                .map_err(|e| {
                    error!(
                        "[{}] Runner configuration failed: {}. \
                         This typically indicates an invalid GitHub registration token.",
                        vm_name, e
                    );
                    e
                })?;

            send_runner_event(
                tx,
                vm_name.clone(),
                vm_id.clone(),
                RunnerEventType::Started,
                String::new(),
            )
            .await;

            // 4. Wait for runner to complete (this blocks until the job finishes)
            info!(
                "[{}] Step 4/5: Runner configured, waiting for job completion...",
                vm_name
            );
            self.vm_manager.wait_for_runner_exit(&vm_id).await.map_err(|e| {
                error!("[{}] Runner execution failed: {}", vm_name, e);
                e
            })?;

            // 5. Cleanup
            info!("[{}] Step 5/5: Cleaning up VM...", vm_name);
            self.vm_manager.destroy_vm(&vm_id).await?;

            Ok((vm_id, ip.to_string()))
        }
        .await;

        match result {
            Ok((vm_id, ip)) => {
                info!(
                    "CreateRunner completed successfully: vm={}, ip={}",
                    vm_id, ip
                );
                send_runner_event(
                    tx,
                    vm_name,
                    vm_id,
                    RunnerEventType::Completed,
                    String::new(),
                )
                .await;
            }
            Err(e) => {
                error!("CreateRunner FAILED for vm={}: {}", vm_name, e);
                // Attempt cleanup on failure
                info!("[{}] Attempting cleanup after failure...", vm_name);
                if let Err(cleanup_err) = self.vm_manager.destroy_vm(&vm_name).await {
                    warn!(
                        "[{}] Cleanup after failure also failed: {}",
                        vm_name, cleanup_err
                    );
                }
                send_runner_event(
                    tx,
                    vm_name.clone(),
                    vm_name,
                    RunnerEventType::Failed,
                    e.to_string(),
                )
                .await;
            }
        }
    }

    /// Handle DestroyRunner command.
    async fn handle_destroy_runner(
        &self,
        cmd: kuiper_agent_proto::DestroyRunnerCommand,
        tx: &mpsc::Sender<AgentMessage>,
    ) {
        let vm_id = cmd.vm_id.clone();

        match self.vm_manager.destroy_vm(&vm_id).await {
            Ok(()) => {
                info!("DestroyRunner completed: vm={}", vm_id);
                send_runner_event(
                    tx,
                    vm_id.clone(),
                    vm_id,
                    RunnerEventType::Destroyed,
                    String::new(),
                )
                .await;
            }
            Err(e) => {
                error!("DestroyRunner failed: {}", e);
                send_runner_event(
                    tx,
                    vm_id.clone(),
                    vm_id,
                    RunnerEventType::Failed,
                    e.to_string(),
                )
                .await;
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
fn init_logging(data_dir: &Path) -> anyhow::Result<()> {
    let log_dir = data_dir.join("logs");
    std::fs::create_dir_all(&log_dir)?;

    // Create a daily rotating file appender (e.g., kuiper-tart-agent.2026-01-15.log)
    let file_appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("kuiper-tart-agent")
        .filename_suffix("log")
        .build(&log_dir)?;

    // Non-blocking writer for the file
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    // Leak the guard to keep the writer alive for the lifetime of the program
    std::mem::forget(_guard);

    // Base filter suppresses noisy libraries, RUST_LOG layers on top (can override if explicit)
    let base = "russh=warn,hyper=warn,reqwest=warn,h2=warn,rustls=warn,tonic=warn";
    let filter = match std::env::var("RUST_LOG") {
        Ok(env) => EnvFilter::new(format!("{base},{env}")),
        Err(_) => EnvFilter::new(format!("{base},info")),
    };

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_target(false)) // stdout
        .with(fmt::layer().with_target(true).with_ansi(false).with_writer(non_blocking)) // file
        .init();

    info!("Logging to: {}", log_dir.display());
    Ok(())
}

/// Wait for a shutdown signal (SIGTERM or SIGINT/Ctrl-C).
async fn shutdown_signal() {
    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl-C handler");
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
        _ = ctrl_c => {
            info!("Received Ctrl-C");
        }
        _ = terminate => {
            info!("Received SIGTERM");
        }
    }
}
