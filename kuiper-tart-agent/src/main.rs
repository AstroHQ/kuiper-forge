//! Tart Agent - manages macOS and linux VMs via Tart CLI for CI runners.
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

mod backend;
mod config;
mod error;
mod host_checks;
mod install;
mod setup;
mod ssh;
mod update;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use kuiper_agent_lib::vm_agent::{AgentInfo, RunOptions, VmAgent, VmBackend};
use kuiper_agent_lib::{AgentCertStore, AgentConfig, LogCapture};
use tracing::{error, info, warn};
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

use backend::TartBackend;
use config::Config;
use ssh::SshConfig;

/// Tart VM Agent for CI Runner Coordination
#[derive(Parser, Debug)]
#[command(name = "kuiper-tart-agent", version, about, long_about = None)]
struct Args {
    /// Path to configuration file (default: platform-specific config dir)
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Skip host environment checks (tart version, local images, DHCP)
    #[arg(long)]
    skip_host_checks: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(clap::Subcommand, Debug)]
enum Commands {
    /// First-run wizard: checks the host, registers, then walks through labels, image mappings and limits. Re-run it
    /// to change an existing config
    Setup,
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
    /// Update to the latest release from GitHub, restarting the LaunchAgent if it's running
    Update {
        /// Install this version instead of the latest (e.g. 0.4.0), allows downgrades
        version: Option<String>,
        /// Only check whether an update is available
        #[arg(long)]
        check: bool,
        /// Restart the LaunchAgent without asking, even if VMs are running
        #[arg(long, short)]
        yes: bool,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let config_path = args.config.clone().unwrap_or_else(Config::default_path);
    let data_dir = Config::default_data_dir();

    // Handle subcommands. They run before there's a config, so their logs keep the default retention
    if let Some(command) = args.command {
        init_logging(
            &data_dir,
            config::LoggingConfig::default().retention_days as usize,
        )?;
        match command {
            Commands::Setup => {
                return setup::run_setup(&config_path).await;
            }
            Commands::Register { bundle } => {
                return cmd_register(&bundle, &config_path).await;
            }
            Commands::Install { no_load, force } => {
                return cmd_install(no_load, force, &config_path).await;
            }
            Commands::Uninstall { purge } => {
                return cmd_uninstall(purge, &config_path).await;
            }
            Commands::Update {
                version,
                check,
                yes,
            } => {
                return update::run(version, check, yes).await;
            }
        }
    }

    // Normal agent mode - load existing config
    if !config_path.exists() {
        eprintln!("Error: Agent not registered\n");
        eprintln!("Run the setup wizard first:");
        eprintln!("  kuiper-tart-agent setup\n");
        eprintln!("To get a registration bundle, run on the coordinator:");
        eprintln!("  coordinator token create --expires 1h --url https://your-coordinator:9443");
        std::process::exit(1);
    }

    // loaded before logging starts, it sets the log retention
    let config = Config::load(&config_path)?;
    let log_capture = init_logging(&data_dir, config.logging.retention_days as usize)?;
    info!("Loaded config from: {}", config_path.display());

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
    if args.skip_host_checks {
        warn!("Skipping host environment checks (--skip-host-checks)");
    } else {
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
    }

    info!("kuiper-tart-agent starting");
    info!("Coordinator: {}", config.coordinator.url);
    info!(
        "Max VMs: {} macOS, {} total",
        config.tart.max_macos_vms, config.tart.max_total_vms
    );

    let pooled: u32 = config
        .tart
        .image_mappings
        .iter()
        .filter_map(|m| m.pool)
        .sum();
    if pooled > config.tart.max_total_vms {
        warn!(
            "Mapping pools add up to {} runners but max_total_vms is {}, the coordinator can't fill them all",
            pooled, config.tart.max_total_vms
        );
    }

    let backend = Arc::new(TartBackend::new(
        config.tart.clone(),
        config.agent.labels.clone(),
        SshConfig::from_config(&config.tart.ssh),
        data_dir.join("logs"),
    ));
    info!("Capabilities: {:?}", backend.capabilities());

    // an unpulled OCI image's OS is unknown and counts as macOS, which can block linux jobs on the macOS limit.
    // Pulling finds out
    let pull_backend = backend.clone();
    tokio::spawn(async move {
        while !pull_backend.resolve_image_os().await {
            tokio::time::sleep(backend::IMAGE_OS_RETRY_INTERVAL).await;
        }
    });

    // count external VMs before the first status so the coordinator never sees too many free slots
    backend.refresh_external().await;
    let external_backend = backend.clone();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(backend::EXTERNAL_POLL_INTERVAL);
        loop {
            ticker.tick().await;
            external_backend.refresh_external().await;
        }
    });

    let log_retention_days = config.logging.retention_days;
    let cleanup_log_dir = data_dir.join("logs");
    let cleanup_interval = Duration::from_secs(config.cleanup.cleanup_interval_mins as u64 * 60);
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(cleanup_interval);
        loop {
            ticker.tick().await;
            cleanup_old_logs(&cleanup_log_dir, log_retention_days);
        }
    });

    let agent = Arc::new(VmAgent::new(
        backend,
        cert_store,
        AgentInfo {
            agent_type: "tart".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
        },
    ));

    if !config.logging.upload {
        log_capture.disable();
    }
    agent
        .run(RunOptions {
            // the advertised labels, label sets and max_vms go in AgentStatus, not here, so the
            // coordinator always sees the current values from config
            agent_config: AgentConfig {
                coordinator_url: config.coordinator.url.clone(),
                coordinator_hostname: config.coordinator.hostname.clone(),
                registration_token: None, // Already registered, using stored certificates
                agent_type: "tart".to_string(),
            },
            initial_reconnect_delay: Duration::from_secs(config.reconnect.initial_delay_secs),
            max_reconnect_delay: Duration::from_secs(config.reconnect.max_delay_secs),
            log_capture: config.logging.upload.then_some(log_capture),
            cleanup_interval,
            max_vm_age: Duration::from_secs(config.cleanup.max_vm_age_hours as u64 * 3600),
        })
        .await;

    Ok(())
}

/// Handle the register subcommand to set up agent registration with the coordinator.
async fn cmd_register(bundle_token: &str, config_path: &Path) -> anyhow::Result<()> {
    println!("Registering agent with coordinator...\n");
    let (coordinator, tls) = setup::register(bundle_token).await?;

    setup::blank_config(coordinator, tls).save(config_path)?;
    println!("✓ Configuration saved\n");

    println!("Next steps:");
    println!(
        "  1. Run `kuiper-tart-agent setup` to pick labels and images, or edit {} and set:",
        config_path.display()
    );
    println!("     • agent.labels (e.g., [\"macos\", \"arm64\", \"sequoia\"])");
    println!("     • tart.base_image (e.g., \"ghcr.io/cirruslabs/macos-sequoia-base:latest\")");
    println!("     • tart.max_macos_vms (default: 2) and tart.max_total_vms (default: 5)");
    println!("  2. Ensure base image is pulled: tart pull <image>");
    println!("  3. Start agent: kuiper-tart-agent\n");

    println!("Config location: {}", config_path.display());

    Ok(())
}

/// Handle the install subcommand to set up the agent as a LaunchAgent.
pub(crate) async fn cmd_install(
    no_load: bool,
    force: bool,
    config_path: &Path,
) -> anyhow::Result<()> {
    println!("Installing kuiper-tart-agent as LaunchAgent...\n");

    // 1. Check binary is in PATH
    print!("Checking binary location... ");
    let binary_path = install::find_binary_in_path().map_err(|e| anyhow::anyhow!("{e}"))?;
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
        install::load_service().map_err(|e| anyhow::anyhow!("{e}"))?;
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

/// Initialize logging with file output and stdout.
fn init_logging(data_dir: &Path, retention_days: usize) -> anyhow::Result<LogCapture> {
    let log_dir = data_dir.join("logs");
    std::fs::create_dir_all(&log_dir)?;

    // Create a daily rotating file appender (e.g., kuiper-tart-agent.2026-01-15.log)
    let file_appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("kuiper-tart-agent")
        .filename_suffix("log")
        .max_log_files(retention_days)
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

    let capture = LogCapture::new();
    tracing_subscriber::registry()
        .with(filter)
        .with(capture.layer()) // uploaded to the coordinator once connected
        .with(fmt::layer().with_target(false)) // stdout
        .with(
            fmt::layer()
                .with_target(true)
                .with_ansi(false)
                .with_writer(non_blocking),
        ) // file
        .init();

    info!("Logging to: {}", log_dir.display());
    Ok(capture)
}

/// Remove runner log files older than the configured retention period.
///
/// Agent log files (kuiper-tart-agent.*.log) are handled by tracing-appender's
/// max_log_files. This function cleans up runner-*.log files.
fn cleanup_old_logs(log_dir: &Path, retention_days: u32) {
    let cutoff = std::time::SystemTime::now()
        - std::time::Duration::from_secs(retention_days as u64 * 86400);

    let entries = match std::fs::read_dir(log_dir) {
        Ok(e) => e,
        Err(e) => {
            warn!("Failed to read log directory for cleanup: {e}");
            return;
        }
    };

    let mut removed = 0u32;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("runner-") || !name.ends_with(".log") {
            continue;
        }
        if let Ok(meta) = entry.metadata() {
            let modified = meta.modified().unwrap_or(std::time::SystemTime::UNIX_EPOCH);
            if modified < cutoff {
                if let Err(e) = std::fs::remove_file(entry.path()) {
                    warn!("Failed to remove old log {}: {e}", entry.path().display());
                } else {
                    removed += 1;
                }
            }
        }
    }
    if removed > 0 {
        info!("Cleaned up {removed} runner log file(s) older than {retention_days} days");
    }
}
