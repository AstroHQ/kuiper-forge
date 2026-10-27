//! libvirt VMs for [`VmAgent`](kuiper_agent_lib::vm_agent::VmAgent): a qcow2 overlay of a base image, a
//! transient domain on it, the job's runner over SSH, then the domain destroyed and the overlay deleted.

use crate::config::{Config, IpSource, SshConfig, VmConfig};
use crate::error::{Error, Result};
use crate::libvirt::{DomainSpec, Libvirt};
use crate::ssh;
use async_trait::async_trait;
use kuiper_agent_lib::guest::{Guest, Job, RunnerVersions};
use kuiper_agent_lib::labels::{self, Capability};
use kuiper_agent_lib::vm_agent::{BackendResult, JobContext, Limit, LimitNames, VmBackend};
use kuiper_agent_proto::CreateRunnerCommand;
use std::net::IpAddr;
use std::path::Path;
use std::time::Duration;
use tokio::time::Instant;
use tracing::info;

/// Every VM counts against the one limit, `vm.max_vms`.
const TOTAL: LimitNames = &["total"];

/// Runs runner VMs on the local libvirt. A VM is its domain name, which is the runner name.
pub struct LibvirtBackend {
    libvirt: Libvirt,
    labels: Vec<String>,
    vm_config: VmConfig,
    ssh_config: SshConfig,
    network: String,
    bridge: Option<String>,
    ip_source: IpSource,
}

impl LibvirtBackend {
    pub fn new(config: &Config, domain_template: Option<String>) -> Self {
        Self {
            libvirt: Libvirt::new(
                config.libvirt.uri.clone(),
                config.libvirt.disk_dir.clone(),
                domain_template,
            ),
            labels: config.agent.labels.clone(),
            vm_config: config.vm.clone(),
            ssh_config: config.ssh.clone(),
            network: config.libvirt.network.clone(),
            bridge: config.libvirt.bridge.clone(),
            ip_source: config.libvirt.ip_source,
        }
    }

    pub fn libvirt(&self) -> &Libvirt {
        &self.libvirt
    }

    fn spec<'a>(&'a self, name: &'a str, disk: &'a Path) -> DomainSpec<'a> {
        DomainSpec {
            name,
            disk,
            vcpus: self.vm_config.vcpus,
            memory_mib: self.vm_config.memory_mib,
            network: &self.network,
            bridge: self.bridge.as_deref(),
        }
    }

    async fn start_and_wait(&self, name: &str) -> Result<IpAddr> {
        let disk = self.libvirt.disk_path(name);
        self.libvirt.start_domain(&self.spec(name, &disk)).await?;

        let wait = Duration::from_secs(self.vm_config.ip_timeout_secs);
        let deadline = Instant::now() + wait;
        info!(
            "Waiting for VM {} to get an IP address (timeout: {:?})",
            name, wait
        );
        let ip = loop {
            if let Some(ip) = self.libvirt.domain_ip(name, self.ip_source).await? {
                break ip;
            }
            if Instant::now() >= deadline {
                return Err(Error::Timeout(format!(
                    "VM {name} got no IP address within {wait:?}"
                )));
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        };
        info!("VM {} has IP {}", name, ip);

        let remaining = deadline.saturating_duration_since(Instant::now());
        ssh::client(&self.ssh_config)?
            .wait_for_port(
                ip,
                remaining.max(Duration::from_secs(self.ssh_config.timeout_secs)),
            )
            .await?;
        Ok(ip)
    }

    async fn configure_and_run(
        &self,
        name: &str,
        ip: IpAddr,
        job: &Job<'_>,
        ctx: &JobContext,
    ) -> Result<()> {
        info!("Configuring runner on VM {} at {}", name, ip);
        let mut session = ssh::client(&self.ssh_config)?
            .connect_with_retries(ip, self.ssh_config.retries)
            .await?;
        let mut guest = Guest::detect(&mut session, format!("VM {name}")).await?;
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
}

/// Runner names become domain and file names, so only allow what's safe in both.
fn check_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('.')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if ok {
        Ok(())
    } else {
        Err(Error::Vm(format!(
            "runner name '{name}' isn't usable as a VM name (letters, digits, '-', '_', '.' only)"
        )))
    }
}

#[async_trait]
impl VmBackend for LibvirtBackend {
    type Vm = String;

    fn labels(&self) -> Vec<String> {
        self.labels.clone()
    }

    /// One capability set per image mapping (base labels plus the mapping's labels), so the coordinator can route
    /// jobs for each mapped image here. With no mappings it's just the base labels.
    fn capabilities(&self) -> Vec<Capability> {
        labels::capabilities(
            &self.labels,
            &self.vm_config.image.display().to_string(),
            &self.vm_config.image_mappings,
        )
    }

    /// A fixed-capacity pool command names its capability's image. Otherwise it's the image of the first mapping
    /// whose capability set covers the job's labels, mirroring how the coordinator routes, or `vm.image` if none
    /// does. See [`labels::mapping_for`].
    fn select_image(&self, cmd: &CreateRunnerCommand) -> String {
        let default = self.vm_config.image.display().to_string();
        match labels::mapping_for(
            &self.labels,
            &default,
            &self.vm_config.image_mappings,
            &cmd.label_set_id,
            &cmd.labels,
        ) {
            Some(mapping) => {
                info!(
                    "Selected image {} for labels {:?} (matched mapping labels {:?})",
                    mapping.image.display(),
                    cmd.labels,
                    mapping.labels
                );
                mapping.image.display().to_string()
            }
            None => {
                info!(
                    "No image mapping for labels {:?}, using default {}",
                    cmd.labels, default
                );
                default
            }
        }
    }

    fn limits(&self) -> Vec<Limit> {
        vec![Limit {
            name: TOTAL[0],
            max: self.vm_config.max_vms,
        }]
    }

    async fn image_limits(&self, _image: &str) -> LimitNames {
        TOTAL
    }

    async fn create(&self, name: &str, image: &str) -> BackendResult<String> {
        check_name(name)?;
        self.libvirt.create_overlay(name, Path::new(image)).await?;
        Ok(name.to_string())
    }

    fn vm_id(&self, name: &String) -> String {
        name.clone()
    }

    async fn start(&self, name: &String) -> BackendResult<IpAddr> {
        Ok(self.start_and_wait(name).await?)
    }

    async fn run_job(
        &self,
        name: &String,
        ip: IpAddr,
        job: &Job<'_>,
        ctx: &JobContext,
    ) -> BackendResult<()> {
        Ok(self.configure_and_run(name, ip, job, ctx).await?)
    }

    async fn destroy(&self, name: &String, _runner: &str) -> BackendResult<()> {
        Ok(self.libvirt.destroy(name).await?)
    }

    /// Domains are named after their runner, so one left over from before a restart can still go. Only the
    /// agent's own domains are touched.
    async fn destroy_untracked(&self, vm_id: &str) -> BackendResult<()> {
        check_name(vm_id)?;
        match self.libvirt.owns_domain(vm_id).await? {
            Some(true) => Ok(self.libvirt.destroy(vm_id).await?),
            _ => Err(format!("No VM named '{vm_id}' on this agent").into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_check_name() {
        assert!(check_name("kuiper-runner-abc_1.2").is_ok());
        assert!(check_name("").is_err());
        assert!(check_name("../etc/passwd").is_err());
        assert!(check_name(".hidden").is_err());
        assert!(check_name("a b").is_err());
        assert!(check_name("a'b").is_err());
        assert!(check_name(&"a".repeat(65)).is_err());
    }
}
