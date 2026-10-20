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
use kuiper_agent_lib::forgejo_runner;
use kuiper_agent_proto::ForgejoRunner;
use kuiper_proxmox_api::ProxmoxVEAPI;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Notify, RwLock};
use tracing::{debug, error, info, warn};

/// Parameters for configuring a GitHub Actions runner on a VM.
pub struct RunnerParams<'a> {
    pub registration_token: &'a str,
    pub labels: &'a [String],
    pub runner_scope_url: &'a str,
    pub runner_name: &'a str,
    pub jit_config: &'a str,
    /// Set for Forgejo jobs: run `forgejo-runner one-job` instead of the GitHub runner
    pub forgejo: Option<&'a ForgejoRunner>,
}

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
    /// Notifier fired whenever the active VM set changes (insert/remove).
    /// Subscribers can use this to push immediate AgentStatus updates so the
    /// coordinator's view doesn't lag the agent's true capacity.
    state_changed: Arc<Notify>,
}

impl VmManager {
    /// Create a new VM manager.
    pub fn new(proxmox: Arc<ProxmoxVEAPI>, vm_config: VmConfig, ssh_config: SshConfig) -> Self {
        Self {
            proxmox,
            vm_config,
            ssh_config,
            active_vms: Arc::new(RwLock::new(HashMap::new())),
            state_changed: Arc::new(Notify::new()),
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

    /// Create a new VM by cloning a template.
    ///
    /// Returns the VM ID of the new clone.
    pub async fn create_vm(&self, name: &str, template_vmid: u32) -> Result<VmInstance> {
        if !self.has_capacity().await {
            return Err(Error::vm("No available VM slots"));
        }

        // Get next available VM ID
        let vmid = self.proxmox.get_next_vmid().await?;
        info!(
            "Creating VM {} with ID {} from template {}",
            name, vmid, template_vmid
        );

        // Track the VM
        let instance = VmInstance {
            vmid,
            name: name.to_string(),
            ip_address: None,
            state: VmState::Creating,
            created_at: Instant::now(),
        };
        self.active_vms.write().await.insert(vmid, instance.clone());
        self.state_changed.notify_one();

        // Clone the template
        let storage = if self.vm_config.storage.is_empty() {
            None
        } else {
            Some(self.vm_config.storage.as_str())
        };

        let task = self
            .proxmox
            .clone_vm(
                template_vmid,
                vmid,
                name,
                self.vm_config.linked_clone,
                storage,
            )
            .await
            .inspect_err(|_e| {
                // Remove from tracking on failure
                let vms = self.active_vms.clone();
                let notify = self.state_changed.clone();
                tokio::spawn(async move {
                    vms.write().await.remove(&vmid);
                    notify.notify_one();
                });
            })?;

        // Wait for clone to complete
        let clone_timeout = Duration::from_secs(self.vm_config.clone_timeout_secs);
        self.proxmox
            .wait_for_task_with_timeout(&task, clone_timeout)
            .await
            .inspect_err(|_e| {
                let vms = self.active_vms.clone();
                let notify = self.state_changed.clone();
                tokio::spawn(async move {
                    vms.write().await.remove(&vmid);
                    notify.notify_one();
                });
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
        info!(
            "Waiting for VM {} to get IP address (timeout: {:?})",
            vmid, ip_timeout
        );
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
    ///
    /// Returns `(is_windows, shell_is_powershell)`:
    /// - `is_windows`: true if the VM is running Windows
    /// - `shell_is_powershell`: true if Windows SSH shell is PowerShell (vs cmd.exe)
    pub async fn configure_runner(
        &self,
        vmid: u32,
        ip: &str,
        params: &RunnerParams<'_>,
    ) -> Result<(bool, bool)> {
        let RunnerParams {
            registration_token,
            labels,
            runner_scope_url,
            runner_name,
            jit_config,
            forgejo,
        } = params;
        // Update state
        if let Some(vm) = self.active_vms.write().await.get_mut(&vmid) {
            vm.state = VmState::Configuring;
        }

        info!("Configuring runner on VM {} at {}", vmid, ip);

        // Connect via SSH
        let ssh_client = SshClient::new(self.ssh_config.clone()).await?;
        let mut session = ssh_client.connect(ip).await?;

        // Detect OS type and shell type
        let (is_windows, shell_is_powershell) = self.detect_os_type(&mut session).await?;
        info!(
            "VM {} detected as {} (shell: {})",
            vmid,
            if is_windows { "Windows" } else { "Linux" },
            if is_windows {
                if shell_is_powershell {
                    "PowerShell"
                } else {
                    "cmd.exe"
                }
            } else {
                "bash/sh"
            }
        );

        // Force time sync on Windows — cloned VMs often have stale clocks
        // which causes JIT token validation to fail ("token is not valid until ...")
        if is_windows {
            info!("Syncing Windows clock on VM {}", vmid);
            let sync_cmd = if shell_is_powershell {
                "net start w32time; w32tm /resync /force"
            } else {
                "powershell -Command \"net start w32time; w32tm /resync /force\""
            };
            match session.execute(sync_cmd).await {
                Ok(output) if output.exit_code == 0 => {
                    debug!("Time sync output (VM {}): {}", vmid, output.stdout.trim());
                }
                Ok(output) => {
                    warn!(
                        "Time sync failed on VM {} (exit {}): {}",
                        vmid,
                        output.exit_code,
                        output.stderr.trim()
                    );
                }
                Err(e) => {
                    warn!("Time sync command failed on VM {}: {}", vmid, e);
                }
            }
        }

        if let Some(forgejo) = forgejo {
            self.run_forgejo_job(vmid, &mut session, forgejo, labels, is_windows)
                .await?;
            session.close().await?;
            return Ok((is_windows, shell_is_powershell));
        }

        // Build runner commands
        let runner_dir = if is_windows {
            r"C:\actions-runner"
        } else {
            "~/actions-runner"
        };

        let builder = if is_windows {
            if shell_is_powershell {
                RunnerConfigBuilder::windows_powershell(runner_dir)
            } else {
                RunnerConfigBuilder::windows(runner_dir)
            }
        } else {
            RunnerConfigBuilder::linux(runner_dir)
        };

        // Check if runner is installed
        let check_cmd = builder.check_installed_command();
        let check_output = session.execute(&check_cmd).await?;
        if check_output.stdout.trim() != "installed" {
            // Determine version to install
            let version = if self.vm_config.runner_version.is_empty()
                || self.vm_config.runner_version == "latest"
            {
                match crate::ssh::github_runner::fetch_latest_version().await {
                    Some(v) => v,
                    None => {
                        info!(
                            "Could not fetch latest runner version, using fallback v{}",
                            crate::ssh::github_runner::FALLBACK_VERSION
                        );
                        crate::ssh::github_runner::FALLBACK_VERSION.to_string()
                    }
                }
            } else {
                self.vm_config.runner_version.clone()
            };

            // Detect architecture - for Windows, default to x64; for Linux, detect via uname
            let arch = if is_windows {
                crate::ssh::github_runner::Arch::X64
            } else {
                let uname_output = session.execute("uname -m").await?;
                crate::ssh::github_runner::Arch::from_uname(&uname_output.stdout)
                    .unwrap_or(crate::ssh::github_runner::Arch::X64)
            };

            info!(
                "GitHub Actions runner not installed on VM {}, installing v{} ({})...",
                vmid,
                version,
                arch.as_str()
            );
            let install_cmd = builder.install_command(&version, arch);
            let install_output = session.execute(&install_cmd).await?;
            if install_output.exit_code != 0 {
                error!(
                    local_only = tracing::field::Empty,
                    "Runner installation failed: stdout={}, stderr={}",
                    install_output.stdout,
                    install_output.stderr
                );
                return Err(Error::runner(format!(
                    "Failed to install runner (exit code {})",
                    install_output.exit_code
                )));
            }
            info!(
                local_only = tracing::field::Empty,
                "Runner installed successfully: {}",
                install_output.stdout.trim()
            );
        } else {
            debug!("GitHub Actions runner already installed on VM {}", vmid);
        }

        // Choose between JIT path (skip config, run with --jitconfig) and legacy path (config + run)
        let run_cmd = if !jit_config.is_empty() {
            info!("Using JIT config on VM {} (skipping config.sh)", vmid);

            // Write JIT config to a file on the VM to avoid shell escaping/length issues
            let write_cmd = builder.write_jit_config_command(jit_config);
            info!(
                "Writing JIT config file on VM {} ({} bytes)",
                vmid,
                jit_config.len()
            );
            let write_output = session
                .execute_secret(&write_cmd, "write JIT config")
                .await?;
            if write_output.exit_code != 0 {
                error!(
                    local_only = tracing::field::Empty,
                    "JIT config write failed on VM {}: {}",
                    vmid,
                    write_output.stderr.trim()
                );
                return Err(Error::runner(format!(
                    "Failed to write JIT config to VM (exit code {})",
                    write_output.exit_code
                )));
            }
            if !write_output.stdout.is_empty() {
                info!(
                    local_only = tracing::field::Empty,
                    "JIT config write stdout (VM {}): {}",
                    vmid,
                    write_output.stdout.trim()
                );
            }
            if !write_output.stderr.is_empty() {
                warn!(
                    local_only = tracing::field::Empty,
                    "JIT config write stderr (VM {}): {}",
                    vmid,
                    write_output.stderr.trim()
                );
            }

            // Update state — no config step needed for JIT
            if let Some(vm) = self.active_vms.write().await.get_mut(&vmid) {
                vm.state = VmState::RunnerActive;
            }

            builder.run_command_jit_from_file()
        } else {
            // Legacy path: config.sh + run.sh
            let config_cmd =
                builder.config_command(runner_scope_url, registration_token, labels, runner_name);

            info!("Running configuration command on VM {}", vmid);
            let output = session
                .execute_secret(&config_cmd, "config.sh (registration token)")
                .await?;
            if output.exit_code != 0 {
                error!(
                    local_only = tracing::field::Empty,
                    "Runner configuration failed (exit {}): stdout={}, stderr={}",
                    output.exit_code,
                    output.stdout,
                    output.stderr
                );
                return Err(Error::runner(format!(
                    "Configuration failed with exit code {}",
                    output.exit_code
                )));
            }
            if !output.stdout.is_empty() {
                info!(
                    local_only = tracing::field::Empty,
                    "Config output: {}",
                    output.stdout.trim()
                );
            }

            // Update state
            if let Some(vm) = self.active_vms.write().await.get_mut(&vmid) {
                vm.state = VmState::RunnerActive;
            }

            builder.run_command_direct()
        };

        // Run the runner and wait for it to complete
        info!(
            "Running runner on VM {} (will block until job completes)",
            vmid
        );
        let run_start = Instant::now();
        let output = self.execute_runner(&mut session, &run_cmd, vmid).await?;
        let run_elapsed = run_start.elapsed();

        info!(
            "Runner completed on VM {} with exit code {} in {:.1}s",
            vmid,
            output.exit_code,
            run_elapsed.as_secs_f64()
        );
        if !output.stdout.is_empty() {
            info!(
                local_only = tracing::field::Empty,
                "Runner stdout (VM {}): {}",
                vmid,
                output.stdout.trim()
            );
        }
        if !output.stderr.is_empty() {
            info!(
                local_only = tracing::field::Empty,
                "Runner stderr (VM {}): {}",
                vmid,
                output.stderr.trim()
            );
        }

        // If the runner exited suspiciously fast, grab as much diagnostic info as possible
        // before the VM is destroyed
        if run_elapsed < Duration::from_secs(30) {
            if !jit_config.is_empty() {
                warn!(
                    "JIT runner on VM {} exited in {:.1}s — runner was pre-assigned a job, \
                     this likely means the runner binary failed to start",
                    vmid,
                    run_elapsed.as_secs_f64()
                );
            } else {
                warn!(
                    "Runner on VM {} exited in {:.1}s — may not have picked up a job",
                    vmid,
                    run_elapsed.as_secs_f64()
                );
            }

            // Always log stdout/stderr at WARN level for fast exits (even if empty)
            warn!(
                local_only = tracing::field::Empty,
                "Early exit diagnostics (VM {}): exit_code={}, stdout_len={}, stderr_len={}, \
                 stdout={:?}, stderr={:?}",
                vmid,
                output.exit_code,
                output.stdout.len(),
                output.stderr.len(),
                output.stdout.chars().take(2000).collect::<String>(),
                output.stderr.chars().take(2000).collect::<String>(),
            );

            // Try to grab runner diagnostic logs
            let diag_cmd = builder.diag_log_command(100);
            match session.execute(&diag_cmd).await {
                Ok(diag) => {
                    if diag.stdout.trim().is_empty() {
                        warn!(
                            "Runner _diag (VM {}): no diagnostic logs found \
                             (runner may not have started at all)",
                            vmid
                        );
                    } else {
                        warn!(
                            local_only = tracing::field::Empty,
                            "Runner _diag (VM {}): {}",
                            vmid,
                            diag.stdout.trim()
                        );
                    }
                }
                Err(e) => {
                    warn!(
                        "Runner _diag (VM {}): failed to fetch diagnostic logs: {}",
                        vmid, e
                    );
                }
            }

            // On Windows, also check if the runner binary exists and .NET is available
            if is_windows {
                let check_cmd = builder.check_installed_command();
                if let Ok(check) = session.execute(&check_cmd).await {
                    warn!(
                        "Runner install check (VM {}): {}",
                        vmid,
                        if check.stdout.trim() == "installed" {
                            "runner binary present"
                        } else {
                            "runner binary NOT FOUND"
                        }
                    );
                }
            }
        }

        if output.exit_code != 0 {
            return Err(Error::runner(format!(
                "Runner exited with code {}",
                output.exit_code
            )));
        }

        session.close().await?;
        Ok((is_windows, shell_is_powershell))
    }

    /// Install forgejo-runner if needed, then run one Forgejo job attempt and wait for it.
    ///
    /// Linux VMs use `~/forgejo-runner` and bash; Windows VMs use `C:\forgejo-runner` and
    /// PowerShell scripts sent with `-EncodedCommand`, which work from cmd.exe and
    /// PowerShell SSH shells alike.
    async fn run_forgejo_job(
        &self,
        vmid: u32,
        session: &mut SshSession,
        forgejo: &ForgejoRunner,
        labels: &[String],
        is_windows: bool,
    ) -> Result<()> {
        let win_dir = forgejo_runner::WINDOWS_RUNNER_DIR;
        let check_cmd = if is_windows {
            forgejo_runner::windows_check_installed_command(win_dir)
        } else {
            forgejo_runner::check_installed_command()
        };
        let check = session.execute(&check_cmd).await?;
        if check.stdout.trim() != "installed" {
            let version =
                forgejo_runner::resolve_version(&self.vm_config.forgejo_runner_version).to_string();
            let (os, arch) = if is_windows {
                // like the GitHub runner path: x64 Windows only for now
                (
                    forgejo_runner::Os::Windows,
                    crate::ssh::github_runner::Arch::X64,
                )
            } else {
                let uname = session.execute("uname -m").await?;
                let arch = crate::ssh::github_runner::Arch::from_uname(&uname.stdout).ok_or_else(
                    || {
                        Error::runner(format!(
                            "forgejo-runner has no release for architecture '{}'",
                            uname.stdout.trim()
                        ))
                    },
                )?;
                (forgejo_runner::Os::Linux, arch)
            };
            let url = forgejo_runner::download_url(
                &self.vm_config.forgejo_runner_url,
                &version,
                os,
                arch,
            );
            info!(
                "forgejo-runner not installed on VM {}, installing v{} ({}) from {}...",
                vmid,
                version,
                arch.as_str(),
                url
            );
            let install_cmd = if is_windows {
                forgejo_runner::windows_install_command(&url, win_dir)
            } else {
                forgejo_runner::install_command(&url, &version, os, arch)
            };
            let install = session.execute(&install_cmd).await?;
            if install.exit_code != 0 {
                error!(
                    local_only = tracing::field::Empty,
                    "forgejo-runner installation failed on VM {}: stdout={}, stderr={}",
                    vmid,
                    install.stdout,
                    install.stderr
                );
                return Err(Error::runner(format!(
                    "Failed to install forgejo-runner v{version} (exit code {})",
                    install.exit_code
                )));
            }
            info!(
                local_only = tracing::field::Empty,
                "forgejo-runner installed on VM {}: {}",
                vmid,
                install.stdout.trim()
            );
        } else {
            debug!("forgejo-runner already installed on VM {}", vmid);
        }

        let job = forgejo_runner::OneJob {
            instance_url: &forgejo.instance_url,
            uuid: &forgejo.uuid,
            token: &forgejo.token,
            handle: &forgejo.handle,
            labels,
        };
        let prepare = if is_windows {
            forgejo_runner::windows_prepare_command(&job, win_dir)
        } else {
            forgejo_runner::prepare_command(&job)
        }
        .map_err(Error::runner)?;
        let prepared = session
            .execute_secret(&prepare, "write forgejo-runner job files (runner token)")
            .await?;
        if prepared.exit_code != 0 {
            error!(
                local_only = tracing::field::Empty,
                "Writing forgejo-runner job files failed on VM {}: {}",
                vmid,
                prepared.stderr.trim()
            );
            return Err(Error::runner(format!(
                "Failed to write forgejo-runner job files (exit code {})",
                prepared.exit_code
            )));
        }

        if let Some(vm) = self.active_vms.write().await.get_mut(&vmid) {
            vm.state = VmState::RunnerActive;
        }
        info!(
            "Running forgejo-runner one-job on VM {} for handle {} (will block until the job completes)",
            vmid, forgejo.handle
        );
        let started = Instant::now();
        let run_cmd = if is_windows {
            forgejo_runner::windows_run_command(win_dir)
        } else {
            forgejo_runner::run_command()
        };
        let output = self.execute_runner(session, &run_cmd, vmid).await?;
        let elapsed = started.elapsed();
        info!(
            "forgejo-runner on VM {} exited with code {} in {:.1}s",
            vmid,
            output.exit_code,
            elapsed.as_secs_f64()
        );
        // forgejo-runner logs to stderr; keep the tail of both for diagnosis
        let tail = |s: &str| {
            let skip = s.chars().count().saturating_sub(4000);
            s.chars().skip(skip).collect::<String>()
        };
        if !output.stdout.is_empty() {
            info!(
                local_only = tracing::field::Empty,
                "forgejo-runner stdout (VM {}): {}",
                vmid,
                tail(output.stdout.trim())
            );
        }
        if !output.stderr.is_empty() {
            info!(
                local_only = tracing::field::Empty,
                "forgejo-runner stderr (VM {}): {}",
                vmid,
                tail(output.stderr.trim())
            );
        }
        if output.exit_code != 0 {
            warn!(
                "forgejo-runner on VM {} failed after {:.1}s (exit {}) for handle {}",
                vmid,
                elapsed.as_secs_f64(),
                output.exit_code,
                forgejo.handle
            );
            return Err(Error::runner(format!(
                "forgejo-runner exited with code {}",
                output.exit_code
            )));
        }
        Ok(())
    }

    /// Destroy one of this agent's VMs.
    ///
    /// `name` is the VM's name when it was created. The destroy only goes ahead if this agent
    /// still tracks `vmid` under that name and nothing else is already destroying it, and
    /// Proxmox's name for the VMID matches too. Once a VM has been destroyed (say by an idle
    /// teardown) Proxmox can hand its VMID to a new clone, so a late destroy by bare VMID could
    /// otherwise delete someone else's VM.
    pub async fn destroy_vm(&self, vmid: u32, name: &str) -> Result<()> {
        if !self.claim_for_destroy(vmid, name).await {
            return Ok(());
        }
        info!("Destroying VM {} ({})", vmid, name);
        let started = Instant::now();

        // make sure Proxmox still has our VM under this VMID before stopping or deleting it
        match self.proxmox.get_vm_status(vmid).await {
            Ok(status) => {
                if let Some(actual) = status.name.as_deref()
                    && actual != name
                {
                    error!(
                        "Not destroying VMID {}: Proxmox names it '{}' but this agent created '{}' \
                         there; dropping it from tracking",
                        vmid, actual, name
                    );
                    self.untrack(vmid, name).await;
                    return Err(Error::vm(format!(
                        "VMID {vmid} is now '{actual}', not '{name}'; refusing to destroy it"
                    )));
                }
            }
            Err(e) => {
                // the VM may already be gone, or the API is having a moment; we still track it
                // under this name, so carry on and let stop/delete report what they find
                warn!(
                    "Couldn't read VM {} ({}) status before destroying it: {}",
                    vmid, name, e
                );
            }
        }

        // Try to stop the VM first (graceful shutdown)
        match self.proxmox.stop_vm(vmid).await {
            Ok(task) => {
                // Wait for stop with short timeout
                let _ = self
                    .proxmox
                    .wait_for_task_with_timeout(&task, Duration::from_secs(30))
                    .await;
            }
            Err(e) => {
                debug!("Stop VM {} failed (may already be stopped): {}", vmid, e);
            }
        }

        // Wait a moment for clean shutdown
        tokio::time::sleep(Duration::from_secs(2)).await;

        // Delete the VM
        let result = match self.proxmox.delete_vm(vmid).await {
            Ok(task) => self.proxmox.wait_for_task(&task).await.map_err(|e| {
                error!("Delete task for VM {} ({}) failed: {}", vmid, name, e);
                Error::from(e)
            }),
            Err(e) => {
                error!("Failed to delete VM {} ({}): {}", vmid, name, e);
                // Remove from tracking even on failure
                Ok(())
            }
        };

        self.untrack(vmid, name).await;
        info!(
            "VM {} ({}) destroyed in {:.1}s",
            vmid,
            name,
            started.elapsed().as_secs_f64()
        );
        result
    }

    /// Mark `vmid` as being destroyed if this agent still tracks it under `name` and no other
    /// destroy has it. Returns false (and logs why) when the destroy should be skipped.
    async fn claim_for_destroy(&self, vmid: u32, name: &str) -> bool {
        let mut vms = self.active_vms.write().await;
        match vms.get_mut(&vmid) {
            Some(vm) if vm.name != name => {
                info!(
                    "Skipping destroy of VM {} ({}): that VMID now belongs to {}",
                    vmid, name, vm.name
                );
                false
            }
            Some(vm) if vm.state == VmState::Destroying => {
                debug!(
                    "Skipping destroy of VM {} ({}): already being destroyed",
                    vmid, name
                );
                false
            }
            Some(vm) => {
                vm.state = VmState::Destroying;
                true
            }
            None => {
                debug!(
                    "Skipping destroy of VM {} ({}): no longer tracked (already destroyed)",
                    vmid, name
                );
                false
            }
        }
    }

    /// Stop tracking `vmid`, but only if it's still the VM named `name`.
    async fn untrack(&self, vmid: u32, name: &str) {
        let mut vms = self.active_vms.write().await;
        if vms.get(&vmid).is_some_and(|vm| vm.name == name) {
            vms.remove(&vmid);
            drop(vms);
            self.state_changed.notify_one();
        }
    }

    /// Run the runner's blocking command, giving up if the VM is torn down meanwhile (an idle
    /// teardown or DestroyRunner), so the lifecycle always ends and reports Completed/Failed.
    async fn execute_runner(
        &self,
        session: &mut SshSession,
        command: &str,
        vmid: u32,
    ) -> Result<crate::ssh::CommandOutput> {
        let Some(name) = self
            .active_vms
            .read()
            .await
            .get(&vmid)
            .map(|vm| vm.name.clone())
        else {
            return Err(Error::vm(format!(
                "VM {vmid} was destroyed before its runner started"
            )));
        };
        tokio::select! {
            output = session.execute(command) => output,
            () = self.torn_down(vmid, &name) => {
                warn!("VM {} ({}) was destroyed while its runner was running", vmid, name);
                Err(Error::vm(format!(
                    "VM {vmid} ({name}) was destroyed while its runner was running"
                )))
            }
        }
    }

    /// Resolves once `vmid` is no longer this agent's running VM `name` (destroyed, or being
    /// destroyed). Checked every 5s.
    async fn torn_down(&self, vmid: u32, name: &str) {
        loop {
            tokio::time::sleep(Duration::from_secs(5)).await;
            let running = self
                .active_vms
                .read()
                .await
                .get(&vmid)
                .is_some_and(|vm| vm.name == name && vm.state != VmState::Destroying);
            if !running {
                return;
            }
        }
    }

    /// Force destroy a VM by ID string.
    ///
    /// `vm_id` is the Proxmox VMID or the VM's name (what the coordinator calls the runner).
    pub async fn force_destroy(&self, vm_id: &str) -> Result<()> {
        let found = {
            let vms = self.active_vms.read().await;
            match vm_id.parse::<u32>() {
                Ok(vmid) => vms.get(&vmid).map(|vm| (vmid, vm.name.clone())),
                Err(_) => vms
                    .iter()
                    .find(|(_, vm)| vm.name == vm_id)
                    .map(|(vmid, vm)| (*vmid, vm.name.clone())),
            }
        };
        let (vmid, name) = found
            .ok_or_else(|| Error::vm(format!("No VM with ID or name '{vm_id}' on this agent")))?;

        self.destroy_vm(vmid, &name).await
    }

    /// Cleanup stale VMs that have exceeded the maximum age.
    pub async fn cleanup_stale_vms(&self, max_age: Duration) {
        let stale_vms: Vec<(u32, String)> = {
            let vms = self.active_vms.read().await;
            vms.iter()
                .filter(|(_, instance)| instance.created_at.elapsed() > max_age)
                .map(|(vmid, instance)| (*vmid, instance.name.clone()))
                .collect()
        };

        for (vmid, name) in stale_vms {
            warn!("Cleaning up stale VM: {} ({})", vmid, name);
            if let Err(e) = self.destroy_vm(vmid, &name).await {
                error!("Failed to cleanup stale VM {}: {}", vmid, e);
            }
        }
    }

    /// Destroy all active VMs (used during shutdown).
    pub async fn destroy_all_vms(&self) {
        let vmids: Vec<(u32, String)> = {
            let vms = self.active_vms.read().await;
            vms.iter()
                .map(|(vmid, vm)| (*vmid, vm.name.clone()))
                .collect()
        };

        if vmids.is_empty() {
            info!("No active VMs to clean up");
            return;
        }

        info!("Destroying {} active VM(s)...", vmids.len());
        for (vmid, name) in vmids {
            if let Err(e) = self.destroy_vm(vmid, &name).await {
                error!("Failed to destroy VM {}: {}", vmid, e);
            }
        }
    }

    /// Detect OS type and shell type by running commands via SSH.
    ///
    /// Returns `(is_windows, shell_is_powershell)`:
    /// - `is_windows`: true if the VM is running Windows
    /// - `shell_is_powershell`: true if the SSH default shell is PowerShell (only relevant for Windows)
    async fn detect_os_type(&self, session: &mut SshSession) -> Result<(bool, bool)> {
        // First, try to detect if the shell is PowerShell by checking $PSVersionTable
        // This only exists in PowerShell, not in cmd.exe
        let ps_check = session.execute("echo $PSVersionTable").await?;
        let shell_is_powershell = ps_check.stdout.contains("PSVersion");

        if shell_is_powershell {
            // Shell is PowerShell - we're on Windows and can use PS commands directly
            info!("Detected PowerShell as SSH default shell (Windows)");
            return Ok((true, true));
        }

        // Try to detect Windows via cmd.exe environment variable
        let output = session.execute("echo %OS%").await?;
        if output.stdout.trim() == "Windows_NT" || output.stdout.contains("Windows_NT") {
            info!("Detected Windows OS via %OS% (cmd.exe shell)");
            return Ok((true, false));
        }

        // Try PowerShell-specific detection (for Windows with cmd.exe shell)
        let output = session
            .execute("powershell -Command \"Write-Output $env:OS\"")
            .await?;
        if output.stdout.contains("Windows") {
            info!("Detected Windows OS via PowerShell probe (cmd.exe shell)");
            return Ok((true, false));
        }

        // Check for Linux/Unix
        let output = session.execute("uname -s").await?;
        if output.stdout.contains("Linux") {
            info!("Detected Linux OS");
            return Ok((false, false));
        }
        if output.stdout.contains("Darwin") {
            info!("Detected macOS");
            return Ok((false, false));
        }

        // If uname works but returns something else, it's Unix-like
        if output.exit_code == 0 && !output.stdout.trim().is_empty() {
            info!("Detected Unix-like OS: {}", output.stdout.trim());
            return Ok((false, false));
        }

        // Default to Windows with cmd.exe if nothing else matches (since uname failed)
        info!("Could not detect OS, defaulting to Windows with cmd.exe shell");
        Ok((true, false))
    }

    /// Run the complete runner lifecycle for a create command.
    ///
    /// This method:
    /// 1. Creates a VM from template
    /// 2. Starts the VM and waits for IP
    /// 3. Waits for SSH to be available
    /// 4. Configures and runs the GitHub runner (blocks until job completes)
    /// 5. Destroys the VM
    pub async fn run_complete_lifecycle(
        &self,
        vm_name: &str,
        template_vmid: u32,
        params: &RunnerParams<'_>,
    ) -> Result<(u32, String)> {
        // 1. Create VM
        let vm = self.create_vm(vm_name, template_vmid).await?;
        let vmid = vm.vmid;

        // Ensure cleanup on failure
        let result = self.run_lifecycle_inner(vmid, vm_name, params).await;

        // Always cleanup on completion or failure
        if let Err(ref e) = result {
            error!("Lifecycle failed, cleaning up VM {}: {}", vmid, e);
        }

        // Destroy VM (ignore errors during cleanup). A no-op if an idle teardown already
        // destroyed it: by then its VMID may belong to another runner's clone.
        let _ = self.destroy_vm(vmid, vm_name).await;

        result
    }

    /// Debug lifecycle: on failure, skips VM destruction so you can SSH in.
    /// On success, destroys the VM normally.
    pub async fn run_lifecycle_debug(
        &self,
        vm_name: &str,
        template_vmid: u32,
        params: &RunnerParams<'_>,
    ) -> Result<(u32, String)> {
        let vm = self.create_vm(vm_name, template_vmid).await?;
        let vmid = vm.vmid;

        let result = self.run_lifecycle_inner(vmid, vm_name, params).await;

        match &result {
            Ok(_) => {
                // Success — clean up normally
                let _ = self.destroy_vm(vmid, vm_name).await;
            }
            Err(e) => {
                error!(
                    "Lifecycle failed on VM {}: {} — VM left running for inspection",
                    vmid, e
                );
            }
        }

        result
    }

    /// Inner lifecycle logic (separated for cleanup handling).
    async fn run_lifecycle_inner(
        &self,
        vmid: u32,
        _vm_name: &str,
        params: &RunnerParams<'_>,
    ) -> Result<(u32, String)> {
        // 2. Start VM and wait for IP
        let ip = self.start_and_wait(vmid).await?;

        // 3. Wait for SSH
        self.wait_for_ssh(&ip).await?;

        // 4. Configure and run the runner (blocks until job completes for ephemeral runners)
        let _os_info = self.configure_runner(vmid, &ip, params).await?;

        // Runner has completed - VM will be cleaned up by caller
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

    /// A manager whose Proxmox API is unreachable (connections are refused right away).
    fn test_manager() -> VmManager {
        let proxmox = ProxmoxVEAPI::new(
            "root@pam",
            kuiper_proxmox_api::ProxmoxAuth::password("x"),
            "http://127.0.0.1:1/",
            false,
        )
        .unwrap();
        let vm_config: VmConfig =
            toml::from_str("template_vmid = 9000\nstorage = \"local\"").unwrap();
        let ssh_config: SshConfig = toml::from_str("").unwrap();
        VmManager::new(Arc::new(proxmox), vm_config, ssh_config)
    }

    async fn track(manager: &VmManager, vmid: u32, name: &str) {
        manager.active_vms.write().await.insert(
            vmid,
            VmInstance {
                vmid,
                name: name.to_string(),
                ip_address: None,
                state: VmState::RunnerActive,
                created_at: Instant::now(),
            },
        );
    }

    #[tokio::test]
    async fn test_destroy_after_force_destroy_is_noop() {
        let manager = test_manager();
        track(&manager, 120, "runner-a").await;

        // idle teardown by name frees VMID 120
        manager.force_destroy("runner-a").await.unwrap();
        assert!(manager.list_vms().await.is_empty());

        // Proxmox hands 120 to another runner's clone
        track(&manager, 120, "runner-b").await;

        // runner-a's lifecycle task finishes and cleans up: it must leave runner-b alone
        manager.destroy_vm(120, "runner-a").await.unwrap();
        let vms = manager.list_vms().await;
        assert_eq!(vms.len(), 1);
        assert_eq!(vms[0].name, "runner-b");
        assert_eq!(vms[0].state, VmState::RunnerActive);
    }

    #[tokio::test(start_paused = true)]
    async fn test_torn_down_fires_on_teardown_or_reuse() {
        let manager = test_manager();
        track(&manager, 123, "runner-d").await;
        let still_running =
            tokio::time::timeout(Duration::from_secs(60), manager.torn_down(123, "runner-d")).await;
        assert!(still_running.is_err(), "a running VM isn't torn down");

        manager.claim_for_destroy(123, "runner-d").await;
        tokio::time::timeout(Duration::from_secs(10), manager.torn_down(123, "runner-d"))
            .await
            .expect("a VM being destroyed counts as torn down");

        manager.active_vms.write().await.clear();
        track(&manager, 123, "runner-e").await;
        tokio::time::timeout(Duration::from_secs(10), manager.torn_down(123, "runner-d"))
            .await
            .expect("a reused VMID counts as torn down");
    }

    #[tokio::test]
    async fn test_destroy_untracked_is_noop() {
        let manager = test_manager();
        manager.destroy_vm(121, "gone").await.unwrap();
        assert!(manager.list_vms().await.is_empty());
    }

    #[tokio::test]
    async fn test_destroy_skips_vm_already_being_destroyed() {
        let manager = test_manager();
        track(&manager, 122, "runner-c").await;
        assert!(manager.claim_for_destroy(122, "runner-c").await);
        // a second destroy while the first is in flight does nothing
        assert!(!manager.claim_for_destroy(122, "runner-c").await);
        manager.destroy_vm(122, "runner-c").await.unwrap();
        assert_eq!(manager.list_vms().await[0].state, VmState::Destroying);
    }
}
