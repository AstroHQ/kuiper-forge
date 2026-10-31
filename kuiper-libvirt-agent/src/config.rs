//! Configuration loading for kuiper-libvirt-agent.
//!
//! Configuration is loaded from a TOML file. See examples/kuiper-libvirt-agent-config.toml for every option.

use kuiper_agent_lib::runner_cache::RunnerCacheConfig;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Main configuration structure for the libvirt agent.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub coordinator: CoordinatorConfig,
    pub tls: TlsConfig,
    pub agent: AgentConfig,
    #[serde(default)]
    pub libvirt: LibvirtConfig,
    pub vm: VmConfig,
    pub ssh: SshConfig,
    /// Cleanup settings (optional)
    #[serde(default)]
    pub cleanup: CleanupConfig,
    /// Logging settings (optional)
    #[serde(default)]
    pub logging: LoggingConfig,
    /// Runner download cache settings (optional)
    #[serde(default)]
    pub runner_cache: RunnerCacheConfig,
}

/// Coordinator connection configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CoordinatorConfig {
    /// gRPC endpoint URL (e.g., "https://coordinator.example.com:9443")
    pub url: String,
    /// Hostname for TLS verification
    pub hostname: String,
}

/// TLS certificate configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct TlsConfig {
    /// Path to server CA certificate from coordinator (optional)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ca_cert: Option<PathBuf>,
    /// Directory for client certificates (auto-populated after registration)
    pub certs_dir: PathBuf,
}

/// Agent-specific configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AgentConfig {
    /// Labels this agent advertises to the coordinator
    pub labels: Vec<String>,
}

/// Where runner VMs run.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LibvirtConfig {
    /// libvirt connection URI
    #[serde(default = "default_uri")]
    pub uri: String,
    /// libvirt network for the VMs' NIC (e.g. the "default" NAT network)
    #[serde(default = "default_network")]
    pub network: String,
    /// Host bridge for the VMs' NIC instead of a libvirt network (e.g. "br0"). Use `ip_source = "agent"` with it,
    /// libvirt doesn't see DHCP leases on a bridge
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bridge: Option<String>,
    /// Where VM disk overlays and domain XML go. Empty means `<data dir>/vms`. The libvirt QEMU user must be able
    /// to reach it (and the base images)
    #[serde(default)]
    pub disk_dir: PathBuf,
    /// How to find a VM's IP: "lease" (libvirt network DHCP), "agent" (qemu-guest-agent in the image), or "any"
    /// (lease, then agent)
    #[serde(default)]
    pub ip_source: IpSource,
}

fn default_uri() -> String {
    "qemu:///system".to_string()
}

fn default_network() -> String {
    "default".to_string()
}

impl Default for LibvirtConfig {
    fn default() -> Self {
        Self {
            uri: default_uri(),
            network: default_network(),
            bridge: None,
            disk_dir: PathBuf::new(),
            ip_source: IpSource::default(),
        }
    }
}

/// Where `virsh domifaddr` gets a VM's addresses.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum IpSource {
    Lease,
    Agent,
    #[default]
    Any,
}

/// What the VM for an image looks like, so the agent can write its domain XML. Set on `[vm]` for `vm.image` and on
/// each image mapping for its image. A mapping doesn't inherit these from `[vm]`, they describe the image.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct Hardware {
    /// Guest OS: "linux", or "windows" (Hyper-V enlightenments, clock in local time)
    #[serde(default, skip_serializing_if = "is_default")]
    pub os: GuestOs,
    /// "bios", or "uefi" (OVMF, libvirt picks the build)
    #[serde(default, skip_serializing_if = "is_default")]
    pub firmware: Firmware,
    /// UEFI variable store each VM starts from, e.g. a copy of a Proxmox VM's EFI disk to keep its Secure Boot keys
    /// and boot entries. The agent needs to be able to read it. Defaults to the firmware's own
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uefi_vars: Option<PathBuf>,
    /// UEFI Secure Boot, q35 only. Without `uefi_vars` the VM starts with Microsoft's keys enrolled
    #[serde(default, skip_serializing_if = "is_default")]
    pub secure_boot: bool,
    /// Machine type: "pc" (i440fx, Proxmox's default) or "q35". Defaults to libvirt's, "pc" on x86
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine: Option<Machine>,
    /// Disk bus: "virtio" (virtio-blk) or "scsi" (virtio-scsi, Proxmox's default). The image needs the driver,
    /// Windows won't boot from a bus it has no driver for
    #[serde(default, skip_serializing_if = "is_default")]
    pub disk_bus: DiskBus,
    /// Emulated TPM 2.0, fresh for each VM
    #[serde(default, skip_serializing_if = "is_default")]
    pub tpm: bool,
    /// Domain XML template to use instead of the generated domain, for what the settings above can't do.
    /// `{name}`, `{disk}`, `{nvram}`, `{vcpus}`, `{memory_mib}`, `{interface}` and `{metadata}` are filled in.
    /// `{metadata}` must be inside `<metadata>`, it's how the agent knows the VM is its own. `{nvram}` is a per-VM
    /// UEFI variable store path the agent cleans up, for `<nvram template='...'>{nvram}</nvram>`
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub domain_template: Option<PathBuf>,
}

fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    *value == T::default()
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum GuestOs {
    #[default]
    Linux,
    Windows,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Firmware {
    #[default]
    Bios,
    Uefi,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Machine {
    Pc,
    Q35,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DiskBus {
    #[default]
    Virtio,
    Scsi,
}

impl Hardware {
    /// Settings that don't go together. `what` names where they're set, for the message.
    fn validate(&self, what: &str) -> Result<(), ConfigError> {
        let invalid = |msg: &str| Err(ConfigError::ValidationError(format!("{what}: {msg}")));
        if self.domain_template.is_some()
            && *self
                != (Hardware {
                    domain_template: self.domain_template.clone(),
                    ..Hardware::default()
                })
        {
            return invalid(
                "domain_template replaces the generated domain, set it or the other VM settings, not both",
            );
        }
        if self.firmware != Firmware::Uefi && (self.uefi_vars.is_some() || self.secure_boot) {
            return invalid("uefi_vars and secure_boot need firmware = \"uefi\"");
        }
        if self.secure_boot && self.machine != Some(Machine::Q35) {
            return invalid("libvirt only does Secure Boot on machine = \"q35\"");
        }
        if let Some(vars) = &self.uefi_vars
            && !vars.is_absolute()
        {
            return invalid(&format!(
                "uefi_vars {} must be an absolute path",
                vars.display()
            ));
        }
        Ok(())
    }
}

/// A label-to-image mapping rule for selecting base images based on job labels.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ImageMapping {
    /// Labels that must ALL be present in job labels for this mapping to match
    pub labels: Vec<String>,
    /// Base disk image (absolute path) to use when this mapping matches
    pub image: PathBuf,
    /// Runners to keep for this mapping in fixed-capacity mode. Setting it on any mapping means unset ones get none
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool: Option<u32>,
    /// vCPUs for this image's VMs, `vm.vcpus` if unset
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vcpus: Option<u32>,
    /// Memory in MiB for this image's VMs, `vm.memory_mib` if unset
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_mib: Option<u32>,
    /// This image's VM hardware
    #[serde(flatten)]
    pub hardware: Hardware,
}

impl kuiper_agent_lib::labels::LabelMapping for ImageMapping {
    fn labels(&self) -> &[String] {
        &self.labels
    }

    fn pool(&self) -> Option<u32> {
        self.pool
    }

    fn id(&self) -> String {
        self.image.display().to_string()
    }
}

/// VM configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct VmConfig {
    /// Default base disk image (absolute path) to clone from, used when no image mapping matches. Each VM gets a
    /// copy-on-write qcow2 overlay of it, the image itself is never written
    pub image: PathBuf,
    /// Maximum concurrent VMs
    #[serde(default = "default_max_vms")]
    pub max_vms: u32,
    /// vCPUs per VM
    #[serde(default = "default_vcpus")]
    pub vcpus: u32,
    /// Memory per VM in MiB
    #[serde(default = "default_memory_mib")]
    pub memory_mib: u32,
    /// `vm.image`'s VM hardware
    #[serde(flatten)]
    pub hardware: Hardware,
    /// Timeout in seconds for VM to get an IP address and accept SSH
    #[serde(default = "default_ip_timeout")]
    pub ip_timeout_secs: u64,
    /// Image mappings for label-based selection (first match wins).
    /// Skipped when empty so the generated config can carry a commented example
    /// (see `IMAGE_MAPPINGS_HELP`) without a duplicate-key TOML conflict.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub image_mappings: Vec<ImageMapping>,
    /// GitHub Actions runner version to install (e.g., "2.321.0")
    #[serde(default = "default_runner_version")]
    pub runner_version: String,
    /// forgejo-runner version to install for Forgejo jobs (e.g., "13.2.0", at least 13.1.0).
    /// "latest" means the version this agent release pins, since builds come from kuiper-forge's releases
    #[serde(default = "default_runner_version")]
    pub forgejo_runner_version: String,
    /// Where to download forgejo-runner from: a URL template with `{version}`, `{os}`, `{arch}` and `{ext}`,
    /// with a `.sha256` next to each binary. Defaults to kuiper-forge's GitHub releases
    #[serde(default = "default_forgejo_runner_url")]
    pub forgejo_runner_url: String,
}

fn default_forgejo_runner_url() -> String {
    kuiper_agent_lib::forgejo_runner::DEFAULT_URL_TEMPLATE.to_string()
}

fn default_max_vms() -> u32 {
    2
}

fn default_vcpus() -> u32 {
    4
}

fn default_memory_mib() -> u32 {
    8192
}

fn default_ip_timeout() -> u64 {
    180
}

fn default_runner_version() -> String {
    "latest".to_string()
}

/// Commented reference appended to the config that `register` generates, so
/// label-based image selection is discoverable without digging up the full
/// example file. All lines are comments; uncomment to use.
pub const IMAGE_MAPPINGS_HELP: &str = r#"
# -----------------------------------------------------------------------------
# Optional: serve multiple base images from one agent
# -----------------------------------------------------------------------------
# Every runner clones `vm.image` by default. To pick a different image per job,
# add one or more [[vm.image_mappings]] rules below. A rule matches when ALL of
# its `labels` are present in the job's labels (case-insensitive); the first
# match wins, otherwise `vm.image` is used.
#
# Each mapping is also advertised to the coordinator as a capability: the agent
# offers `agent.labels` PLUS each mapping's labels as a distinct label set, so
# jobs for any mapped image route here automatically.
#
# In FIXED-CAPACITY mode, set `pool` on a mapping to keep that many runners of
# it. Once any mapping has `pool`, mappings without it get none.
#
# [[vm.image_mappings]]
# labels = ["ubuntu-22.04"]
# image = "/var/lib/libvirt/images/ubuntu-22.04-runner.qcow2"
# pool = 1
#
# [[vm.image_mappings]]
# labels = ["debian-12"]
# image = "/var/lib/libvirt/images/debian-12-runner.qcow2"
#
# Each image says what its VM looks like (defaults: Linux, BIOS, virtio disk).
# E.g. Windows exported from Proxmox, with its EFI disk copied alongside:
#
# [[vm.image_mappings]]
# labels = ["windows"]
# image = "/var/lib/libvirt/images/win11-ci.qcow2"
# os = "windows"
# firmware = "uefi"
# uefi_vars = "/var/lib/libvirt/images/win11-ci_VARS.fd"
# machine = "pc"
# disk_bus = "scsi"
#
# Full reference: examples/kuiper-libvirt-agent-config.toml
"#;

/// SSH configuration for connecting to VMs.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SshConfig {
    /// Path to SSH private key (optional if password is provided)
    #[serde(default)]
    pub private_key_path: Option<PathBuf>,
    /// SSH password (optional if private_key_path is provided)
    #[serde(default)]
    pub password: Option<String>,
    /// SSH username
    #[serde(default = "default_ssh_username")]
    pub username: String,
    /// SSH port
    #[serde(default = "default_ssh_port")]
    pub port: u16,
    /// Connection timeout in seconds
    #[serde(default = "default_ssh_timeout")]
    pub timeout_secs: u64,
    /// Number of connection retries
    #[serde(default = "default_ssh_retries")]
    pub retries: u32,
}

fn default_ssh_username() -> String {
    "ci".to_string()
}

fn default_ssh_port() -> u16 {
    22
}

fn default_ssh_timeout() -> u64 {
    30
}

fn default_ssh_retries() -> u32 {
    10
}

/// Cleanup configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct CleanupConfig {
    /// Maximum VM age in hours before forced cleanup
    #[serde(default = "default_max_vm_age_hours")]
    pub max_vm_age_hours: u32,
    /// How often to run cleanup (minutes)
    #[serde(default = "default_cleanup_interval_mins")]
    pub cleanup_interval_mins: u32,
}

fn default_max_vm_age_hours() -> u32 {
    2
}

fn default_cleanup_interval_mins() -> u32 {
    15
}

impl Default for CleanupConfig {
    fn default() -> Self {
        Self {
            max_vm_age_hours: default_max_vm_age_hours(),
            cleanup_interval_mins: default_cleanup_interval_mins(),
        }
    }
}

/// Logging configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LoggingConfig {
    /// Number of days to retain log files
    #[serde(default = "default_log_retention_days")]
    pub retention_days: u32,
    /// Upload this agent's own log lines to the coordinator (shown in its admin UI). Runner logs are never sent
    #[serde(default = "default_true")]
    pub upload: bool,
}

fn default_true() -> bool {
    true
}

fn default_log_retention_days() -> u32 {
    14
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            retention_days: default_log_retention_days(),
            upload: true,
        }
    }
}

impl Config {
    /// Load configuration from a TOML file. An empty `libvirt.disk_dir` becomes `<data_dir>/vms`.
    pub fn load<P: AsRef<Path>>(path: P, data_dir: &Path) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path.as_ref()).map_err(|e| {
            ConfigError::IoError(format!(
                "Failed to read config file {:?}: {}",
                path.as_ref(),
                e
            ))
        })?;

        let mut config: Config = toml::from_str(&content)
            .map_err(|e| ConfigError::ParseError(format!("Failed to parse config: {e}")))?;

        // Expand ~ in paths
        config.tls.ca_cert = config.tls.ca_cert.map(|p| expand_tilde(&p));
        config.tls.certs_dir = expand_tilde(&config.tls.certs_dir);
        config.ssh.private_key_path = config.ssh.private_key_path.map(|p| expand_tilde(&p));
        for hardware in std::iter::once(&mut config.vm.hardware)
            .chain(config.vm.image_mappings.iter_mut().map(|m| &mut m.hardware))
        {
            hardware.domain_template = hardware.domain_template.as_deref().map(expand_tilde);
            hardware.uefi_vars = hardware.uefi_vars.as_deref().map(expand_tilde);
        }
        config.libvirt.disk_dir = if config.libvirt.disk_dir.as_os_str().is_empty() {
            data_dir.join("vms")
        } else {
            expand_tilde(&config.libvirt.disk_dir)
        };

        config.validate()?;

        Ok(config)
    }

    /// Load configuration from the default location.
    ///
    /// Searches in order:
    /// 1. `./kuiper-libvirt-agent.toml` (current directory)
    /// 2. `~/.config/kuiper-libvirt-agent/config.toml` (user config)
    /// 3. `/etc/kuiper-libvirt-agent/config.toml` (system config)
    pub fn load_default(data_dir: &Path) -> Result<Self, ConfigError> {
        let candidates = Self::config_search_paths();

        for path in &candidates {
            if path.exists() {
                tracing::info!("Loading config from {:?}", path);
                return Self::load(path, data_dir);
            }
        }

        Err(ConfigError::NotFound(format!(
            "No config file found. Searched: {candidates:?}"
        )))
    }

    /// Get the list of paths to search for config files.
    pub fn config_search_paths() -> Vec<PathBuf> {
        let mut paths = vec![PathBuf::from("kuiper-libvirt-agent.toml")];
        if let Some(config_dir) = dirs::config_dir() {
            paths.push(config_dir.join("kuiper-libvirt-agent").join("config.toml"));
        }
        paths.push(Self::system_config_path());
        paths
    }

    /// Get the default data directory, `~/.local/share/kuiper-libvirt-agent/`.
    pub fn default_data_dir() -> PathBuf {
        dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("kuiper-libvirt-agent")
    }

    /// Get the default log directory for the per-user layout (`<data_dir>/logs`).
    pub fn default_log_dir() -> PathBuf {
        Self::default_data_dir().join("logs")
    }

    /// Get the default configuration file path.
    pub fn default_config_path() -> PathBuf {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("kuiper-libvirt-agent")
            .join("config.toml")
    }

    /// System (FHS) config path used when running as a `--system` daemon.
    pub fn system_config_path() -> PathBuf {
        PathBuf::from("/etc/kuiper-libvirt-agent/config.toml")
    }

    /// System (FHS) data directory (certs, VM disks) for `--system` daemon mode.
    ///
    /// Matches systemd's `StateDirectory=kuiper-libvirt-agent`.
    pub fn system_data_dir() -> PathBuf {
        PathBuf::from("/var/lib/kuiper-libvirt-agent")
    }

    /// System (FHS) log directory for `--system` daemon mode.
    ///
    /// Matches systemd's `LogsDirectory=kuiper-libvirt-agent`.
    pub fn system_log_dir() -> PathBuf {
        PathBuf::from("/var/log/kuiper-libvirt-agent")
    }

    /// Generate a template configuration for initial registration.
    pub fn generate_template(
        coordinator_url: String,
        coordinator_hostname: String,
        certs_dir: PathBuf,
    ) -> Self {
        Self {
            coordinator: CoordinatorConfig {
                url: coordinator_url,
                hostname: coordinator_hostname,
            },
            tls: TlsConfig {
                ca_cert: Some(certs_dir.join("ca.crt")),
                certs_dir,
            },
            agent: AgentConfig { labels: vec![] },
            libvirt: LibvirtConfig::default(),
            vm: VmConfig {
                image: PathBuf::new(), // User must set
                max_vms: default_max_vms(),
                vcpus: default_vcpus(),
                memory_mib: default_memory_mib(),
                hardware: Hardware::default(),
                ip_timeout_secs: default_ip_timeout(),
                image_mappings: vec![],
                runner_version: default_runner_version(),
                forgejo_runner_version: default_runner_version(),
                forgejo_runner_url: default_forgejo_runner_url(),
            },
            ssh: SshConfig {
                private_key_path: None, // User must set
                password: None,
                username: default_ssh_username(),
                port: default_ssh_port(),
                timeout_secs: default_ssh_timeout(),
                retries: default_ssh_retries(),
            },
            cleanup: CleanupConfig::default(),
            logging: LoggingConfig::default(),
            runner_cache: RunnerCacheConfig::default(),
        }
    }

    /// Save configuration to a TOML file.
    pub fn save<P: AsRef<Path>>(&self, path: P) -> Result<(), ConfigError> {
        if let Some(parent) = path.as_ref().parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                ConfigError::IoError(format!("Failed to create config directory: {e}"))
            })?;
        }

        let content = toml::to_string_pretty(self)
            .map_err(|e| ConfigError::ParseError(format!("Failed to serialize config: {e}")))?;

        std::fs::write(path.as_ref(), content).map_err(|e| {
            ConfigError::IoError(format!(
                "Failed to write config file {:?}: {e}",
                path.as_ref()
            ))
        })?;

        Ok(())
    }

    /// Validate the configuration.
    fn validate(&self) -> Result<(), ConfigError> {
        let mut errors = Vec::new();

        if self.agent.labels.is_empty() {
            errors.push(
                "agent.labels: Labels to identify this agent (e.g., [\"libvirt\", \"linux\"])",
            );
        }
        if self.vm.image.as_os_str().is_empty() {
            errors.push("vm.image: Base disk image to clone (e.g., \"/var/lib/libvirt/images/runner.qcow2\")");
        }
        if self.ssh.private_key_path.is_none() && self.ssh.password.is_none() {
            errors.push("ssh.private_key_path or ssh.password: SSH authentication method required");
        }

        if !errors.is_empty() {
            return Err(ConfigError::ValidationError(format!(
                "Configuration incomplete\n\nPlease edit the config file and set:\n  - {}\n\nThen start the agent:\n  kuiper-libvirt-agent",
                errors.join("\n  - ")
            )));
        }

        if !self.coordinator.url.starts_with("https://") {
            return Err(ConfigError::ValidationError(
                "Coordinator URL must use https://".to_string(),
            ));
        }

        // overlays point at their base image by path, a relative one would resolve against the overlay's dir
        let images =
            std::iter::once(&self.vm.image).chain(self.vm.image_mappings.iter().map(|m| &m.image));
        for image in images {
            if !image.is_absolute() {
                return Err(ConfigError::ValidationError(format!(
                    "Image {} must be an absolute path",
                    image.display()
                )));
            }
        }

        if self.vm.max_vms == 0 {
            return Err(ConfigError::ValidationError(
                "vm.max_vms must be at least 1".to_string(),
            ));
        }
        let sizes = std::iter::once((Some(self.vm.vcpus), Some(self.vm.memory_mib))).chain(
            self.vm
                .image_mappings
                .iter()
                .map(|m| (m.vcpus, m.memory_mib)),
        );
        if sizes
            .into_iter()
            .any(|(vcpus, memory)| vcpus == Some(0) || memory == Some(0))
        {
            return Err(ConfigError::ValidationError(
                "vcpus and memory_mib must be at least 1".to_string(),
            ));
        }

        self.vm.hardware.validate("vm")?;
        for mapping in &self.vm.image_mappings {
            mapping
                .hardware
                .validate(&format!("image mapping {}", mapping.image.display()))?;
        }

        Ok(())
    }
}

/// Configuration errors.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("Config file not found: {0}")]
    NotFound(String),

    #[error("IO error: {0}")]
    IoError(String),

    #[error("Parse error: {0}")]
    ParseError(String),

    #[error("Validation error: {0}")]
    ValidationError(String),
}

/// Expand ~ to the user's home directory.
fn expand_tilde(path: &Path) -> PathBuf {
    if let Some(path_str) = path.to_str()
        && path_str.starts_with("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(&path_str[2..]);
    }
    path.to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = r#"
[coordinator]
url = "https://coordinator.example.com:9443"
hostname = "coordinator.example.com"

[tls]
certs_dir = "/var/lib/kuiper-libvirt-agent/certs"

[agent]
labels = ["self-hosted", "linux", "x64"]

[vm]
image = "/var/lib/libvirt/images/runner.qcow2"

[ssh]
private_key_path = "/etc/kuiper-libvirt-agent/id_ed25519"
"#;

    fn load(toml: &str) -> Result<Config, ConfigError> {
        let dir =
            std::env::temp_dir().join(format!("kuiper-libvirt-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(format!("{}.toml", toml.len()));
        std::fs::write(&path, toml).unwrap();
        Config::load(&path, Path::new("/var/lib/kuiper-libvirt-agent"))
    }

    #[test]
    fn test_parse_minimal_config() {
        let config = load(MINIMAL).expect("minimal config loads");
        assert_eq!(config.libvirt.uri, "qemu:///system");
        assert_eq!(config.libvirt.network, "default");
        assert_eq!(config.libvirt.ip_source, IpSource::Any);
        assert_eq!(
            config.libvirt.disk_dir,
            PathBuf::from("/var/lib/kuiper-libvirt-agent/vms")
        );
        assert_eq!(config.vm.max_vms, 2);
        assert_eq!(config.ssh.username, "ci");
    }

    #[test]
    fn test_parse_full_config() {
        let toml = format!(
            "{MINIMAL}
[libvirt]
uri = \"qemu:///session\"
bridge = \"br0\"
disk_dir = \"/srv/vms\"
ip_source = \"agent\"
"
        );
        let config = load(&toml).expect("full config loads");
        assert_eq!(config.libvirt.bridge.as_deref(), Some("br0"));
        assert_eq!(config.libvirt.disk_dir, PathBuf::from("/srv/vms"));
        assert_eq!(config.libvirt.ip_source, IpSource::Agent);
    }

    #[test]
    fn test_relative_image_rejected() {
        let toml = MINIMAL.replace("/var/lib/libvirt/images/runner.qcow2", "runner.qcow2");
        let err = load(&toml).unwrap_err().to_string();
        assert!(err.contains("absolute path"), "{err}");
    }

    #[test]
    fn test_generated_config_documents_image_mappings() {
        let config = Config::generate_template(
            "https://coordinator.example.com:9443".to_string(),
            "coordinator.example.com".to_string(),
            PathBuf::from("/tmp/certs"),
        );
        let mut content = toml::to_string_pretty(&config).expect("serialize");
        assert!(!content.contains("image_mappings"));

        content.push_str(IMAGE_MAPPINGS_HELP);
        let _: Config = toml::from_str(&content).expect("generated config + help parses");

        let uncommented = content
            .replace("# [[vm.image_mappings]]", "[[vm.image_mappings]]")
            .replace("# labels = ", "labels = ")
            .replace("# image = ", "image = ");
        let parsed: Config = toml::from_str(&uncommented).expect("uncommented mappings parse");
        assert_eq!(parsed.vm.image_mappings.len(), 3);
    }

    const WINDOWS: &str = r#"
[[vm.image_mappings]]
labels = ["windows"]
image = "/var/lib/libvirt/images/win11-ci.qcow2"
os = "windows"
firmware = "uefi"
uefi_vars = "/var/lib/libvirt/images/win11-ci_VARS.fd"
machine = "pc"
disk_bus = "scsi"
memory_mib = 16384
"#;

    #[test]
    fn test_parse_windows_mapping() {
        let config = load(&format!("{MINIMAL}{WINDOWS}")).expect("windows mapping loads");
        assert_eq!(config.vm.hardware, Hardware::default());
        let mapping = &config.vm.image_mappings[0];
        assert_eq!(mapping.memory_mib, Some(16384));
        assert_eq!(mapping.vcpus, None);
        assert_eq!(
            mapping.hardware,
            Hardware {
                os: GuestOs::Windows,
                firmware: Firmware::Uefi,
                uefi_vars: Some(PathBuf::from("/var/lib/libvirt/images/win11-ci_VARS.fd")),
                secure_boot: false,
                machine: Some(Machine::Pc),
                disk_bus: DiskBus::Scsi,
                tpm: false,
                domain_template: None,
            }
        );
    }

    #[test]
    fn test_hardware_conflicts_rejected() {
        let cases = [
            (r#"uefi_vars = "/x_VARS.fd""#, "need firmware"),
            ("firmware = \"uefi\"\nsecure_boot = true", "q35"),
            ("firmware = \"uefi\"\nuefi_vars = \"x_VARS.fd\"", "absolute"),
            (
                "firmware = \"uefi\"\ndomain_template = \"/x.xml\"",
                "not both",
            ),
            ("os = \"macos\"", "unknown variant"),
        ];
        for (settings, expected) in cases {
            let toml = MINIMAL.replace(
                "image = \"/var/lib/libvirt/images/runner.qcow2\"",
                &format!("image = \"/var/lib/libvirt/images/runner.qcow2\"\n{settings}"),
            );
            let err = load(&toml).unwrap_err().to_string();
            assert!(err.contains(expected), "{settings}: {err}");
        }
    }
}
