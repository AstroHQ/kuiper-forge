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

use clap::Parser;
use config::Config;
use error::{Error, Result};
use kuiper_agent_lib::{
    AgentCertStore, AgentConfig as LibAgentConfig, AgentConnector, RegistrationBundle,
};
use kuiper_agent_proto::{
    AgentMessage, AgentPayload, AgentStatus, CommandAck, CoordinatorPayload, LabelSet, Ping, Pong,
    RunnerEvent, RunnerEventType, VmInfo,
};
use kuiper_proxmox_api::{ProxmoxAuth, ProxmoxVEAPI};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tracing::{debug, error, info};
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};
use vm_manager::VmManager;

/// Proxmox Agent for CI Runner Coordination
#[derive(Parser, Debug)]
#[command(name = "kuiper-proxmox-agent")]
#[command(about = "Proxmox VE agent for ephemeral CI runner VMs")]
struct Args {
    /// Path to configuration file
    #[arg(short, long)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(clap::Subcommand, Debug)]
enum Commands {
    /// Register this agent with the coordinator using a registration bundle
    Register {
        /// Registration bundle token from coordinator (kfr1_...)
        bundle: String,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let data_dir = Config::default_data_dir();

    // Initialize logging with file output
    init_logging(&data_dir)?;

    // Handle subcommands
    if let Some(command) = args.command {
        match command {
            Commands::Register { bundle } => {
                let config_path = args.config.unwrap_or_else(Config::default_config_path);
                return cmd_register(&bundle, &config_path).await;
            }
        }
    }

    info!("Starting kuiper-proxmox-agent");

    // Normal agent mode - load existing config
    let config = match &args.config {
        Some(path) => Config::load(path)?,
        None => Config::load_default()?,
    };

    info!("Loaded configuration");
    debug!("Coordinator URL: {}", config.coordinator.url);
    debug!("Proxmox node: {}", config.proxmox.node);
    debug!("Template VMID: {}", config.vm.template_vmid);
    debug!("Max concurrent VMs: {}", config.vm.concurrent_vms);

    // Seed server trust from config if provided
    let cert_store = AgentCertStore::new(config.tls.certs_dir.clone());
    if let Some(ref ca_cert_path) = config.tls.ca_cert {
        let ca_dest = config.tls.certs_dir.join("ca.crt");
        if !ca_dest.exists() && ca_cert_path.exists() {
            std::fs::create_dir_all(&config.tls.certs_dir)?;
            std::fs::copy(ca_cert_path, &ca_dest)?;
            info!("Copied server CA certificate to {}", ca_dest.display());
        }
    }
    // Verify certificates exist
    if !cert_store.has_certificates() {
        eprintln!("Error: Certificates not found\n");
        eprintln!("The config file exists but certificates are missing.");
        eprintln!("Please re-register:");
        eprintln!("  kuiper-proxmox-agent register <kfr1_token>");
        std::process::exit(1);
    }

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

    // Clone vm_manager for shutdown cleanup
    let shutdown_vm_manager = vm_manager.clone();

    // Create agent runner
    let agent = ProxmoxAgent::new(config, vm_manager, cert_store);

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

/// Handle the register subcommand to set up agent registration with the coordinator.
async fn cmd_register(bundle_token: &str, config_path: &Path) -> anyhow::Result<()> {
    println!("Registering agent with coordinator...\n");

    // 1. Parse bundle
    let bundle = RegistrationBundle::decode(bundle_token)
        .map_err(|e| anyhow::anyhow!("Invalid registration bundle: {e}"))?;

    println!("Coordinator: {}", bundle.coordinator_url);

    // 2. Create cert store and save server trust
    let data_dir = Config::default_data_dir();
    let certs_dir = data_dir.join("certs");
    std::fs::create_dir_all(&certs_dir)?;

    let cert_store = AgentCertStore::new(certs_dir.clone());
    if let Some(ca_pem) = bundle.server_ca_pem.as_deref() {
        cert_store.save_ca(ca_pem)?;
        println!("✓ Saved server CA certificate");
    }
    cert_store.save_server_trust_mode(match bundle.server_trust_mode {
        kuiper_agent_lib::bundle::ServerTrustMode::Ca => "ca",
        kuiper_agent_lib::bundle::ServerTrustMode::Chain => "chain",
    })?;

    // 3. Extract hostname from URL for TLS verification
    let hostname = url::Url::parse(&bundle.coordinator_url)
        .ok()
        .and_then(|u| u.host_str().map(String::from))
        .unwrap_or_else(|| "localhost".to_string());

    // 4. Build agent config for registration
    let agent_config = kuiper_agent_lib::AgentConfig {
        coordinator_url: bundle.coordinator_url.clone(),
        coordinator_hostname: hostname.clone(),
        registration_token: Some(bundle.token),
        agent_type: "proxmox".to_string(),
        labels: vec![],      // Empty for registration - user will set in config
        label_sets: vec![],  // Empty for registration - user will set in config
        max_vms: 5,          // Default - user will set in config
    };

    // 5. Connect and register
    println!("Connecting to coordinator...");
    let mut connector = kuiper_agent_lib::AgentConnector::new(agent_config, cert_store.clone());
    let _client = connector
        .connect()
        .await
        .map_err(|e| anyhow::anyhow!("Registration failed: {e}"))?;

    let agent_id = cert_store
        .get_agent_id()
        .ok_or_else(|| anyhow::anyhow!("Failed to get agent ID after registration"))?;

    println!("✓ Registration successful");
    println!("✓ Agent ID: {agent_id}");

    // 6. Generate initial config with placeholders
    // Note: User must edit this config to add Proxmox API credentials and VM settings
    let config = Config::generate_template(bundle.coordinator_url, hostname, certs_dir);

    config.save(config_path)?;
    println!("✓ Configuration saved\n");

    // 7. Print next steps
    println!("Next steps:");
    println!("  1. Edit {} and configure:", config_path.display());
    println!("     • agent.labels (e.g., [\"proxmox\", \"x86_64\"])");
    println!("     • proxmox.* settings (API URL, credentials, node)");
    println!("     • vm.* settings (template ID, resources)");
    println!("     • ssh.* settings (user, private key path)");
    println!("  2. Ensure VM template exists in Proxmox");
    println!("  3. Start agent: kuiper-proxmox-agent\n");

    println!("Certificate location: {}", cert_store.base_dir().display());
    println!("Config location:      {}", config_path.display());

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
    let username = config
        .proxmox
        .token_id
        .split('!')
        .next()
        .unwrap_or(&config.proxmox.token_id);

    ProxmoxVEAPI::with_node(
        username,
        auth,
        &config.proxmox.api_url,
        &config.proxmox.node,
        config.proxmox.accept_invalid_certs,
    )
    .map_err(|e| Error::Vm(format!("Failed to create Proxmox client: {e}")))
}

/// The main Proxmox agent.
struct ProxmoxAgent {
    config: Config,
    vm_manager: Arc<VmManager>,
    cert_store: AgentCertStore,
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
    fn new(config: Config, vm_manager: Arc<VmManager>, cert_store: AgentCertStore) -> Arc<Self> {
        Arc::new(Self {
            config,
            vm_manager,
            cert_store,
        })
    }

    /// Run the agent, handling reconnection.
    async fn run(self: &Arc<Self>) -> Result<()> {
        loop {
            match self.connect_and_run().await {
                Ok(_) => {
                    info!("Connection closed, reconnecting...");
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
        // For proxmox, label_sets is just a single set containing the configured labels
        let labels = self.config.agent.labels.clone();
        let agent_config = LibAgentConfig {
            coordinator_url: self.config.coordinator.url.clone(),
            coordinator_hostname: self.config.coordinator.hostname.clone(),
            registration_token: None, // Already registered, using stored certificates
            agent_type: "proxmox".to_string(),
            labels: labels.clone(),
            label_sets: vec![labels], // Single capability set
            max_vms: self.config.vm.concurrent_vms,
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
            .map_err(Error::Grpc)?;

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
                    self.handle_coordinator_message(coordinator_msg, &tx)
                        .await?;
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
        self: &Arc<Self>,
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
                send_command_ack(tx, cmd.command_id.clone(), true, String::new()).await?;
                self.handle_create_runner(cmd, tx.clone()).await;
            }
            CoordinatorPayload::DestroyRunner(cmd) => {
                info!(
                    "Received DestroyRunner command: {} ({})",
                    cmd.command_id, cmd.vm_id
                );
                send_command_ack(tx, cmd.command_id.clone(), true, String::new()).await?;
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
        let job_labels_lower: HashSet<String> =
            job_labels.iter().map(|l| l.to_lowercase()).collect();

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
        self: &Arc<Self>,
        cmd: kuiper_agent_proto::CreateRunnerCommand,
        tx: mpsc::Sender<AgentMessage>,
    ) {
        let vm_manager = self.vm_manager.clone();
        let runner_name = cmd.vm_name.clone();
        let agent = Arc::clone(self);

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
                    // Send immediate status update so coordinator knows capacity is available
                    let status = agent.build_status().await;
                    let _ = tx
                        .send(AgentMessage {
                            payload: Some(AgentPayload::Status(status)),
                        })
                        .await;
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
                    // Send immediate status update so coordinator knows capacity is available
                    let status = agent.build_status().await;
                    let _ = tx
                        .send(AgentMessage {
                            payload: Some(AgentPayload::Status(status)),
                        })
                        .await;
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
        self: &Arc<Self>,
        cmd: kuiper_agent_proto::DestroyRunnerCommand,
        tx: mpsc::Sender<AgentMessage>,
    ) {
        let vm_manager = self.vm_manager.clone();
        let vm_id = cmd.vm_id.clone();
        let runner_name = vm_id.clone();
        let agent = Arc::clone(self);

        // Spawn task to handle destruction
        tokio::spawn(async move {
            let result = vm_manager.force_destroy(&vm_id).await;

            match result {
                Ok(()) => {
                    info!("VM {} destroyed successfully", vm_id);
                    // Send immediate status update so coordinator knows capacity is available
                    let status = agent.build_status().await;
                    let _ = tx
                        .send(AgentMessage {
                            payload: Some(AgentPayload::Status(status)),
                        })
                        .await;
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
        let hostname = gethostname::gethostname().to_string_lossy().to_string();

        // Convert labels to label_sets (single capability set for proxmox)
        let labels = self.config.agent.labels.clone();
        let label_sets = vec![LabelSet {
            labels: labels.clone(),
        }];

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
            labels,
            max_vms: self.config.vm.concurrent_vms,
            label_sets,
        }
    }
}

/// Initialize logging with file output and stdout.
fn init_logging(data_dir: &Path) -> anyhow::Result<()> {
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

    // Base filter suppresses noisy libraries, RUST_LOG layers on top (can override if explicit)
    let base = "russh=warn,hyper=warn,reqwest=warn,h2=warn,rustls=warn,tonic=warn";
    let filter = match std::env::var("RUST_LOG") {
        Ok(env) => EnvFilter::new(format!("{base},{env}")),
        Err(_) => EnvFilter::new(format!("{base},info")),
    };

    tracing_subscriber::registry()
        .with(filter)
        .with(fmt::layer().with_target(false)) // stdout
        .with(
            fmt::layer()
                .with_target(true)
                .with_ansi(false)
                .with_writer(non_blocking),
        ) // file
        .init();

    info!("Logging to: {}", log_dir.display());
    Ok(())
}
