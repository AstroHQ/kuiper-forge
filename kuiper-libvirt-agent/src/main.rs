//! libvirt Agent - runs ephemeral CI runner VMs on the local Linux host through libvirt/KVM.
//!
//! This daemon:
//! - Connects to the coordinator via gRPC + mTLS
//! - Receives commands to create/destroy runner VMs
//! - Manages the full VM lifecycle (qcow2 overlay, transient domain, configure runner, wait, destroy)
//! - Reports status back to the coordinator

mod backend;
mod config;
mod error;
mod libvirt;
mod ssh;

use backend::LibvirtBackend;
use clap::Parser;
use config::Config;
use kuiper_agent_lib::vm_agent::{AgentInfo, RunOptions, VmAgent};
use kuiper_agent_lib::{
    AgentCertStore, AgentConfig as LibAgentConfig, LogCapture, RegistrationBundle,
};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, info, warn};
use tracing_appender::rolling::{RollingFileAppender, Rotation};
use tracing_subscriber::{EnvFilter, fmt, layer::SubscriberExt, util::SubscriberInitExt};

/// libvirt Agent for CI Runner Coordination
#[derive(Parser, Debug)]
#[command(name = "kuiper-libvirt-agent")]
#[command(about = "libvirt/KVM agent for ephemeral CI runner VMs on a Linux host")]
struct Args {
    /// Path to configuration file
    #[arg(short, long)]
    config: Option<PathBuf>,

    /// Run with system daemon (FHS) paths: config in /etc, logs in /var/log,
    /// data/certs in /var/lib. Without this, per-user directories are used.
    /// Individual paths (--config, --log-dir) still take precedence.
    ///
    /// As a flag it needs no value (`--system`); the env var accepts the usual
    /// truthy values (1/true/yes/on), so `KUIPER_LIBVIRT_AGENT_SYSTEM=1` works.
    #[arg(
        long,
        env = "KUIPER_LIBVIRT_AGENT_SYSTEM",
        value_parser = clap::builder::BoolishValueParser::new(),
        num_args = 0..=1,
        require_equals = true,
        default_value_t = false,
        default_missing_value = "true",
    )]
    system: bool,

    /// Directory for rotating log files. Overrides the default chosen by --system.
    #[arg(long, env = "KUIPER_LIBVIRT_AGENT_LOG_DIR")]
    log_dir: Option<PathBuf>,

    /// Debug mode: skip VM deletion on runner failure and exit the agent,
    /// leaving VMs running so you can SSH in and inspect.
    #[arg(long)]
    debug_keep_vms: bool,

    #[command(subcommand)]
    command: Option<Commands>,
}

impl Args {
    /// Resolve the log directory: explicit `--log-dir`, else the system or
    /// per-user default depending on `--system`.
    fn resolve_log_dir(&self) -> PathBuf {
        if let Some(dir) = &self.log_dir {
            dir.clone()
        } else if self.system {
            Config::system_log_dir()
        } else {
            Config::default_log_dir()
        }
    }

    /// Resolve an explicit config path to load. Returns `None` to fall back to
    /// the default search paths (per-user layout); `--system` pins it to /etc.
    fn resolve_config_path(&self) -> Option<PathBuf> {
        self.config
            .clone()
            .or_else(|| self.system.then(Config::system_config_path))
    }

    /// Data directory (certs, and VM disks unless `libvirt.disk_dir` is set).
    fn data_dir(&self) -> PathBuf {
        if self.system {
            Config::system_data_dir()
        } else {
            Config::default_data_dir()
        }
    }
}

#[derive(clap::Subcommand, Debug)]
enum Commands {
    /// Register this agent with the coordinator using a registration bundle
    Register {
        /// Registration bundle token from coordinator (kfr1_...)
        bundle: String,
    },
    /// Print a systemd service unit for running this agent on startup.
    ///
    /// The unit references this binary's current path. It is written to stdout
    /// only (status messages go to stderr) so it can be redirected straight to
    /// the desired location, e.g.:
    ///
    ///   kuiper-libvirt-agent generate-service > /etc/systemd/system/kuiper-libvirt-agent.service
    GenerateService,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    // Generate the systemd unit before initializing logging so the unit file is
    // the only thing on stdout and can be redirected to a file cleanly.
    if let Some(Commands::GenerateService) = args.command {
        return cmd_generate_service(args.config.as_deref(), args.system);
    }

    // Initialize logging with file output (retention applied after config load)
    let log_capture = init_logging(
        &args.resolve_log_dir(),
        config::LoggingConfig::default().retention_days as usize,
    )?;

    // Handle subcommands
    if let Some(command) = &args.command {
        match command {
            Commands::Register { bundle } => {
                let config_path = args.config.clone().unwrap_or_else(|| {
                    if args.system {
                        Config::system_config_path()
                    } else {
                        Config::default_config_path()
                    }
                });
                return cmd_register(bundle, &config_path, args.data_dir(), args.system).await;
            }
            Commands::GenerateService => unreachable!("handled before logging init"),
        }
    }

    info!("Starting kuiper-libvirt-agent");

    // Normal agent mode - load existing config
    let data_dir = args.data_dir();
    let config = match args.resolve_config_path() {
        Some(path) => Config::load(&path, &data_dir)?,
        None => Config::load_default(&data_dir)?,
    };

    info!("Loaded configuration");
    debug!("Coordinator URL: {}", config.coordinator.url);
    debug!("libvirt URI: {}", config.libvirt.uri);
    debug!("Base image: {}", config.vm.image.display());
    debug!("Max VMs: {}", config.vm.max_vms);

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
        eprintln!("  kuiper-libvirt-agent register <kfr1_token>");
        std::process::exit(1);
    }

    let domain_template = match &config.vm.domain_template {
        Some(path) => Some(std::fs::read_to_string(path).map_err(|e| {
            anyhow::anyhow!("Failed to read domain template {}: {e}", path.display())
        })?),
        None => None,
    };
    std::fs::create_dir_all(&config.libvirt.disk_dir)?;
    let backend = Arc::new(LibvirtBackend::new(&config, domain_template));

    // VMs from an earlier run that crashed (or a host reboot) would otherwise use up disk forever
    backend.libvirt().sweep_leftovers().await.map_err(|e| {
        anyhow::anyhow!(
            "Failed to clean up leftover VMs (is libvirt reachable at {}?): {e}",
            config.libvirt.uri
        )
    })?;

    if args.debug_keep_vms {
        warn!(
            "Debug mode: VMs will NOT be deleted on failure, agent will exit on first runner failure"
        );
    }
    let agent = Arc::new(
        VmAgent::new(
            backend,
            cert_store.clone(),
            AgentInfo {
                agent_type: "libvirt".to_string(),
                version: env!("CARGO_PKG_VERSION").to_string(),
            },
        )
        .keep_failed_vms(args.debug_keep_vms),
    );

    let upload_logs = config.logging.upload;
    if !upload_logs {
        log_capture.disable();
    }
    agent
        .run(RunOptions {
            agent_config: LibAgentConfig {
                coordinator_url: config.coordinator.url.clone(),
                coordinator_hostname: config.coordinator.hostname.clone(),
                registration_token: None,
                agent_type: "libvirt".to_string(),
            },
            initial_reconnect_delay: Duration::from_secs(5),
            max_reconnect_delay: Duration::from_secs(60),
            log_capture: upload_logs.then_some(log_capture),
            cleanup_interval: Duration::from_secs(config.cleanup.cleanup_interval_mins as u64 * 60),
            max_vm_age: Duration::from_secs(config.cleanup.max_vm_age_hours as u64 * 3600),
        })
        .await;

    Ok(())
}

/// Handle the register subcommand to set up agent registration with the coordinator.
///
/// `data_dir` is where certificates are stored; the caller picks the per-user or
/// system (`--system`) location.
async fn cmd_register(
    bundle_token: &str,
    config_path: &Path,
    data_dir: PathBuf,
    system: bool,
) -> anyhow::Result<()> {
    println!("Registering agent with coordinator...\n");

    // 1. Parse bundle
    let bundle = RegistrationBundle::decode(bundle_token)
        .map_err(|e| anyhow::anyhow!("Invalid registration bundle: {e}"))?;

    println!("Coordinator: {}", bundle.coordinator_url);

    // 2. Create cert store and save server trust
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

    // 4. Build agent config for registration. labels/max_vms aren't here —
    // they're sent in the first AgentStatus when the daemon connects.
    let agent_config = kuiper_agent_lib::AgentConfig {
        coordinator_url: bundle.coordinator_url.clone(),
        coordinator_hostname: hostname.clone(),
        registration_token: Some(bundle.token),
        agent_type: "libvirt".to_string(),
    };

    // 5. Connect and register.
    // Use register() (not connect()): a `register` invocation always re-registers
    // with this token instead of silently reusing an existing (possibly revoked)
    // cert. The new identity is written only on success, so a bad/expired token
    // leaves any existing certificate untouched.
    println!("Connecting to coordinator...");
    let mut connector = kuiper_agent_lib::AgentConnector::new(agent_config, cert_store.clone());
    let _client = connector
        .register()
        .await
        .map_err(|e| anyhow::anyhow!("Registration failed: {e}"))?;

    let agent_id = cert_store
        .get_agent_id()
        .ok_or_else(|| anyhow::anyhow!("Failed to get agent ID after registration"))?;

    println!("✓ Registration successful");
    println!("✓ Agent ID: {agent_id}");

    // 6. Generate initial config with placeholders
    // Note: User must edit this config to add the base image and SSH settings
    let config = Config::generate_template(bundle.coordinator_url, hostname, certs_dir);

    config.save(config_path)?;
    // Append a commented reference so the generated config documents the
    // (non-obvious) label-based image selection feature.
    {
        use std::io::Write as _;
        let mut f = std::fs::OpenOptions::new().append(true).open(config_path)?;
        f.write_all(config::IMAGE_MAPPINGS_HELP.as_bytes())?;
    }
    println!("✓ Configuration saved\n");

    // 7. Print next steps
    println!("Next steps:");
    println!("  1. Edit {} and configure:", config_path.display());
    println!("     • agent.labels (e.g., [\"libvirt\", \"linux\", \"x64\"])");
    println!("     • vm.image (base qcow2 image with SSH and the runner user)");
    println!("     • libvirt.* settings if not the qemu:///system default network");
    println!("     • ssh.* settings (user, private key path)");
    println!("  2. Make sure this user can reach libvirt (e.g. in the `libvirt` group)");
    let start_cmd = if system {
        "kuiper-libvirt-agent --system"
    } else {
        "kuiper-libvirt-agent"
    };
    println!("  3. Start agent: {start_cmd}\n");

    println!("Certificate location: {}", cert_store.base_dir().display());
    println!("Config location:      {}", config_path.display());

    Ok(())
}

/// Handle the generate-service subcommand: print a systemd unit to stdout.
///
/// The unit's `ExecStart` uses this binary's own path (via [`std::env::current_exe`])
/// so the generated file points at wherever the agent is currently installed. The
/// unit is printed to stdout and all human-facing guidance to stderr, so the output
/// can be redirected directly into a unit file.
fn cmd_generate_service(config: Option<&Path>, system: bool) -> anyhow::Result<()> {
    let binary_path = std::env::current_exe()
        .map_err(|e| anyhow::anyhow!("Failed to determine current binary path: {e}"))?;

    // Resolve any explicit --config to an absolute path so the unit isn't tied to
    // the install-time working directory.
    let config_path = config.map(|p| std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf()));

    let unit = generate_systemd_unit(&binary_path, config_path.as_deref(), system);
    print!("{unit}");

    eprintln!("# Generated systemd unit for kuiper-libvirt-agent.");
    eprintln!("# Binary: {}", binary_path.display());
    if system {
        eprintln!("# Mode:   --system (config /etc, logs /var/log, data /var/lib)");
    } else if let Some(cfg) = &config_path {
        eprintln!("# Config: {}", cfg.display());
    } else {
        eprintln!("# Config: /etc/kuiper-libvirt-agent/config.toml");
        eprintln!("#         (tip: pass --system for full FHS daemon paths)");
    }
    eprintln!("#");
    eprintln!("# Install it with, for example:");
    eprintln!(
        "#   kuiper-libvirt-agent generate-service > /etc/systemd/system/kuiper-libvirt-agent.service"
    );
    eprintln!("#   systemctl daemon-reload");
    eprintln!("#   systemctl enable --now kuiper-libvirt-agent");

    Ok(())
}

/// Escape a path for use as a single argument in a systemd `ExecStart=` line.
///
/// Implements the metacharacter rules from `man systemd.service` ("Command
/// Lines"): the line is split on whitespace, `%` introduces a specifier, `$`
/// introduces environment-variable expansion, and `"`/`'`/`\` drive quoting and
/// C-style escapes. Note `%` and `$` are expanded even inside quotes, so they
/// must be doubled regardless.
///
/// A clean path (none of those characters) is returned unchanged; otherwise it is
/// wrapped in double quotes with `\` and `"` C-escaped and `%`/`$` doubled, so
/// systemd treats it as one literal argument.
fn systemd_escape_arg(s: &str) -> String {
    let needs_quoting = s
        .chars()
        .any(|c| c.is_whitespace() || matches!(c, '"' | '\'' | '\\' | '%' | '$'));
    if !needs_quoting {
        return s.to_string();
    }

    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '%' => out.push_str("%%"),
            '$' => out.push_str("$$"),
            // A single quote is literal inside double quotes — no escaping needed.
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Build a systemd service unit for running the agent on startup.
///
/// When `system` is set the unit runs the agent with `--system` (FHS paths) and
/// declares `LogsDirectory`/`StateDirectory` so systemd creates `/var/log` and
/// `/var/lib` entries owned by the service user. Otherwise it passes an explicit
/// `--config` (the given path, or the conventional `/etc` location).
fn generate_systemd_unit(binary_path: &Path, config: Option<&Path>, system: bool) -> String {
    let binary = systemd_escape_arg(&binary_path.display().to_string());

    // Build the ExecStart argument list. Paths are escaped so a path containing
    // spaces or `%` is parsed by systemd as a single, literal argument.
    let mut exec_args = String::new();
    if system {
        exec_args.push_str(" --system");
    }
    // In --system mode the daemon already resolves config from /etc; only append
    // --config when the operator pinned a specific path (or in non-system mode).
    if let Some(cfg) = config {
        exec_args.push_str(&format!(
            " --config {}",
            systemd_escape_arg(&cfg.display().to_string())
        ));
    } else if !system {
        exec_args.push_str(" --config /etc/kuiper-libvirt-agent/config.toml");
    }

    // systemd-managed runtime directories (only meaningful in system mode).
    let runtime_dirs = if system {
        "# systemd creates these (owned by the service user) on start:\n\
         LogsDirectory=kuiper-libvirt-agent\n\
         StateDirectory=kuiper-libvirt-agent\n"
    } else {
        ""
    };

    format!(
        r#"[Unit]
Description=Kuiper libvirt Agent (ephemeral CI runner VMs)
Documentation=https://github.com/AstroHQ/kuiper-forge
After=network-online.target libvirtd.service
Wants=network-online.target

[Service]
Type=simple
ExecStart={binary}{exec_args}
Restart=on-failure
RestartSec=5
# The agent destroys all managed VMs on SIGTERM, first waiting out any still being created; give cleanup time to
# finish (each virsh/qemu-img step is capped at 2 minutes).
TimeoutStopSec=360
{runtime_dirs}# Recommended: run as a dedicated, non-root user with access to the config and
# certificates. Create one (e.g. `useradd --system --no-create-home kuiper`) and
# uncomment the lines below.
#User=kuiper
#Group=kuiper
# The user needs libvirt access for qemu:///system.
#SupplementaryGroups=libvirt

[Install]
WantedBy=multi-user.target
"#,
    )
}

/// Initialize logging with file output and stdout.
fn init_logging(log_dir: &Path, retention_days: usize) -> anyhow::Result<LogCapture> {
    std::fs::create_dir_all(log_dir)?;

    // Create a daily rotating file appender (e.g., kuiper-libvirt-agent.2026-01-15.log)
    let file_appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("kuiper-libvirt-agent")
        .filename_suffix("log")
        .max_log_files(retention_days)
        .build(log_dir)?;

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

#[cfg(test)]
mod tests {
    use super::*;

    const BIN: &str = "/usr/local/bin/kuiper-libvirt-agent";

    /// Extract the `ExecStart=` line from a unit (the `--system`/`--config`
    /// comments elsewhere in the file would otherwise foil substring checks).
    fn exec_start(unit: &str) -> &str {
        unit.lines()
            .find(|l| l.starts_with("ExecStart="))
            .expect("unit has an ExecStart line")
    }

    #[test]
    fn test_generate_systemd_unit_explicit_config() {
        let unit = generate_systemd_unit(Path::new(BIN), Some(Path::new("/srv/agent.toml")), false);

        assert_eq!(
            exec_start(&unit),
            format!("ExecStart={BIN} --config /srv/agent.toml")
        );
        assert!(!unit.contains("LogsDirectory"));
        assert!(unit.contains("WantedBy=multi-user.target"));
        assert!(unit.contains("Restart=on-failure"));
    }

    #[test]
    fn test_generate_systemd_unit_defaults_to_etc_config() {
        // No --config and not --system: pin the conventional /etc path explicitly.
        let unit = generate_systemd_unit(Path::new(BIN), None, false);
        assert_eq!(
            exec_start(&unit),
            format!("ExecStart={BIN} --config /etc/kuiper-libvirt-agent/config.toml")
        );
    }

    #[test]
    fn test_generate_systemd_unit_system_mode() {
        // --system uses FHS paths internally, so no --config is emitted, and
        // systemd manages the log/state directories.
        let unit = generate_systemd_unit(Path::new(BIN), None, true);
        assert_eq!(exec_start(&unit), format!("ExecStart={BIN} --system"));
        assert!(unit.contains("LogsDirectory=kuiper-libvirt-agent"));
        assert!(unit.contains("StateDirectory=kuiper-libvirt-agent"));
    }

    #[test]
    fn test_generate_systemd_unit_system_mode_with_explicit_config() {
        // An explicit --config is honored even alongside --system.
        let unit = generate_systemd_unit(Path::new(BIN), Some(Path::new("/srv/agent.toml")), true);
        assert_eq!(
            exec_start(&unit),
            format!("ExecStart={BIN} --system --config /srv/agent.toml")
        );
    }

    #[test]
    fn test_generate_systemd_unit_quotes_config_path_with_spaces() {
        // A config path with spaces must become a single quoted argument so
        // systemd doesn't split it into multiple args.
        let unit = generate_systemd_unit(
            Path::new(BIN),
            Some(Path::new("/opt/Kuiper Agent/config.toml")),
            false,
        );
        assert_eq!(
            exec_start(&unit),
            format!("ExecStart={BIN} --config \"/opt/Kuiper Agent/config.toml\"")
        );
    }

    #[test]
    fn test_generate_systemd_unit_quotes_binary_path_with_spaces() {
        let unit = generate_systemd_unit(Path::new("/opt/Kuiper Agent/bin"), None, true);
        assert_eq!(
            exec_start(&unit),
            "ExecStart=\"/opt/Kuiper Agent/bin\" --system"
        );
    }

    #[test]
    fn test_systemd_escape_arg() {
        // Clean paths are left untouched.
        assert_eq!(systemd_escape_arg("/usr/bin/agent"), "/usr/bin/agent");
        // Spaces force quoting.
        assert_eq!(systemd_escape_arg("/a b/c"), "\"/a b/c\"");
        // `%` is a systemd specifier and must be doubled (and triggers quoting).
        assert_eq!(systemd_escape_arg("/a%b"), "\"/a%%b\"");
        // `$` triggers env-var expansion and must be doubled.
        assert_eq!(systemd_escape_arg("/a$b"), "\"/a$$b\"");
        // Backslash and double-quote are C-escaped inside the quotes.
        assert_eq!(systemd_escape_arg("a\\b\"c d"), "\"a\\\\b\\\"c d\"");
        // A single quote forces quoting but is literal inside double quotes.
        assert_eq!(systemd_escape_arg("/a'b"), "\"/a'b\"");
    }
}
