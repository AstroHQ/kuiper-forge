//! VM Manager - handles VM lifecycle for the Tart agent.

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use kuiper_agent_proto::VmInfo;
use tokio::process::Command;
use tokio::sync::RwLock;
use tokio::time::Instant;
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
            ip_address: state.ip_address.map(|ip| ip.to_string()).unwrap_or_default(),
        }
    }
}

/// Manages Tart VMs for the agent.
pub struct VmManager {
    /// Tart configuration
    config: TartConfig,
    /// SSH configuration
    ssh_config: SshConfig,
    /// Maximum concurrent VMs
    max_concurrent: u32,
    /// Active VMs tracked by ID
    active_vms: Arc<RwLock<HashMap<String, VmState>>>,
    /// Directory for runner log files
    log_dir: PathBuf,
}

impl VmManager {
    /// Create a new VM manager.
    pub fn new(config: TartConfig, ssh_config: SshConfig, log_dir: PathBuf) -> Self {
        let max_concurrent = config.max_concurrent_vms;
        Self {
            config,
            ssh_config,
            max_concurrent,
            active_vms: Arc::new(RwLock::new(HashMap::new())),
            log_dir,
        }
    }

    /// Get the number of active VMs.
    pub async fn active_count(&self) -> usize {
        self.active_vms.read().await.len()
    }

    /// Get the number of available slots.
    pub async fn available_slots(&self) -> u32 {
        let active = self.active_vms.read().await.len() as u32;
        self.max_concurrent.saturating_sub(active)
    }

    /// Get maximum VM capacity.
    #[cfg(test)]
    pub fn max_vms(&self) -> u32 {
        self.max_concurrent
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
        // Check capacity
        {
            let active = self.active_vms.read().await;
            if active.len() >= self.max_concurrent as usize {
                return Err(Error::CapacityExceeded(self.max_concurrent));
            }
            if active.contains_key(vm_name) {
                return Err(Error::VmAlreadyRunning(vm_name.to_string()));
            }
        }

        info!("Creating VM {} from template {}", vm_name, template);

        // Track VM as creating
        let vm_state = VmState {
            vm_id: vm_name.to_string(),
            name: vm_name.to_string(),
            state: VmStatus::Creating,
            ip_address: None,
            created_at: Instant::now(),
        };
        self.active_vms
            .write()
            .await
            .insert(vm_name.to_string(), vm_state);

        // Clone the VM
        match self.tart_clone(template, vm_name).await {
            Ok(_) => {
                info!("VM {} cloned successfully", vm_name);
            }
            Err(e) => {
                // Remove from tracking on failure
                self.active_vms.write().await.remove(vm_name);
                return Err(e);
            }
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
                return Err(e);
            }
        }

        Ok(vm_name.to_string())
    }

    /// Wait for VM to be ready (IP available and SSH accessible).
    pub async fn wait_for_ready(&self, vm_id: &str, timeout: Duration) -> Result<Ipv4Addr> {
        let deadline = Instant::now() + timeout;

        info!("Waiting for VM {} to be ready (timeout: {:?})", vm_id, timeout);

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
    pub async fn configure_runner(
        &self,
        vm_id: &str,
        registration_token: &str,
        labels: &[String],
        runner_scope_url: &str,
    ) -> Result<()> {
        let ip = {
            let vms = self.active_vms.read().await;
            vms.get(vm_id)
                .and_then(|s| s.ip_address)
                .ok_or_else(|| Error::VmNotFound(vm_id.to_string()))?
        };

        self.update_state(vm_id, VmStatus::ConfiguringRunner).await;

        ssh::configure_runner(
            ip,
            &self.ssh_config,
            registration_token,
            labels,
            runner_scope_url,
            vm_id,
            &self.config.runner_version,
        )
        .await?;

        self.update_state(vm_id, VmStatus::RunnerActive).await;

        Ok(())
    }

    /// Wait for the runner to complete its job.
    pub async fn wait_for_runner_exit(&self, vm_id: &str) -> Result<()> {
        let ip = {
            let vms = self.active_vms.read().await;
            vms.get(vm_id)
                .and_then(|s| s.ip_address)
                .ok_or_else(|| Error::VmNotFound(vm_id.to_string()))?
        };

        info!("Starting runner and waiting for completion on VM {}", vm_id);

        // Create log file path for this runner with date
        let timestamp = chrono::Local::now().format("%Y-%m-%d_%H%M%S");
        let log_file = self.log_dir.join(format!("runner-{}-{}.log", vm_id, timestamp));

        // Start the runner and wait for it to complete, streaming output to log file
        ssh::start_runner_and_wait(ip, &self.ssh_config, &log_file).await?;

        info!("Runner completed on VM {} (log: {})", vm_id, log_file.display());
        Ok(())
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
        let output = Command::new("tart")
            .args(["clone", source, target])
            .output()
            .await?;

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
        let output = Command::new("tart")
            .args(["stop", name])
            .output()
            .await?;

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
        let output = Command::new("tart")
            .args(["delete", name])
            .output()
            .await?;

        if output.status.success() {
            Ok(())
        } else {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(Error::TartError(format!("delete failed: {}", stderr)))
        }
    }

    /// Get the IP address of a VM.
    async fn tart_ip(&self, name: &str) -> Result<Option<Ipv4Addr>> {
        let output = Command::new("tart")
            .args(["ip", name])
            .output()
            .await?;

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
            max_concurrent_vms: 2,
            shared_cache_dir: None,
            ssh: Default::default(),
            runner_version: "latest".to_string(),
        };
        let log_dir = std::env::temp_dir().join("kuiper-tart-agent-test-logs");
        let manager = VmManager::new(config, SshConfig::default(), log_dir);

        assert_eq!(manager.max_vms(), 2);
        assert_eq!(manager.available_slots().await, 2);
        assert_eq!(manager.active_count().await, 0);
    }
}
