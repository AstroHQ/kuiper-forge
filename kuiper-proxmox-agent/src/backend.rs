//! Proxmox VMs for [`VmAgent`](kuiper_agent_lib::vm_agent::VmAgent): clone a template, start it and
//! wait for its IP, run the job's runner over SSH, then stop and delete it.

use crate::config::{SshConfig, VmConfig};
use crate::error::{Error, Result};
use crate::ssh;
use async_trait::async_trait;
use kuiper_agent_lib::guest::{Guest, Job, RunnerVersions};
use kuiper_agent_lib::labels::{self, Capability};
use kuiper_agent_lib::vm_agent::{BackendResult, JobContext, Limit, LimitNames, VmBackend};
use kuiper_agent_proto::CreateRunnerCommand;
use kuiper_proxmox_api::ProxmoxVEAPI;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error, info, warn};

/// Every VM counts against the one limit, `vm.concurrent_vms`.
const TOTAL: LimitNames = &["total"];

/// Runs runner VMs on a Proxmox node. A VM is its VMID.
pub struct ProxmoxBackend {
    proxmox: Arc<ProxmoxVEAPI>,
    labels: Vec<String>,
    vm_config: VmConfig,
    ssh_config: SshConfig,
}

impl ProxmoxBackend {
    pub fn new(
        proxmox: Arc<ProxmoxVEAPI>,
        labels: Vec<String>,
        vm_config: VmConfig,
        ssh_config: SshConfig,
    ) -> Self {
        Self {
            proxmox,
            labels,
            vm_config,
            ssh_config,
        }
    }

    async fn clone_template(&self, name: &str, template_vmid: u32) -> Result<u32> {
        let vmid = self.proxmox.get_next_vmid().await?;
        info!(
            "Creating VM {} with ID {} from template {}",
            name, vmid, template_vmid
        );

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
            .await?;
        let clone_timeout = Duration::from_secs(self.vm_config.clone_timeout_secs);
        self.proxmox
            .wait_for_task_with_timeout(&task, clone_timeout)
            .await?;

        info!("VM {} cloned successfully", vmid);
        Ok(vmid)
    }

    async fn start_and_wait(&self, vmid: u32) -> Result<IpAddr> {
        info!("Starting VM {}", vmid);
        let task = self.proxmox.start_vm(vmid).await?;
        self.proxmox.wait_for_task(&task).await?;

        let ip_timeout = Duration::from_secs(self.vm_config.ip_timeout_secs);
        info!(
            "Waiting for VM {} to get IP address (timeout: {:?})",
            vmid, ip_timeout
        );
        let ip = ssh::parse_ip(&self.proxmox.poll_for_ip(vmid, ip_timeout).await?)?;
        info!("VM {} has IP {}", vmid, ip);

        let ssh_timeout = Duration::from_secs(self.ssh_config.timeout_secs);
        ssh::client(&self.ssh_config)?
            .wait_for_port(ip, ssh_timeout)
            .await?;
        Ok(ip)
    }

    async fn configure_and_run(
        &self,
        vmid: u32,
        ip: IpAddr,
        job: &Job<'_>,
        ctx: &JobContext,
    ) -> Result<()> {
        info!("Configuring runner on VM {} at {}", vmid, ip);
        let mut session = ssh::client(&self.ssh_config)?
            .connect_with_retries(ip, self.ssh_config.retries)
            .await?;
        let mut guest = Guest::detect(&mut session, format!("VM {vmid}")).await?;
        guest.sync_clock().await;

        let versions = RunnerVersions {
            github: &self.vm_config.runner_version,
            forgejo: &self.vm_config.forgejo_runner_version,
            forgejo_url: &self.vm_config.forgejo_runner_url,
        };
        let runner = guest.prepare(job, &versions).await?;
        ctx.runner_started().await;
        guest.run(runner).await?;

        session.close().await?;
        Ok(())
    }

    /// Stop and delete VMID `vmid`, as long as Proxmox still names it `name`. Once a VM is gone
    /// Proxmox can hand its VMID to a new clone, maybe one this agent didn't make.
    async fn stop_and_delete(&self, vmid: u32, name: &str) -> Result<()> {
        match self.proxmox.get_vm_status(vmid).await {
            Ok(status) => {
                if let Some(actual) = status.name.as_deref()
                    && actual != name
                {
                    error!(
                        "Not destroying VMID {}: Proxmox names it '{}' but this agent created '{}' there",
                        vmid, actual, name
                    );
                    return Err(Error::vm(format!(
                        "VMID {vmid} is now '{actual}', not '{name}'; refusing to destroy it"
                    )));
                }
            }
            Err(e) => {
                // the VM may already be gone, or the API is having a moment; carry on and let
                // stop/delete report what they find
                warn!(
                    "Couldn't read VM {} ({}) status before destroying it: {}",
                    vmid, name, e
                );
            }
        }

        // Try to stop the VM first (graceful shutdown)
        match self.proxmox.stop_vm(vmid).await {
            Ok(task) => {
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

        match self.proxmox.delete_vm(vmid).await {
            Ok(task) => self.proxmox.wait_for_task(&task).await.map_err(|e| {
                error!("Delete task for VM {} ({}) failed: {}", vmid, name, e);
                Error::from(e)
            }),
            Err(e) => {
                // reported, but the VM is dropped from tracking all the same
                error!("Failed to delete VM {} ({}): {}", vmid, name, e);
                Ok(())
            }
        }
    }
}

#[async_trait]
impl VmBackend for ProxmoxBackend {
    type Vm = u32;

    fn labels(&self) -> Vec<String> {
        self.labels.clone()
    }

    /// One capability set per template mapping (base labels plus the mapping's labels), so the
    /// coordinator can route jobs for each mapped template here. With no mappings it's just the base labels.
    fn capabilities(&self) -> Vec<Capability> {
        labels::capabilities(
            &self.labels,
            &self.vm_config.template_vmid.to_string(),
            &self.vm_config.template_mappings,
        )
    }

    /// A fixed-capacity pool command names its capability's template. Otherwise it's the template of the
    /// first mapping whose capability set covers the job's labels, mirroring how the coordinator routes,
    /// or the default `template_vmid` if none does. See [`labels::mapping_for`].
    fn select_image(&self, cmd: &CreateRunnerCommand) -> String {
        let template_vmid = match labels::mapping_for(
            &self.labels,
            &self.vm_config.template_vmid.to_string(),
            &self.vm_config.template_mappings,
            &cmd.label_set_id,
            &cmd.labels,
        ) {
            Some(mapping) => {
                info!(
                    "Selected template {} for labels {:?} (matched mapping labels {:?})",
                    mapping.template_vmid, cmd.labels, mapping.labels
                );
                mapping.template_vmid
            }
            None => {
                info!(
                    "No template mapping for labels {:?}, using default {}",
                    cmd.labels, self.vm_config.template_vmid
                );
                self.vm_config.template_vmid
            }
        };
        template_vmid.to_string()
    }

    fn limits(&self) -> Vec<Limit> {
        vec![Limit {
            name: TOTAL[0],
            max: self.vm_config.concurrent_vms,
        }]
    }

    async fn image_limits(&self, _image: &str) -> LimitNames {
        TOTAL
    }

    async fn create(&self, name: &str, image: &str) -> BackendResult<u32> {
        let template_vmid = image
            .parse()
            .map_err(|e| Error::vm(format!("Invalid template VMID '{image}': {e}")))?;
        Ok(self.clone_template(name, template_vmid).await?)
    }

    fn vm_id(&self, vmid: &u32) -> String {
        vmid.to_string()
    }

    async fn start(&self, vmid: &u32) -> BackendResult<IpAddr> {
        Ok(self.start_and_wait(*vmid).await?)
    }

    async fn run_job(
        &self,
        vmid: &u32,
        ip: IpAddr,
        job: &Job<'_>,
        ctx: &JobContext,
    ) -> BackendResult<()> {
        Ok(self.configure_and_run(*vmid, ip, job, ctx).await?)
    }

    async fn destroy(&self, vmid: &u32, name: &str) -> BackendResult<()> {
        Ok(self.stop_and_delete(*vmid, name).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn backend(vm_config: &str) -> ProxmoxBackend {
        let proxmox = ProxmoxVEAPI::new(
            "root@pam",
            kuiper_proxmox_api::ProxmoxAuth::password("x"),
            "http://127.0.0.1:1/",
            false,
        )
        .unwrap();
        ProxmoxBackend::new(
            Arc::new(proxmox),
            vec!["proxmox".to_string()],
            toml::from_str(vm_config).unwrap(),
            toml::from_str("").unwrap(),
        )
    }

    fn create_cmd(labels: &[&str]) -> CreateRunnerCommand {
        CreateRunnerCommand {
            labels: labels.iter().map(|l| l.to_string()).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn test_select_image_uses_mapping_or_default() {
        let backend = backend(
            "template_vmid = 9000\nstorage = \"local\"\n\
             [[template_mappings]]\nlabels = [\"windows\"]\ntemplate_vmid = 9001\n",
        );
        assert_eq!(
            backend.select_image(&create_cmd(&["proxmox", "windows"])),
            "9001"
        );
        assert_eq!(backend.select_image(&create_cmd(&["proxmox"])), "9000");
    }

    #[test]
    fn test_one_total_limit() {
        let backend = backend("template_vmid = 9000\nstorage = \"local\"\nconcurrent_vms = 3");
        assert_eq!(
            backend.limits(),
            vec![Limit {
                name: "total",
                max: 3
            }]
        );
    }

    #[tokio::test]
    async fn test_create_rejects_bad_template() {
        let backend = backend("template_vmid = 9000\nstorage = \"local\"");
        assert!(backend.create("runner", "not-a-vmid").await.is_err());
    }
}
