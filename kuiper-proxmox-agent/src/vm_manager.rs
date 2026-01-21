//! VM lifecycle management for Proxmox VMs.
//!
//! This module handles:
//! - Cloning VMs from templates
//! - Starting VMs and waiting for IP addresses
//! - Configuring GitHub runners via SSH
//! - Monitoring runner completion
//! - Cleaning up VMs after use

use crate::config::{SshConfig, VmConfig};
use crate::error::{Error, Result};
use crate::ssh::{RunnerConfigBuilder, SshClient, SshSession};
use kuiper_proxmox_api::ProxmoxVEAPI;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

/// Information about a running VM.
#[derive(Debug, Clone)]
pub struct VmInstance {
    /// Proxmox VM ID
    pub vmid: u32,
    /// VM name
    pub name: String,
    /// IP address (once assigned)
    pub ip_address: Option<String>,
    /// Current state
    pub state: VmState,
    /// When the VM was created
    pub created_at: Instant,
}

/// State of a VM in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VmState {
    /// VM is being created (cloning)
    Creating,
    /// VM is starting up
    Starting,
    /// VM is running and ready
    Running,
    /// VM is configuring the runner
    Configuring,
    /// Runner is active and processing a job
    RunnerActive,
    /// VM is being destroyed
    Destroying,
}

impl VmState {
    /// Convert to string for status reporting.
    pub fn as_str(&self) -> &'static str {
        match self {
            VmState::Creating => "creating",
            VmState::Starting => "starting",
            VmState::Running => "running",
            VmState::Configuring => "configuring",
            VmState::RunnerActive => "runner_active",
            VmState::Destroying => "destroying",
        }
    }
}

/// Manages the lifecycle of Proxmox VMs.
pub struct VmManager {
    /// Proxmox API client
    proxmox: Arc<ProxmoxVEAPI>,
    /// VM configuration
    vm_config: VmConfig,
    /// SSH configuration
    ssh_config: SshConfig,
    /// Currently active VMs
    active_vms: Arc<RwLock<HashMap<u32, VmInstance>>>,
}

impl VmManager {
    /// Create a new VM manager.
    pub fn new(
        proxmox: Arc<ProxmoxVEAPI>,
        vm_config: VmConfig,
        ssh_config: SshConfig,
    ) -> Self {
        Self {
            proxmox,
            vm_config,
            ssh_config,
            active_vms: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Get the number of active VMs.
    pub async fn active_count(&self) -> usize {
        self.active_vms.read().await.len()
    }

    /// Get the number of available VM slots.
    pub async fn available_slots(&self) -> u32 {
        let active = self.active_count().await as u32;
        self.vm_config.concurrent_vms.saturating_sub(active)
    }

    /// Get information about all active VMs.
    pub async fn list_vms(&self) -> Vec<VmInstance> {
        self.active_vms.read().await.values().cloned().collect()
    }

    /// Check if there's capacity for another VM.
    pub async fn has_capacity(&self) -> bool {
        self.available_slots().await > 0
    }

    /// Create a new VM by cloning the template.
    ///
    /// Returns the VM ID of the new clone.
    pub async fn create_vm(&self, name: &str) -> Result<VmInstance> {
        if !self.has_capacity().await {
            return Err(Error::vm("No available VM slots"));
        }

        // Get next available VM ID
        let vmid = self.proxmox.get_next_vmid().await?;
        info!("Creating VM {} with ID {}", name, vmid);

        // Track the VM
        let instance = VmInstance {
            vmid,
            name: name.to_string(),
            ip_address: None,
            state: VmState::Creating,
            created_at: Instant::now(),
        };
        self.active_vms.write().await.insert(vmid, instance.clone());

        // Clone the template
        let storage = if self.vm_config.storage.is_empty() {
            None
        } else {
            Some(self.vm_config.storage.as_str())
        };

        let task = self.proxmox.clone_vm(
            self.vm_config.template_vmid,
            vmid,
            name,
            self.vm_config.linked_clone,
            storage,
        ).await.map_err(|e| {
            // Remove from tracking on failure
            let vms = self.active_vms.clone();
            tokio::spawn(async move {
                vms.write().await.remove(&vmid);
            });
            e
        })?;

        // Wait for clone to complete
        let clone_timeout = Duration::from_secs(self.vm_config.clone_timeout_secs);
        self.proxmox.wait_for_task_with_timeout(&task, clone_timeout).await.map_err(|e| {
            let vms = self.active_vms.clone();
            tokio::spawn(async move {
                vms.write().await.remove(&vmid);
            });
            e
        })?;

        info!("VM {} cloned successfully", vmid);
        Ok(VmInstance {
            vmid,
            name: name.to_string(),
            ip_address: None,
            state: VmState::Creating,
            created_at: Instant::now(),
        })
    }

    /// Start a VM and wait for it to be ready.
    ///
    /// Returns the IP address of the VM.
    pub async fn start_and_wait(&self, vmid: u32) -> Result<String> {
        // Update state
        if let Some(vm) = self.active_vms.write().await.get_mut(&vmid) {
            vm.state = VmState::Starting;
        }

        // Start the VM
        info!("Starting VM {}", vmid);
        let task = self.proxmox.start_vm(vmid).await?;
        self.proxmox.wait_for_task(&task).await?;

        // Wait for IP address
        let ip_timeout = Duration::from_secs(self.vm_config.ip_timeout_secs);
        info!("Waiting for VM {} to get IP address (timeout: {:?})", vmid, ip_timeout);
        let ip = self.proxmox.poll_for_ip(vmid, ip_timeout).await?;

        // Update tracking
        if let Some(vm) = self.active_vms.write().await.get_mut(&vmid) {
            vm.ip_address = Some(ip.clone());
            vm.state = VmState::Running;
        }

        info!("VM {} is ready at IP {}", vmid, ip);
        Ok(ip)
    }

    /// Wait for SSH to be available on a VM.
    pub async fn wait_for_ssh(&self, ip: &str) -> Result<()> {
        let ssh_client = SshClient::new(self.ssh_config.clone()).await?;
        let ssh_timeout = Duration::from_secs(self.ssh_config.timeout_secs);
        ssh_client.wait_for_ssh(ip, ssh_timeout).await
    }

    /// Configure the GitHub runner on a VM.
    pub async fn configure_runner(
        &self,
        vmid: u32,
        ip: &str,
        registration_token: &str,
        labels: &[String],
        runner_scope_url: &str,
        runner_name: &str,
    ) -> Result<()> {
        // Update state
        if let Some(vm) = self.active_vms.write().await.get_mut(&vmid) {
            vm.state = VmState::Configuring;
        }

        info!("Configuring runner on VM {} at {}", vmid, ip);

        // Connect via SSH
        let ssh_client = SshClient::new(self.ssh_config.clone()).await?;
        let mut session = ssh_client.connect(ip).await?;

        // Detect OS type
        let is_windows = self.detect_os_type(&mut session).await?;
        debug!("VM {} is running {}", vmid, if is_windows { "Windows" } else { "Linux" });

        // Build runner commands
        let runner_dir = if is_windows {
            r"C:\actions-runner"
        } else {
            "~/actions-runner"
        };

        let builder = if is_windows {
            RunnerConfigBuilder::windows(runner_dir)
        } else {
            RunnerConfigBuilder::linux(runner_dir)
        };

        // Configure runner
        let config_cmd = builder.config_command(
            runner_scope_url,
            registration_token,
            labels,
            runner_name,
        );

        info!("Running configuration command on VM {}", vmid);
        let output = session.execute(&config_cmd).await?;
        if output.exit_code != 0 {
            error!("Runner configuration failed: {}", output.stderr);
            return Err(Error::runner(format!(
                "Configuration failed with exit code {}: {}",
                output.exit_code, output.stderr
            )));
        }

        // Start runner in background
        info!("Starting runner on VM {}", vmid);
        let run_cmd = builder.run_command_background();
        session.execute_background(&run_cmd).await?;

        // Update state
        if let Some(vm) = self.active_vms.write().await.get_mut(&vmid) {
            vm.state = VmState::RunnerActive;
        }

        // Give runner time to start
        tokio::time::sleep(Duration::from_secs(5)).await;

        // Verify runner started
        if !session.is_runner_running().await? {
            return Err(Error::runner("Runner process failed to start"));
        }

        info!("Runner started successfully on VM {}", vmid);
        session.close().await?;

        Ok(())
    }

    /// Wait for the runner to complete.
    ///
    /// Polls the VM until the runner process exits.
    pub async fn wait_for_runner_completion(
        &self,
        vmid: u32,
        ip: &str,
        timeout: Duration,
    ) -> Result<()> {
        info!("Waiting for runner completion on VM {} (timeout: {:?})", vmid, timeout);

        let ssh_client = SshClient::new(self.ssh_config.clone()).await?;
        let start = std::time::Instant::now();
        let poll_interval = Duration::from_secs(10);

        while start.elapsed() < timeout {
            // Try to connect and check runner status
            match ssh_client.connect(ip).await {
                Ok(mut session) => {
                    match session.is_runner_running().await {
                        Ok(running) => {
                            if !running {
                                info!("Runner completed on VM {}", vmid);
                                let _ = session.close().await;
                                return Ok(());
                            }
                            debug!("Runner still running on VM {}", vmid);
                            let _ = session.close().await;
                        }
                        Err(e) => {
                            warn!("Failed to check runner status: {}", e);
                        }
                    }
                }
                Err(e) => {
                    // Connection failed - VM might be shutting down
                    debug!("SSH connection failed (VM may be stopping): {}", e);
                }
            }

            tokio::time::sleep(poll_interval).await;
        }

        Err(Error::timeout(format!(
            "Runner on VM {} did not complete within {:?}",
            vmid, timeout
        )))
    }

    /// Destroy a VM.
    pub async fn destroy_vm(&self, vmid: u32) -> Result<()> {
        info!("Destroying VM {}", vmid);

        // Update state
        if let Some(vm) = self.active_vms.write().await.get_mut(&vmid) {
            vm.state = VmState::Destroying;
        }

        // Try to stop the VM first (graceful shutdown)
        match self.proxmox.stop_vm(vmid).await {
            Ok(task) => {
                // Wait for stop with short timeout
                let _ = self.proxmox.wait_for_task_with_timeout(&task, Duration::from_secs(30)).await;
            }
            Err(e) => {
                debug!("Stop VM {} failed (may already be stopped): {}", vmid, e);
            }
        }

        // Wait a moment for clean shutdown
        tokio::time::sleep(Duration::from_secs(2)).await;

        // Delete the VM
        match self.proxmox.delete_vm(vmid).await {
            Ok(task) => {
                self.proxmox.wait_for_task(&task).await?;
            }
            Err(e) => {
                error!("Failed to delete VM {}: {}", vmid, e);
                // Remove from tracking even on failure
            }
        }

        // Remove from tracking
        self.active_vms.write().await.remove(&vmid);

        info!("VM {} destroyed", vmid);
        Ok(())
    }

    /// Force destroy a VM by ID string.
    pub async fn force_destroy(&self, vm_id: &str) -> Result<()> {
        let vmid: u32 = vm_id.parse().map_err(|_| {
            Error::vm(format!("Invalid VM ID: {}", vm_id))
        })?;

        self.destroy_vm(vmid).await
    }

    /// Cleanup stale VMs that have exceeded the maximum age.
    pub async fn cleanup_stale_vms(&self, max_age: Duration) {
        let stale_vms: Vec<u32> = {
            let vms = self.active_vms.read().await;
            vms.iter()
                .filter(|(_, instance)| instance.created_at.elapsed() > max_age)
                .map(|(vmid, _)| *vmid)
                .collect()
        };

        for vmid in stale_vms {
            warn!("Cleaning up stale VM: {}", vmid);
            if let Err(e) = self.destroy_vm(vmid).await {
                error!("Failed to cleanup stale VM {}: {}", vmid, e);
            }
        }
    }

    /// Destroy all active VMs (used during shutdown).
    pub async fn destroy_all_vms(&self) {
        let vmids: Vec<u32> = {
            self.active_vms.read().await.keys().copied().collect()
        };

        if vmids.is_empty() {
            info!("No active VMs to clean up");
            return;
        }

        info!("Destroying {} active VM(s)...", vmids.len());
        for vmid in vmids {
            info!("Destroying VM {}", vmid);
            if let Err(e) = self.destroy_vm(vmid).await {
                error!("Failed to destroy VM {}: {}", vmid, e);
            }
        }
    }

    /// Detect OS type by running a command via SSH.
    async fn detect_os_type(&self, session: &mut SshSession) -> Result<bool> {
        // Try Windows-specific command first
        let output = session.execute("ver 2>nul || echo NOTWINDOWS").await?;
        let is_windows = !output.stdout.contains("NOTWINDOWS") &&
                         (output.stdout.contains("Windows") || output.stdout.contains("Microsoft"));

        if is_windows {
            return Ok(true);
        }

        // Check for Linux
        let output = session.execute("uname -s 2>/dev/null || echo UNKNOWN").await?;
        if output.stdout.contains("Linux") || output.stdout.contains("Darwin") {
            return Ok(false);
        }

        // Default to Linux
        Ok(false)
    }

    /// Run the complete runner lifecycle for a create command.
    ///
    /// This method:
    /// 1. Creates a VM from template
    /// 2. Starts the VM and waits for IP
    /// 3. Waits for SSH to be available
    /// 4. Configures the GitHub runner
    /// 5. Waits for the runner to complete
    /// 6. Destroys the VM
    pub async fn run_complete_lifecycle(
        &self,
        vm_name: &str,
        registration_token: &str,
        labels: &[String],
        runner_scope_url: &str,
    ) -> Result<(u32, String)> {
        // 1. Create VM
        let vm = self.create_vm(vm_name).await?;
        let vmid = vm.vmid;

        // Ensure cleanup on failure
        let result = self.run_lifecycle_inner(
            vmid,
            vm_name,
            registration_token,
            labels,
            runner_scope_url,
        ).await;

        // Always cleanup on completion or failure
        if let Err(ref e) = result {
            error!("Lifecycle failed, cleaning up VM {}: {}", vmid, e);
        }

        // Destroy VM (ignore errors during cleanup)
        let _ = self.destroy_vm(vmid).await;

        result
    }

    /// Inner lifecycle logic (separated for cleanup handling).
    async fn run_lifecycle_inner(
        &self,
        vmid: u32,
        vm_name: &str,
        registration_token: &str,
        labels: &[String],
        runner_scope_url: &str,
    ) -> Result<(u32, String)> {
        // 2. Start VM and wait for IP
        let ip = self.start_and_wait(vmid).await?;

        // 3. Wait for SSH
        self.wait_for_ssh(&ip).await?;

        // 4. Configure runner
        self.configure_runner(
            vmid,
            &ip,
            registration_token,
            labels,
            runner_scope_url,
            vm_name,
        ).await?;

        // 5. Wait for runner completion (1 hour timeout)
        let runner_timeout = Duration::from_secs(3600);
        self.wait_for_runner_completion(vmid, &ip, runner_timeout).await?;

        Ok((vmid, ip))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_vm_state_as_str() {
        assert_eq!(VmState::Creating.as_str(), "creating");
        assert_eq!(VmState::Running.as_str(), "running");
        assert_eq!(VmState::RunnerActive.as_str(), "runner_active");
        assert_eq!(VmState::Destroying.as_str(), "destroying");
    }
}
