//! Proxmox Agent - manages Windows/Linux VMs via Proxmox API for CI runners.
//!
//! This daemon:
//! - Connects to the coordinator via gRPC + mTLS
//! - Receives commands to create/destroy runner VMs
//! - Manages the full VM lifecycle (clone, start, configure runner, wait, destroy)
//! - Reports status back to the coordinator

mod config;
mod error;
mod ssh;
mod vm_manager;

use kuiper_agent_lib::{AgentCertStore, AgentConfig as LibAgentConfig, AgentConnector};
use kuiper_agent_proto::{
    AgentMessage, AgentPayload, AgentStatus, CommandAck, CoordinatorPayload, Ping, Pong,
    RunnerEvent, RunnerEventType, VmInfo,
};
use clap::Parser;
use config::Config;
use error::{Error, Result};
use kuiper_proxmox_api::{ProxmoxAuth, ProxmoxVEAPI};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tracing::{debug, error, info};
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};
use vm_manager::VmManager;

/// Proxmox Agent for CI Runner Coordination
#[derive(Parser, Debug)]
#[command(name = "kuiper-proxmox-agent")]
#[command(about = "Proxmox VE agent for ephemeral CI runner VMs")]
struct Args {
    /// Path to configuration file
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Registration token from coordinator (single-use, for first-time registration)
    #[arg(long)]
    token: Option<String>,

    /// Enable verbose logging
    #[arg(short, long)]
    verbose: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    // Initialize logging with file output
    let data_dir = Config::default_data_dir();
    init_logging(&data_dir, args.verbose)?;

    info!("Starting kuiper-proxmox-agent");

    // Load configuration
    let config = match &args.config {
        Some(path) => Config::load(path)?,
        None => Config::load_default()?,
    };

    info!("Loaded configuration");
    debug!("Coordinator URL: {}", config.coordinator.url);
    debug!("Proxmox node: {}", config.proxmox.node);
    debug!("Template VMID: {}", config.vm.template_vmid);
    debug!("Max concurrent VMs: {}", config.vm.concurrent_vms);

    // Create Proxmox API client
    let proxmox = create_proxmox_client(&config)?;
    let proxmox = Arc::new(proxmox);

    // Create VM manager
    let vm_manager = Arc::new(VmManager::new(
        proxmox,
        config.vm.clone(),
        config.ssh.clone(),
    ));

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

    // Initialize certificate store
    let cert_store = AgentCertStore::new(config.tls.certs_dir.clone());

    // Copy CA cert to certs_dir if it doesn't exist there
    let ca_dest = config.tls.certs_dir.join("ca.crt");
    if !ca_dest.exists() && config.tls.ca_cert.exists() {
        std::fs::create_dir_all(&config.tls.certs_dir)?;
        std::fs::copy(&config.tls.ca_cert, &ca_dest)?;
        info!("Copied CA certificate to {:?}", ca_dest);
    }

    // Clone vm_manager for shutdown cleanup
    let shutdown_vm_manager = vm_manager.clone();

    // Create agent runner
    let agent = ProxmoxAgent::new(config, vm_manager, cert_store, args.token);

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

/// Wait for a shutdown signal (SIGTERM or SIGINT/Ctrl-C).
async fn shutdown_signal() {
    use tokio::signal;

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

/// Create the Proxmox API client from configuration.
fn create_proxmox_client(config: &Config) -> Result<ProxmoxVEAPI> {
    let auth = ProxmoxAuth::api_token(&config.proxmox.token_id, &config.proxmox.token_secret);

    // The username is derived from token_id (format: user@realm!tokenname)
    let username = config.proxmox.token_id
        .split('!')
        .next()
        .unwrap_or(&config.proxmox.token_id);

    ProxmoxVEAPI::with_node(
        username,
        auth,
        &config.proxmox.api_url,
        &config.proxmox.node,
        config.proxmox.accept_invalid_certs,
    ).map_err(|e| Error::Vm(format!("Failed to create Proxmox client: {}", e)))
}

/// The main Proxmox agent.
struct ProxmoxAgent {
    config: Config,
    vm_manager: Arc<VmManager>,
    cert_store: AgentCertStore,
    /// Registration token from CLI (for first-time registration)
    registration_token: Option<String>,
}

async fn send_command_ack(
    tx: &mpsc::Sender<AgentMessage>,
    command_id: String,
    accepted: bool,
    error: String,
) -> Result<()> {
    tx.send(AgentMessage {
        payload: Some(AgentPayload::Ack(CommandAck {
            command_id,
            accepted,
            error,
        })),
    })
    .await
    .map_err(|_| Error::ChannelSend)
}

async fn send_runner_event(
    tx: &mpsc::Sender<AgentMessage>,
    runner_name: String,
    vm_id: String,
    event_type: RunnerEventType,
    error: String,
) -> Result<()> {
    tx.send(AgentMessage {
        payload: Some(AgentPayload::RunnerEvent(RunnerEvent {
            runner_name,
            vm_id,
            event_type: event_type as i32,
            error,
        })),
    })
    .await
    .map_err(|_| Error::ChannelSend)
}

impl ProxmoxAgent {
    /// Create a new Proxmox agent.
    fn new(
        config: Config,
        vm_manager: Arc<VmManager>,
        cert_store: AgentCertStore,
        registration_token: Option<String>,
    ) -> Arc<Self> {
        Arc::new(Self {
            config,
            vm_manager,
            cert_store,
            registration_token,
        })
    }

    /// Run the agent, handling reconnection.
    async fn run(self: &Arc<Self>) -> Result<()> {
        loop {
            match self.connect_and_run().await {
                Ok(_) => {
                    info!("Connection closed, reconnecting...");
                }
                Err(Error::Shutdown) => {
                    info!("Shutting down");
                    return Ok(());
                }
                Err(e) => {
                    error!("Agent error: {}", e);
                }
            }

            // Wait before reconnecting
            info!("Reconnecting in 5 seconds...");
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }

    /// Connect to coordinator and run the agent loop.
    async fn connect_and_run(self: &Arc<Self>) -> Result<()> {
        // Create agent config for connector
        // Use registration token from CLI args (for first-time registration)
        let agent_config = LibAgentConfig {
            coordinator_url: self.config.coordinator.url.clone(),
            coordinator_hostname: self.config.coordinator.hostname.clone(),
            registration_token: self.registration_token.clone(),
            agent_type: "proxmox".to_string(),
            labels: self.config.agent.labels.clone(),
            max_vms: self.config.vm.concurrent_vms,
            registration_tls_mode: Default::default(), // Insecure (TOFU)
        };

        // Connect to coordinator using stored cert_store
        let mut connector = AgentConnector::new(agent_config, self.cert_store.clone());
        let client = connector.connect().await?;

        info!("Connected to coordinator");
        if let Some(agent_id) = connector.agent_id() {
            info!("Agent ID: {}", agent_id);
        }

        // Run the agent stream
        self.run_stream(client).await
    }

    /// Run the bidirectional gRPC stream.
    async fn run_stream(
        self: &Arc<Self>,
        mut client: kuiper_agent_proto::AgentServiceClient<tonic::transport::Channel>,
    ) -> Result<()> {
        // Create channels for sending/receiving
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
            .await
            .map_err(|e| Error::Grpc(e))?;

        let mut inbound = response.into_inner();

        info!("Sent initial status to coordinator");

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
        while let Some(msg) = inbound.message().await.transpose() {
            match msg {
                Ok(coordinator_msg) => {
                    self.handle_coordinator_message(coordinator_msg, &tx).await?;
                }
                Err(e) => {
                    error!("Stream error: {}", e);
                    status_handle.abort();
                    return Err(Error::Grpc(e));
                }
            }
        }
        info!("Stream closed by coordinator");

        // Clean up status task when stream closes
        status_handle.abort();

        Ok(())
    }

    /// Handle a message from the coordinator.
    async fn handle_coordinator_message(
        &self,
        msg: kuiper_agent_proto::CoordinatorMessage,
        tx: &mpsc::Sender<AgentMessage>,
    ) -> Result<()> {
        let payload = match msg.payload {
            Some(p) => p,
            None => {
                debug!("Received empty message from coordinator");
                return Ok(());
            }
        };

        match payload {
            CoordinatorPayload::CreateRunner(cmd) => {
                info!(
                    "Received CreateRunner command: {} ({})",
                    cmd.command_id, cmd.vm_name
                );
                send_command_ack(&tx, cmd.command_id.clone(), true, String::new()).await?;
                self.handle_create_runner(cmd, tx.clone()).await;
            }
            CoordinatorPayload::DestroyRunner(cmd) => {
                info!(
                    "Received DestroyRunner command: {} ({})",
                    cmd.command_id, cmd.vm_id
                );
                send_command_ack(&tx, cmd.command_id.clone(), true, String::new()).await?;
                self.handle_destroy_runner(cmd, tx.clone()).await;
            }
            CoordinatorPayload::Ping(Ping {}) => {
                debug!("Received ping from coordinator");
                tx.send(AgentMessage {
                    payload: Some(AgentPayload::Pong(Pong {})),
                })
                .await
                .map_err(|_| Error::ChannelSend)?;
                let status = self.build_status().await;
                tx.send(AgentMessage {
                    payload: Some(AgentPayload::Status(status)),
                })
                .await
                .map_err(|_| Error::ChannelSend)?;
            }
        }

        Ok(())
    }

    /// Select the appropriate template VMID based on job labels.
    ///
    /// Iterates through `template_mappings` in order and returns the template_vmid from the first
    /// mapping where ALL mapping labels are present in the job labels (case-insensitive).
    /// Falls back to the default `template_vmid` if no mapping matches.
    fn select_template(&self, job_labels: &[String]) -> u32 {
        let job_labels_lower: HashSet<String> = job_labels
            .iter()
            .map(|l| l.to_lowercase())
            .collect();

        for mapping in &self.config.vm.template_mappings {
            let all_match = mapping
                .labels
                .iter()
                .all(|ml| job_labels_lower.contains(&ml.to_lowercase()));
            if all_match {
                info!(
                    "Selected template {} for labels {:?} (matched mapping labels {:?})",
                    mapping.template_vmid, job_labels, mapping.labels
                );
                return mapping.template_vmid;
            }
        }

        // Fallback to default
        info!(
            "No template mapping matched labels {:?}, using default {}",
            job_labels, self.config.vm.template_vmid
        );
        self.config.vm.template_vmid
    }

    /// Handle a CreateRunner command.
    async fn handle_create_runner(
        &self,
        cmd: kuiper_agent_proto::CreateRunnerCommand,
        tx: mpsc::Sender<AgentMessage>,
    ) {
        let vm_manager = self.vm_manager.clone();
        let runner_name = cmd.vm_name.clone();

        // Select template based on job labels
        let template_vmid = self.select_template(&cmd.labels);

        // Spawn task to handle the runner lifecycle
        tokio::spawn(async move {
            let result = vm_manager
                .run_complete_lifecycle(
                    &cmd.vm_name,
                    template_vmid,
                    &cmd.registration_token,
                    &cmd.labels,
                    &cmd.runner_scope_url,
                )
                .await;

            match result {
                Ok((vmid, ip)) => {
                    info!("Runner {} completed successfully", runner_name);
                    if let Err(e) = send_runner_event(
                        &tx,
                        runner_name,
                        vmid.to_string(),
                        RunnerEventType::Completed,
                        String::new(),
                    )
                    .await
                    {
                        error!("Failed to send runner event: {}", e);
                    }
                    info!("Runner completed at IP {}", ip);
                }
                Err(e) => {
                    error!("Runner {} failed: {}", runner_name, e);
                    if let Err(e) = send_runner_event(
                        &tx,
                        runner_name,
                        cmd.vm_name.clone(),
                        RunnerEventType::Failed,
                        e.to_string(),
                    )
                    .await
                    {
                        error!("Failed to send runner event: {}", e);
                    }
                }
            }
        });
    }

    /// Handle a DestroyRunner command.
    async fn handle_destroy_runner(
        &self,
        cmd: kuiper_agent_proto::DestroyRunnerCommand,
        tx: mpsc::Sender<AgentMessage>,
    ) {
        let vm_manager = self.vm_manager.clone();
        let vm_id = cmd.vm_id.clone();
        let runner_name = vm_id.clone();

        // Spawn task to handle destruction
        tokio::spawn(async move {
            let result = vm_manager.force_destroy(&vm_id).await;

            match result {
                Ok(()) => {
                    info!("VM {} destroyed successfully", vm_id);
                    if let Err(e) = send_runner_event(
                        &tx,
                        runner_name,
                        vm_id,
                        RunnerEventType::Destroyed,
                        String::new(),
                    )
                    .await
                    {
                        error!("Failed to send runner event: {}", e);
                    }
                }
                Err(e) => {
                    error!("Failed to destroy VM {}: {}", vm_id, e);
                    if let Err(e) = send_runner_event(
                        &tx,
                        runner_name,
                        vm_id,
                        RunnerEventType::Failed,
                        e.to_string(),
                    )
                    .await
                    {
                        error!("Failed to send runner event: {}", e);
                    }
                }
            }
        });
    }

    /// Build current agent status.
    async fn build_status(&self) -> AgentStatus {
        let vms = self.vm_manager.list_vms().await;
        let active_count = vms.len() as u32;
        let available = self.config.vm.concurrent_vms.saturating_sub(active_count);
        let hostname = gethostname::gethostname()
            .to_string_lossy()
            .to_string();

        AgentStatus {
            active_vms: active_count,
            available_slots: available,
            vms: vms
                .into_iter()
                .map(|vm| VmInfo {
                    vm_id: vm.vmid.to_string(),
                    name: vm.name,
                    state: vm.state.as_str().to_string(),
                    ip_address: vm.ip_address.unwrap_or_default(),
                })
                .collect(),
            // Identity fields - required for first message
            agent_id: self.cert_store.get_agent_id().unwrap_or_default(),
            hostname,
            agent_type: "proxmox".to_string(),
            labels: self.config.agent.labels.clone(),
            max_vms: self.config.vm.concurrent_vms,
        }
    }
}

/// Initialize logging with file output and stdout.
fn init_logging(data_dir: &PathBuf, verbose: bool) -> anyhow::Result<()> {
    let log_dir = data_dir.join("logs");
    std::fs::create_dir_all(&log_dir)?;

    // Create a daily rotating file appender (e.g., kuiper-proxmox-agent.2026-01-15.log)
    let file_appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("kuiper-proxmox-agent")
        .filename_suffix("log")
        .build(&log_dir)?;

    // Non-blocking writer for the file
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    // Leak the guard to keep the writer alive for the lifetime of the program
    std::mem::forget(_guard);

    let filter = if verbose {
        EnvFilter::try_new("debug,russh=info,hyper=info,reqwest=info")?
    } else {
        EnvFilter::try_new("info,russh=warn")?
    };

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_target(false)) // stdout
        .with(fmt::layer().with_target(true).with_ansi(false).with_writer(non_blocking)) // file
        .init();

    info!("Logging to: {}", log_dir.display());
    Ok(())
}
