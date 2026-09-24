//! VM Manager - handles VM lifecycle for the Tart agent.

use std::collections::{HashMap, HashSet};
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use kuiper_agent_proto::{CapacityLimit, VmInfo};
use serde::Deserialize;
use tokio::process::Command;
use tokio::sync::{Notify, RwLock};
use tokio::time::{Instant, timeout};

/// Timeout for tart CLI commands (clone, stop, delete, ip).
/// This prevents the agent from hanging indefinitely if tart gets stuck.
const TART_COMMAND_TIMEOUT: Duration = Duration::from_secs(120);

/// How often to re-count tart VMs this agent doesn't manage.
pub const EXTERNAL_POLL_INTERVAL: Duration = Duration::from_secs(15);

use tracing::{debug, error, info, warn};

use crate::config::TartConfig;
use crate::error::{Error, Result};
use crate::ssh::{self, SshConfig};

/// State of a managed VM.
#[derive(Debug, Clone)]
pub struct VmState {
    /// Unique VM identifier (same as name for Tart)
    pub vm_id: String,
    /// VM name
    pub name: String,
    /// Current state
    pub state: VmStatus,
    /// IP address (if known)
    pub ip_address: Option<Ipv4Addr>,
    /// Guest OS. From the image, or macOS until the clone tells us when the image wasn't pulled yet
    pub os: GuestOs,
    /// When the VM was created
    pub created_at: Instant,
}

/// VM status enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmStatus {
    /// VM is being created/cloned
    Creating,
    /// VM is booting
    Booting,
    /// VM is running
    Running,
    /// Runner is being configured
    ConfiguringRunner,
    /// Runner is executing a job
    RunnerActive,
    /// VM is being stopped/deleted
    Stopping,
}

impl VmStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            VmStatus::Creating => "creating",
            VmStatus::Booting => "booting",
            VmStatus::Running => "running",
            VmStatus::ConfiguringRunner => "configuring",
            VmStatus::RunnerActive => "runner_active",
            VmStatus::Stopping => "stopping",
        }
    }
}

impl From<&VmState> for VmInfo {
    fn from(state: &VmState) -> Self {
        VmInfo {
            vm_id: state.vm_id.clone(),
            name: state.name.clone(),
            state: state.state.as_str().to_string(),
            ip_address: state
                .ip_address
                .map(|ip| ip.to_string())
                .unwrap_or_default(),
        }
    }
}

/// Guest OS of a tart VM, from `tart get`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuestOs {
    /// Counts against the macOS limit too. The runner runs in the GUI session through Terminal.app
    MacOS,
    /// Only counts against the total limit. The runner runs headless
    Linux,
}

impl GuestOs {
    /// Anything tart doesn't call linux counts as macOS, so we don't overbook the stricter limit
    fn from_tart(os: &str) -> Self {
        if os.eq_ignore_ascii_case("linux") {
            GuestOs::Linux
        } else {
            GuestOs::MacOS
        }
    }

    /// Names of the `limits()` a VM with this OS counts against.
    pub fn limit_names(self) -> &'static [&'static str] {
        match self {
            GuestOs::MacOS => &[LIMIT_MACOS, LIMIT_TOTAL],
            GuestOs::Linux => &[LIMIT_TOTAL],
        }
    }
}

const LIMIT_MACOS: &str = "macos";
const LIMIT_TOTAL: &str = "total";

/// VM counts against the host-wide limits.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VmCounts {
    pub macos: u32,
    /// Any OS, macOS included
    pub total: u32,
}

impl VmCounts {
    fn add(&mut self, os: GuestOs) {
        self.total += 1;
        if os == GuestOs::MacOS {
            self.macos += 1;
        }
    }

    fn of<'a>(vms: impl IntoIterator<Item = &'a VmState>) -> Self {
        let mut counts = Self::default();
        for vm in vms {
            counts.add(vm.os);
        }
        counts
    }
}

#[derive(Deserialize)]
struct TartListEntry {
    #[serde(rename = "Name")]
    name: String,
    #[serde(rename = "Running", default)]
    running: bool,
}

#[derive(Deserialize)]
struct TartGetInfo {
    #[serde(rename = "OS")]
    os: String,
}

/// Manages Tart VMs for the agent.
pub struct VmManager {
    /// Tart configuration
    config: TartConfig,
    /// SSH configuration
    ssh_config: SshConfig,
    /// Active VMs tracked by ID
    active_vms: Arc<RwLock<HashMap<String, VmState>>>,
    /// Directory for runner log files
    log_dir: PathBuf,
    /// Notifier fired whenever the active VM set changes (insert/remove).
    /// Used by the agent's main loop to push immediate AgentStatus updates so
    /// the coordinator's view doesn't lag the agent's true capacity.
    state_changed: Arc<Notify>,
    /// Running tart VMs this agent doesn't manage (started by hand, another tool, a previous agent run), from the
    /// last `refresh_external`. They use up the same host-wide limits as runner VMs
    external: RwLock<VmCounts>,
    /// Guest OS per image, once `tart get` knows it. OCI images that aren't pulled yet stay unknown
    image_os: RwLock<HashMap<String, GuestOs>>,
}

impl VmManager {
    /// Create a new VM manager.
    pub fn new(config: TartConfig, ssh_config: SshConfig, log_dir: PathBuf) -> Self {
        Self {
            config,
            ssh_config,
            active_vms: Arc::new(RwLock::new(HashMap::new())),
            log_dir,
            state_changed: Arc::new(Notify::new()),
            external: RwLock::new(VmCounts::default()),
            image_os: RwLock::new(HashMap::new()),
        }
    }

    /// Get the notifier for active-VM-set changes. Each call to `notified()`
    /// awaits the next insert/remove on `active_vms`.
    pub fn state_changes(&self) -> Arc<Notify> {
        self.state_changed.clone()
    }

    /// Get the number of active VMs.
    pub async fn active_count(&self) -> usize {
        self.active_vms.read().await.len()
    }

    /// Get the number of slots free for a VM with this OS, after VMs this agent doesn't manage.
    pub async fn available_slots(&self, os: GuestOs) -> u32 {
        let external = *self.external.read().await;
        let own = VmCounts::of(self.active_vms.read().await.values());
        self.free_slots(os, own, external)
    }

    fn free_slots(&self, os: GuestOs, own: VmCounts, external: VmCounts) -> u32 {
        let total = self
            .config
            .max_total_vms
            .saturating_sub(own.total + external.total);
        match os {
            GuestOs::Linux => total,
            GuestOs::MacOS => total.min(
                self.config
                    .max_macos_vms
                    .saturating_sub(own.macos + external.macos),
            ),
        }
    }

    /// Get maximum capacity for VMs with this OS when nothing else is running on the host.
    pub fn max_vms(&self, os: GuestOs) -> u32 {
        match os {
            GuestOs::Linux => self.config.max_total_vms,
            GuestOs::MacOS => self.config.max_macos_vms.min(self.config.max_total_vms),
        }
    }

    /// The host-wide limits with current usage, for `AgentStatus`.
    pub async fn limits(&self) -> Vec<CapacityLimit> {
        let external = *self.external.read().await;
        let own = VmCounts::of(self.active_vms.read().await.values());
        vec![
            CapacityLimit {
                name: LIMIT_MACOS.to_string(),
                max: self.config.max_macos_vms,
                external: external.macos,
                active: own.macos,
            },
            CapacityLimit {
                name: LIMIT_TOTAL.to_string(),
                max: self.config.max_total_vms,
                external: external.total,
                active: own.total,
            },
        ]
    }

    /// Current usage against both limits, e.g. for a capacity rejection.
    pub async fn capacity_summary(&self) -> String {
        let external = *self.external.read().await;
        let own = VmCounts::of(self.active_vms.read().await.values());
        self.format_capacity(own, external)
    }

    fn format_capacity(&self, own: VmCounts, external: VmCounts) -> String {
        format!(
            "macOS {}/{} ({} external), total {}/{} ({} external)",
            own.macos + external.macos,
            self.config.max_macos_vms,
            external.macos,
            own.total + external.total,
            self.config.max_total_vms,
            external.total,
        )
    }

    /// Guest OS of an image. An image `tart get` doesn't know yet (an OCI image that isn't pulled) counts as macOS
    /// until a clone of it tells us otherwise.
    pub async fn image_os(&self, image: &str) -> GuestOs {
        self.known_image_os(image).await.unwrap_or(GuestOs::MacOS)
    }

    async fn known_image_os(&self, image: &str) -> Option<GuestOs> {
        if let Some(os) = self.image_os.read().await.get(image) {
            return Some(*os);
        }
        match self.tart_os(image).await {
            Ok(os) => {
                let os = GuestOs::from_tart(&os);
                self.image_os.write().await.insert(image.to_string(), os);
                Some(os)
            }
            Err(e) => {
                debug!("OS of image {} not known yet: {}", image, e);
                None
            }
        }
    }

    /// Re-count running tart VMs this agent doesn't manage. Keeps the last count if tart fails.
    pub async fn refresh_external(&self) {
        let external = match self.count_external().await {
            Ok(external) => external,
            Err(e) => {
                warn!("Failed to count external tart VMs: {}", e);
                return;
            }
        };
        let old = std::mem::replace(&mut *self.external.write().await, external);
        if old != external {
            info!(
                "External tart VMs changed: {} macOS, {} total (was {} macOS, {} total)",
                external.macos, external.total, old.macos, old.total
            );
            self.state_changed.notify_one();
        }
    }

    async fn count_external(&self) -> Result<VmCounts> {
        // ours can start or finish while `tart list` runs, so take our names from both sides of it
        let mut ours: HashSet<String> = self.active_vms.read().await.keys().cloned().collect();
        let running = self.tart_running_vms().await?;
        ours.extend(self.active_vms.read().await.keys().cloned());

        let mut external = VmCounts::default();
        for name in running.iter().filter(|name| !ours.contains(*name)) {
            let os = match self.tart_os(name).await {
                Ok(os) => GuestOs::from_tart(&os),
                Err(e) => {
                    warn!("Couldn't get OS of tart VM {}, assuming macOS: {}", name, e);
                    GuestOs::MacOS
                }
            };
            external.add(os);
        }
        Ok(external)
    }

    /// Get current VM states.
    pub async fn get_vms(&self) -> Vec<VmInfo> {
        self.active_vms
            .read()
            .await
            .values()
            .map(VmInfo::from)
            .collect()
    }

    /// Create a new VM from a template.
    ///
    /// Returns the VM ID on success.
    pub async fn create_vm(&self, vm_name: &str, template: &str) -> Result<String> {
        let known_os = self.known_image_os(template).await;
        let os = known_os.unwrap_or(GuestOs::MacOS);

        // check and insert under one lock so two creates can't both take the last slot
        {
            let external = *self.external.read().await;
            let mut active = self.active_vms.write().await;
            let own = VmCounts::of(active.values());
            if self.free_slots(os, own, external) == 0 {
                return Err(Error::CapacityExceeded(self.format_capacity(own, external)));
            }
            if active.contains_key(vm_name) {
                return Err(Error::VmAlreadyRunning(vm_name.to_string()));
            }

            info!("Creating VM {} from template {}", vm_name, template);
            active.insert(
                vm_name.to_string(),
                VmState {
                    vm_id: vm_name.to_string(),
                    name: vm_name.to_string(),
                    state: VmStatus::Creating,
                    ip_address: None,
                    os,
                    created_at: Instant::now(),
                },
            );
        }
        self.state_changed.notify_one();

        // Clone the VM
        match self.tart_clone(template, vm_name).await {
            Ok(_) => {
                info!("VM {} cloned successfully", vm_name);
            }
            Err(e) => {
                // Remove from tracking on failure
                self.active_vms.write().await.remove(vm_name);
                self.state_changed.notify_one();
                return Err(e);
            }
        }

        // the clone pulled the image if it wasn't local, so now tart knows its OS
        if known_os.is_none() {
            self.learn_os_from_clone(vm_name, template).await;
        }

        // Start the VM
        self.update_state(vm_name, VmStatus::Booting).await;

        match self.tart_run(vm_name).await {
            Ok(_) => {
                info!("VM {} started", vm_name);
            }
            Err(e) => {
                // Cleanup on failure
                let _ = self.tart_delete(vm_name).await;
                self.active_vms.write().await.remove(vm_name);
                self.state_changed.notify_one();
                return Err(e);
            }
        }

        Ok(vm_name.to_string())
    }

    async fn learn_os_from_clone(&self, vm_name: &str, template: &str) {
        let os = match self.tart_os(vm_name).await {
            Ok(os) => GuestOs::from_tart(&os),
            Err(e) => {
                warn!("Couldn't get OS of VM {}, assuming macOS: {}", vm_name, e);
                return;
            }
        };
        info!("Image {} is {:?}", template, os);
        self.image_os.write().await.insert(template.to_string(), os);
        if let Some(state) = self.active_vms.write().await.get_mut(vm_name) {
            state.os = os;
        }
        self.state_changed.notify_one();
    }

    /// Wait for VM to be ready (IP available and SSH accessible).
    pub async fn wait_for_ready(&self, vm_id: &str, timeout: Duration) -> Result<Ipv4Addr> {
        let deadline = Instant::now() + timeout;

        info!(
            "Waiting for VM {} to be ready (timeout: {:?})",
            vm_id, timeout
        );

        // Poll for IP address
        while Instant::now() < deadline {
            if let Some(ip) = self.tart_ip(vm_id).await? {
                // Update state with IP
                {
                    let mut vms = self.active_vms.write().await;
                    if let Some(state) = vms.get_mut(vm_id) {
                        state.ip_address = Some(ip);
                        state.state = VmStatus::Running;
                    }
                }

                // Wait for SSH to be available
                let remaining = deadline - Instant::now();
                match ssh::wait_for_ssh(ip, remaining).await {
                    Ok(_) => {
                        info!("VM {} ready at {}", vm_id, ip);
                        return Ok(ip);
                    }
                    Err(Error::Timeout(_)) => {
                        // Continue waiting, IP might change
                        warn!("SSH not ready yet on {}, retrying...", ip);
                    }
                    Err(e) => return Err(e),
                }
            }

            tokio::time::sleep(Duration::from_secs(3)).await;
        }

        Err(Error::Timeout("waiting for VM to be ready"))
    }

    /// Configure the GitHub runner on a VM.
    ///
    /// If `jit_config` is non-empty, only installs the runner (config.sh is skipped —
    /// the JIT blob is passed to run.sh in `wait_for_runner_exit`).
    pub async fn configure_runner(
        &self,
        vm_id: &str,
        registration_token: &str,
        labels: &[String],
        runner_scope_url: &str,
        jit_config: &str,
    ) -> Result<()> {
        let (ip, os) = self.ip_and_os(vm_id).await?;

        self.update_state(vm_id, VmStatus::ConfiguringRunner).await;

        if !jit_config.is_empty() {
            // JIT path: only install the runner, skip config.sh.
            // The JIT blob will be passed to run.sh in wait_for_runner_exit.
            info!(
                "JIT mode: ensuring runner is installed on VM {} (skipping config.sh)",
                vm_id
            );
            ssh::ensure_runner_installed(ip, &self.ssh_config, &self.config.runner_version, os)
                .await?;
        } else {
            // Legacy path: install + config.sh
            ssh::configure_runner(
                ip,
                &self.ssh_config,
                registration_token,
                labels,
                runner_scope_url,
                vm_id,
                &self.config.runner_version,
                os,
            )
            .await?;
        }

        self.update_state(vm_id, VmStatus::RunnerActive).await;

        Ok(())
    }

    /// Wait for the runner to complete its job.
    ///
    /// On macOS the runner runs in the Terminal.app GUI context for macOS services (code signing, keychain,
    /// notarization). If `jit_config` is non-empty, it's written to the VM and passed to run.sh --jitconfig.
    pub async fn wait_for_runner_exit(&self, vm_id: &str, jit_config: &str) -> Result<()> {
        let (ip, os) = self.ip_and_os(vm_id).await?;

        info!(
            "Starting runner ({:?}) and waiting for completion on VM {}",
            os, vm_id
        );

        // Create log file path for this runner with date
        let timestamp = chrono::Local::now().format("%Y-%m-%d_%H%M%S");
        let log_file = self.log_dir.join(format!("runner-{vm_id}-{timestamp}.log"));

        ssh::start_runner_and_wait(ip, &self.ssh_config, &log_file, jit_config, os).await?;

        info!(
            "Runner completed on VM {} (log: {})",
            vm_id,
            log_file.display()
        );
        Ok(())
    }

    async fn ip_and_os(&self, vm_id: &str) -> Result<(Ipv4Addr, GuestOs)> {
        let vms = self.active_vms.read().await;
        vms.get(vm_id)
            .and_then(|s| Some((s.ip_address?, s.os)))
            .ok_or_else(|| Error::VmNotFound(vm_id.to_string()))
    }

    /// Destroy a VM.
    pub async fn destroy_vm(&self, vm_id: &str) -> Result<()> {
        info!("Destroying VM {}", vm_id);

        // Update state
        self.update_state(vm_id, VmStatus::Stopping).await;

        // Stop the VM (ignore errors - might already be stopped)
        let _ = self.tart_stop(vm_id).await;

        // Delete the VM
        match self.tart_delete(vm_id).await {
            Ok(_) => {
                info!("VM {} deleted", vm_id);
            }
            Err(e) => {
                warn!("Failed to delete VM {}: {}", vm_id, e);
            }
        }

        // Remove from tracking
        self.active_vms.write().await.remove(vm_id);
        self.state_changed.notify_one();

        Ok(())
    }

    /// Cleanup stale VMs that have exceeded the maximum age.
    pub async fn cleanup_stale_vms(&self, max_age: Duration) {
        let stale_vms: Vec<String> = {
            let vms = self.active_vms.read().await;
            vms.iter()
                .filter(|(_, state)| state.created_at.elapsed() > max_age)
                .map(|(id, _)| id.clone())
                .collect()
        };

        for vm_id in stale_vms {
            warn!("Cleaning up stale VM: {}", vm_id);
            if let Err(e) = self.destroy_vm(&vm_id).await {
                error!("Failed to cleanup stale VM {}: {}", vm_id, e);
            }
        }
    }

    /// Destroy all active VMs (for graceful shutdown).
    ///
    /// This stops and deletes all VMs currently tracked by this manager.
    /// Used during agent shutdown to clean up resources.
    pub async fn destroy_all_vms(&self) {
        let vm_ids: Vec<String> = {
            let vms = self.active_vms.read().await;
            vms.keys().cloned().collect()
        };

        if vm_ids.is_empty() {
            info!("No active VMs to cleanup");
            return;
        }

        info!("Destroying {} active VMs for shutdown...", vm_ids.len());

        for vm_id in vm_ids {
            info!("Destroying VM: {}", vm_id);
            if let Err(e) = self.destroy_vm(&vm_id).await {
                error!("Failed to destroy VM {} during shutdown: {}", vm_id, e);
            }
        }

        info!("All VMs destroyed");
    }

    /// Update VM state.
    async fn update_state(&self, vm_id: &str, status: VmStatus) {
        let mut vms = self.active_vms.write().await;
        if let Some(state) = vms.get_mut(vm_id) {
            debug!("VM {} state: {:?} -> {:?}", vm_id, state.state, status);
            state.state = status;
        }
    }

    // --- Tart CLI wrappers ---

    /// Clone a VM from template.
    async fn tart_clone(&self, source: &str, target: &str) -> Result<()> {
        let output = timeout(
            TART_COMMAND_TIMEOUT,
            Command::new("tart")
                .args(["clone", source, target])
                .output(),
        )
        .await
        .map_err(|_| Error::Timeout("tart clone"))??;

        if output.status.success() {
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(Error::CloneFailed(stderr.to_string()))
        }
    }

    /// Run a VM in headless mode.
    async fn tart_run(&self, name: &str) -> Result<()> {
        let mut cmd = Command::new("tart");
        cmd.args(["run", name, "--no-graphics"]);

        // Add shared directory if configured
        if let Some(ref cache_dir) = self.config.shared_cache_dir {
            cmd.arg("--dir")
                .arg(format!("cache:{}", cache_dir.display()));
        }

        // Spawn in background (detached)
        cmd.spawn()?;

        Ok(())
    }

    /// Stop a VM.
    async fn tart_stop(&self, name: &str) -> Result<()> {
        let output = timeout(
            TART_COMMAND_TIMEOUT,
            Command::new("tart").args(["stop", name]).output(),
        )
        .await
        .map_err(|_| Error::Timeout("tart stop"))??;

        // Ignore failure - VM might already be stopped
        if !output.status.success() {
            debug!(
                "tart stop {} returned non-zero (might be already stopped)",
                name
            );
        }

        Ok(())
    }

    /// Delete a VM.
    async fn tart_delete(&self, name: &str) -> Result<()> {
        let output = timeout(
            TART_COMMAND_TIMEOUT,
            Command::new("tart").args(["delete", name]).output(),
        )
        .await
        .map_err(|_| Error::Timeout("tart delete"))??;

        if output.status.success() {
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(Error::Tart(format!("delete failed: {stderr}")))
        }
    }

    /// Names of running local VMs (any owner).
    async fn tart_running_vms(&self) -> Result<Vec<String>> {
        let output = timeout(
            TART_COMMAND_TIMEOUT,
            Command::new("tart")
                .args(["list", "--source", "local", "--format", "json"])
                .output(),
        )
        .await
        .map_err(|_| Error::Timeout("tart list"))??;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::Tart(format!("list failed: {stderr}")));
        }
        let entries: Vec<TartListEntry> = serde_json::from_slice(&output.stdout)
            .map_err(|e| Error::Tart(format!("bad `tart list` output: {e}")))?;
        Ok(entries
            .into_iter()
            .filter(|e| e.running)
            .map(|e| e.name)
            .collect())
    }

    /// Guest OS of a VM, e.g. "darwin" or "linux".
    async fn tart_os(&self, name: &str) -> Result<String> {
        let output = timeout(
            TART_COMMAND_TIMEOUT,
            Command::new("tart")
                .args(["get", name, "--format", "json"])
                .output(),
        )
        .await
        .map_err(|_| Error::Timeout("tart get"))??;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(Error::Tart(format!("get failed: {stderr}")));
        }
        let info: TartGetInfo = serde_json::from_slice(&output.stdout)
            .map_err(|e| Error::Tart(format!("bad `tart get` output: {e}")))?;
        Ok(info.os)
    }

    /// Get the IP address of a VM.
    async fn tart_ip(&self, name: &str) -> Result<Option<Ipv4Addr>> {
        let output = timeout(
            TART_COMMAND_TIMEOUT,
            Command::new("tart").args(["ip", name]).output(),
        )
        .await
        .map_err(|_| Error::Timeout("tart ip"))??;

        if output.status.success() {
            let ip_str = String::from_utf8_lossy(&output.stdout);
            let ip_str = ip_str.trim();
            if !ip_str.is_empty() {
                match Ipv4Addr::from_str(ip_str) {
                    Ok(ip) => return Ok(Some(ip)),
                    Err(_) => {
                        warn!("Invalid IP address from tart: {}", ip_str);
                    }
                }
            }
        }

        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vm_status_as_str() {
        assert_eq!(VmStatus::Creating.as_str(), "creating");
        assert_eq!(VmStatus::Running.as_str(), "running");
        assert_eq!(VmStatus::RunnerActive.as_str(), "runner_active");
    }

    #[tokio::test]
    async fn test_vm_manager_capacity() {
        let config = TartConfig {
            base_image: "test".to_string(),
            max_macos_vms: 2,
            max_total_vms: 3,
            shared_cache_dir: None,
            ssh: Default::default(),
            runner_version: "latest".to_string(),
            image_mappings: Vec::new(),
        };
        let log_dir = std::env::temp_dir().join("kuiper-tart-agent-test-logs");
        let manager = VmManager::new(config, SshConfig::default(), log_dir);

        assert_eq!(manager.max_vms(GuestOs::MacOS), 2);
        assert_eq!(manager.max_vms(GuestOs::Linux), 3);
        assert_eq!(manager.available_slots(GuestOs::MacOS).await, 2);
        assert_eq!(manager.available_slots(GuestOs::Linux).await, 3);
        assert_eq!(manager.active_count().await, 0);
    }

    #[test]
    fn test_free_slots_counts_external_vms() {
        let config = TartConfig {
            base_image: "test".to_string(),
            max_macos_vms: 2,
            max_total_vms: 3,
            shared_cache_dir: None,
            ssh: Default::default(),
            runner_version: "latest".to_string(),
            image_mappings: Vec::new(),
        };
        let manager = VmManager::new(config, SshConfig::default(), std::env::temp_dir());
        let counts = |macos, total| VmCounts { macos, total };
        let none = VmCounts::default();
        let mac = GuestOs::MacOS;

        // one external mac leaves one mac slot
        assert_eq!(manager.free_slots(mac, none, counts(1, 1)), 1);
        assert_eq!(manager.free_slots(mac, counts(1, 1), counts(1, 1)), 0);

        // external linux VMs only use up the total limit
        assert_eq!(manager.free_slots(mac, none, counts(0, 2)), 1);
        assert_eq!(manager.free_slots(mac, none, counts(0, 3)), 0);

        // over the limit doesn't underflow
        assert_eq!(manager.free_slots(mac, counts(2, 2), counts(2, 4)), 0);
    }

    #[test]
    fn test_free_slots_linux_skips_macos_limit() {
        let config = TartConfig {
            base_image: "test".to_string(),
            max_macos_vms: 2,
            max_total_vms: 4,
            shared_cache_dir: None,
            ssh: Default::default(),
            runner_version: "latest".to_string(),
            image_mappings: Vec::new(),
        };
        let manager = VmManager::new(config, SshConfig::default(), std::env::temp_dir());
        let counts = |macos, total| VmCounts { macos, total };
        let none = VmCounts::default();

        // both mac slots taken: no more mac, linux still fits
        assert_eq!(manager.free_slots(GuestOs::MacOS, counts(2, 2), none), 0);
        assert_eq!(manager.free_slots(GuestOs::Linux, counts(2, 2), none), 2);

        // linux VMs use up mac slots only through the total limit
        assert_eq!(manager.free_slots(GuestOs::MacOS, counts(0, 3), none), 1);
        assert_eq!(
            manager.free_slots(GuestOs::Linux, counts(1, 3), counts(0, 1)),
            0
        );
    }

    #[test]
    fn test_guest_os_from_tart() {
        assert_eq!(GuestOs::from_tart("linux"), GuestOs::Linux);
        assert_eq!(GuestOs::from_tart("darwin"), GuestOs::MacOS);
        assert_eq!(GuestOs::from_tart(""), GuestOs::MacOS);
    }

    #[test]
    fn test_parse_tart_list() {
        let json = r#"[
            {"Name": "a", "Running": true, "State": "running", "Source": "local"},
            {"Name": "b", "Running": false, "State": "stopped", "Source": "local"}
        ]"#;
        let entries: Vec<TartListEntry> = serde_json::from_str(json).unwrap();
        let running: Vec<_> = entries
            .iter()
            .filter(|e| e.running)
            .map(|e| &e.name)
            .collect();
        assert_eq!(running, ["a"]);
    }
}
