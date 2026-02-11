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
//! First run with a registration bundle:
//! ```
//! kuiper-tart-agent --register kfr1_<bundle> --labels macos,arm64
//! ```
//!
//! Subsequent runs (uses saved config):
//! ```
//! kuiper-tart-agent
//! ```

mod config;
mod error;
mod host_checks;
mod install;
mod ssh;
mod vm_manager;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use kuiper_agent_lib::{AgentCertStore, AgentConfig, AgentConnector, RegistrationBundle};
use kuiper_agent_proto::{
    AgentMessage, AgentPayload, AgentStatus, CommandAck, CoordinatorPayload, LabelSet, Pong,
    RunnerEvent, RunnerEventType,
};
use tokio::signal;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tracing::{error, info, warn};
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

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
    /// Install as a macOS LaunchAgent (runs on login, restarts on crash)
    Install {
        /// Don't load the service after installation (just generate plist)
        #[arg(long)]
        no_load: bool,
        /// Force overwrite existing plist
        #[arg(long, short)]
        force: bool,
    },
    /// Uninstall the LaunchAgent service
    Uninstall {
        /// Also remove configuration and data files
        #[arg(long)]
        purge: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let config_path = args.config.clone().unwrap_or_else(Config::default_path);
    let data_dir = Config::default_data_dir();

    // Initialize logging with file output
    init_logging(&data_dir)?;

    // Handle subcommands
    if let Some(command) = args.command {
        match command {
            Commands::Register { bundle } => {
                return cmd_register(&bundle, &config_path).await;
            }
            Commands::Install { no_load, force } => {
                return cmd_install(no_load, force, &config_path).await;
            }
            Commands::Uninstall { purge } => {
                return cmd_uninstall(purge, &config_path).await;
            }
        }
    }

    // Normal agent mode - load existing config
    if !config_path.exists() {
        eprintln!("Error: Agent not registered\n");
        eprintln!("Run registration first:");
        eprintln!("  kuiper-tart-agent register <kfr1_token>\n");
        eprintln!("To get a registration bundle, run on the coordinator:");
        eprintln!("  coordinator token create --expires 1h --url https://your-coordinator:9443");
        std::process::exit(1);
    }

    info!("Loading config from: {}", config_path.display());
    let config = Config::load(&config_path)?;

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
        eprintln!("  kuiper-tart-agent register <kfr1_token>");
        std::process::exit(1);
    }

    // Run host environment checks
    match host_checks::check_tart_version() {
        Ok(version) => info!("Tart CLI version: {}", version),
        Err(msg) => {
            error!("{}", msg);
            std::process::exit(1);
        }
    }

    // Check local images exist
    let mut images_to_check: Vec<&str> = vec![&config.tart.base_image];
    for mapping in &config.tart.image_mappings {
        images_to_check.push(&mapping.image);
    }
    if let Err(msg) = host_checks::check_local_images(&images_to_check) {
        error!("{}", msg);
        std::process::exit(1);
    }

    match config.host.dhcp_lease_check.as_str() {
        "ignore" => {}
        mode => {
            if let Err(msg) = host_checks::check_dhcp_lease_time() {
                let fix_cmd = host_checks::dhcp_lease_fix_command();
                if mode == "error" {
                    error!("{}", msg);
                    error!("Fix with: {}", fix_cmd);
                    std::process::exit(1);
                } else {
                    warn!("{}", msg);
                    warn!("Fix with: {}", fix_cmd);
                }
            }
        }
    }

    info!("kuiper-tart-agent starting");
    info!("Coordinator: {}", config.coordinator.url);
    info!("Max concurrent VMs: {}", config.tart.max_concurrent_vms);

    // Build label_sets: each set is base_labels + one image_mapping's labels
    // This represents the capabilities this agent can fulfill
    let base_labels: Vec<String> = config
        .agent
        .labels
        .iter()
        .map(|l| l.to_lowercase())
        .collect();

    let label_sets: Vec<Vec<String>> = if config.tart.image_mappings.is_empty() {
        // No image mappings - just use base labels as a single capability
        vec![base_labels.clone()]
    } else {
        // Each image_mapping defines a capability: base_labels + mapping labels
        config
            .tart
            .image_mappings
            .iter()
            .map(|mapping| {
                let mut labels = base_labels.clone();
                for label in &mapping.labels {
                    let lower = label.to_lowercase();
                    if !labels.iter().any(|l| l.eq_ignore_ascii_case(&lower)) {
                        labels.push(lower);
                    }
                }
                labels
            })
            .collect()
    };

    info!(
        "Label sets: {:?} (base: {:?}, image_mappings: {:?})",
        label_sets,
        config.agent.labels,
        config
            .tart
            .image_mappings
            .iter()
            .map(|m| &m.labels)
            .collect::<Vec<_>>()
    );

    // Initialize VM manager with SSH config from file
    let tart_config = config.tart.clone();
    let ssh_config = SshConfig::from_config(&config.tart.ssh);
    let log_dir = data_dir.join("logs");
    let vm_manager = Arc::new(VmManager::new(tart_config, ssh_config, log_dir));

    // Initialize certificate store
    let cert_store = AgentCertStore::new(config.tls.certs_dir.clone());

    // Build agent connector config
    let agent_config = AgentConfig {
        coordinator_url: config.coordinator.url.clone(),
        coordinator_hostname: config.coordinator.hostname.clone(),
        registration_token: None, // Already registered, using stored certificates
        agent_type: "tart".to_string(),
        labels: base_labels,
        label_sets,
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
        agent_type: "tart".to_string(),
        labels: vec![],     // Empty for registration - user will set in config
        label_sets: vec![], // Empty for registration - derived from image_mappings
        max_vms: 2,         // Default - user will set in config
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
    let config = Config {
        coordinator: config::CoordinatorConfig {
            url: bundle.coordinator_url,
            hostname,
        },
        tls: config::TlsConfig {
            ca_cert: Some(certs_dir.join("ca.crt")),
            certs_dir,
        },
        agent: config::AgentConfig { labels: vec![] },
        tart: config::TartConfig {
            base_image: String::new(), // User must set
            max_concurrent_vms: 2,
            shared_cache_dir: None,
            ssh: config::SshAuthConfig::default(),
            runner_version: "latest".to_string(),
            image_mappings: vec![
                config::ImageMapping {
                    labels: vec!["macOS".to_string(), "sonoma".to_string()],
                    image: "ghcr.io/cirruslabs/macos-sonoma-base:latest".to_string(),
                },
                config::ImageMapping {
                    labels: vec!["macOS".to_string(), "ventura".to_string()],
                    image: "ghcr.io/cirruslabs/macos-ventura-base:latest".to_string(),
                },
            ],
        },
        cleanup: config::CleanupConfig::default(),
        reconnect: config::ReconnectConfig::default(),
        host: config::HostConfig::default(),
    };

    config.save(config_path)?;
    println!("✓ Configuration saved\n");

    // 7. Print next steps
    println!("Next steps:");
    println!("  1. Edit {} and configure:", config_path.display());
    println!("     • agent.labels (e.g., [\"macos\", \"arm64\", \"sequoia\"])");
    println!("     • tart.base_image (e.g., \"ghcr.io/cirruslabs/macos-sequoia-base:latest\")");
    println!("     • tart.max_concurrent_vms (default: 2)");
    println!("  2. Ensure base image is pulled: tart pull <image>");
    println!("  3. Start agent: kuiper-tart-agent\n");

    println!("Certificate location: {}", cert_store.base_dir().display());
    println!("Config location:      {}", config_path.display());

    Ok(())
}

/// Handle the install subcommand to set up the agent as a LaunchAgent.
async fn cmd_install(no_load: bool, force: bool, config_path: &Path) -> anyhow::Result<()> {
    println!("Installing kuiper-tart-agent as LaunchAgent...\n");

    // 1. Check binary is in PATH
    print!("Checking binary location... ");
    let binary_path = install::find_binary_in_path().map_err(|e| anyhow::anyhow!("{}", e))?;
    println!("found at {}", binary_path.display());

    // 2. Check config exists
    print!("Checking configuration... ");
    if !config_path.exists() {
        return Err(anyhow::anyhow!(
            "Config file not found at {}.\nRun 'kuiper-tart-agent register <token>' first.",
            config_path.display()
        ));
    }
    println!("found at {}", config_path.display());

    // 3. Check for existing installation
    let plist_path = install::plist_path();
    if plist_path.exists() {
        if !force {
            return Err(anyhow::anyhow!(
                "Service already installed at {}.\nUse --force to overwrite.",
                plist_path.display()
            ));
        }
        // Always unload before overwriting - service may be loaded even if not "running"
        print!("Unloading existing service... ");
        let _ = install::unload_service(); // Ignore errors
        println!("done");
    }

    // 4. Ensure log directory exists
    let log_dir = Config::default_data_dir().join("logs");
    std::fs::create_dir_all(&log_dir)?;

    // 5. Generate plist
    let plist_content = install::generate_plist(&binary_path, config_path, &log_dir);

    // 6. Ensure LaunchAgents directory exists
    let launch_agents_dir = install::launch_agents_dir();
    if !launch_agents_dir.exists() {
        std::fs::create_dir_all(&launch_agents_dir)?;
    }

    // 8. Write plist
    print!("Writing plist to {}... ", plist_path.display());
    std::fs::write(&plist_path, &plist_content)?;
    println!("done");

    // 9. Load service
    if !no_load {
        print!("Loading service... ");
        install::load_service().map_err(|e| anyhow::anyhow!("{}", e))?;
        println!("done");

        // Brief delay then check status
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        if install::is_service_running() {
            println!("\n\u{2713} Service is running.");
        } else {
            println!(
                "\nWarning: Service may not have started. Check logs at {}",
                log_dir.display()
            );
        }
    } else {
        println!("\nPlist generated but not loaded (--no-load specified).");
    }

    println!("\nInstallation complete!");
    println!("\nUseful commands:");
    println!(
        "  View logs:    tail -f {}/launchd-stdout.log",
        log_dir.display()
    );
    println!("  Stop:         launchctl unload {}", plist_path.display());
    println!("  Start:        launchctl load {}", plist_path.display());
    println!("  Uninstall:    kuiper-tart-agent uninstall");

    Ok(())
}

/// Handle the uninstall subcommand to remove the LaunchAgent.
async fn cmd_uninstall(purge: bool, config_path: &Path) -> anyhow::Result<()> {
    let plist_path = install::plist_path();

    if !plist_path.exists() {
        println!(
            "Service not installed (no plist at {}).",
            plist_path.display()
        );
        if !purge {
            return Ok(());
        }
    } else {
        println!("Found service at {}", plist_path.display());

        // Unload if running
        if install::is_service_running() {
            print!("Stopping service... ");
            let _ = install::unload_service(); // Ignore errors
            println!("done");
        }

        // Remove plist
        print!("Removing plist... ");
        std::fs::remove_file(&plist_path)?;
        println!("done");

        println!("\u{2713} Service uninstalled.");
    }

    if purge {
        println!("\nPurging data files...");

        if config_path.exists() {
            print!("Removing config at {}... ", config_path.display());
            std::fs::remove_file(config_path)?;
            println!("done");
        }

        let data_dir = Config::default_data_dir();
        if data_dir.exists() {
            print!("Removing data directory at {}... ", data_dir.display());
            std::fs::remove_dir_all(&data_dir)?;
            println!("done");
        }

        println!("\u{2713} All data purged.");
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
        let response = client.agent_stream(ReceiverStream::new(rx)).await?;

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
        let job_labels_lower: HashSet<String> =
            job_labels.iter().map(|l| l.to_lowercase()).collect();

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
                .configure_runner(
                    &vm_id,
                    &cmd.registration_token,
                    &cmd.labels,
                    &cmd.runner_scope_url,
                )
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
            self.vm_manager
                .wait_for_runner_exit(&vm_id)
                .await
                .map_err(|e| {
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
                // Send immediate status update so coordinator knows capacity is available
                let status = self.build_status().await;
                let _ = tx
                    .send(AgentMessage {
                        payload: Some(AgentPayload::Status(status)),
                    })
                    .await;
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
                // Send immediate status update so coordinator knows capacity is available
                let status = self.build_status().await;
                let _ = tx
                    .send(AgentMessage {
                        payload: Some(AgentPayload::Status(status)),
                    })
                    .await;
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
                // Send immediate status update so coordinator knows capacity is available
                let status = self.build_status().await;
                let _ = tx
                    .send(AgentMessage {
                        payload: Some(AgentPayload::Status(status)),
                    })
                    .await;
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
        let hostname = gethostname::gethostname().to_string_lossy().to_string();

        // Convert Vec<Vec<String>> to Vec<LabelSet>
        let label_sets: Vec<LabelSet> = self
            .agent_config
            .label_sets
            .iter()
            .map(|ls| LabelSet { labels: ls.clone() })
            .collect();

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
            label_sets,
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
