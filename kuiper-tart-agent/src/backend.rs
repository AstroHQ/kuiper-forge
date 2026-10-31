//! Tart VMs for [`VmAgent`](kuiper_agent_lib::vm_agent::VmAgent): clone an image, `tart run` it and wait for
//! SSH, run the job's runner (in the GUI session on macOS), then stop and delete it.
//!
//! macOS only runs 2 macOS guests at once, so macOS VMs count against a `macos` limit as well as the `total`
//! one. Running tart VMs this agent didn't start (by hand, another tool, a previous agent run) count against
//! the same limits.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use kuiper_agent_lib::guest::Job;
use kuiper_agent_lib::labels::{self, Capability};
use kuiper_agent_lib::runner_cache::RunnerCache;
use kuiper_agent_lib::vm_agent::{BackendResult, JobContext, Limit, LimitNames, Usage, VmBackend};
use kuiper_agent_proto::CreateRunnerCommand;
use serde::Deserialize;
use tokio::process::Command;
use tokio::sync::{Notify, RwLock};
use tokio::time::{Instant, timeout};
use tracing::{debug, info, warn};

use crate::config::TartConfig;
use crate::error::{Error, Result};
use crate::ssh::{self, RunnerProgram, SshConfig};

/// Timeout for tart CLI commands (clone, stop, delete, ip).
/// This prevents the agent from hanging indefinitely if tart gets stuck.
const TART_COMMAND_TIMEOUT: Duration = Duration::from_secs(120);

/// How often to re-count tart VMs this agent doesn't manage.
pub const EXTERNAL_POLL_INTERVAL: Duration = Duration::from_secs(15);

/// How long to wait before pulling images again after a pull failed.
pub const IMAGE_OS_RETRY_INTERVAL: Duration = Duration::from_secs(600);

/// Pulls can be big images over slow links, so this is long.
const TART_PULL_TIMEOUT: Duration = Duration::from_secs(2 * 3600);

/// How long a started VM gets to come up with an IP and SSH.
const READY_TIMEOUT: Duration = Duration::from_secs(120);

const LIMIT_MACOS: &str = "macos";
const LIMIT_TOTAL: &str = "total";

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

    /// The limits a VM with this OS counts against.
    pub fn limit_names(self) -> LimitNames {
        match self {
            GuestOs::MacOS => &[LIMIT_MACOS, LIMIT_TOTAL],
            GuestOs::Linux => &[LIMIT_TOTAL],
        }
    }
}

// some tart 2.x builds emit lowercase keys, same as `host_checks::TartImage`
#[derive(Deserialize)]
struct TartListEntry {
    #[serde(rename = "Name", alias = "name")]
    name: String,
    #[serde(rename = "Running", alias = "running", default)]
    running: bool,
}

#[derive(Deserialize)]
struct TartGetInfo {
    #[serde(rename = "OS", alias = "os")]
    os: String,
}

/// A tart VM: its name, and its OS once the clone told us.
#[derive(Debug, Clone)]
pub struct TartVm {
    name: String,
    os: GuestOs,
}

/// Runs runner VMs with the tart CLI.
pub struct TartBackend {
    config: TartConfig,
    /// `agent.labels` as configured, for matching jobs to image mappings
    agent_labels: Vec<String>,
    /// Lowercased base labels, advertised
    labels: Vec<String>,
    capabilities: Vec<Capability>,
    ssh_config: SshConfig,
    /// Directory for runner log files
    log_dir: PathBuf,
    runner_cache: RunnerCache,
    /// Fired when external usage or an image's OS changes
    changed: Arc<Notify>,
    /// Running tart VMs this agent doesn't manage, from the last `refresh_external`
    external: RwLock<Usage>,
    /// Guest OS per image, once `tart get` knows it. OCI images that aren't pulled yet stay unknown
    image_os: RwLock<HashMap<String, GuestOs>>,
    /// Names of the VMs this agent has created and not yet deleted, so they don't count as external
    ours: RwLock<HashSet<String>>,
}

impl TartBackend {
    pub fn new(
        config: TartConfig,
        agent_labels: Vec<String>,
        ssh_config: SshConfig,
        log_dir: PathBuf,
        runner_cache: RunnerCache,
    ) -> Self {
        // base labels plus one image mapping's labels each, then base_image alone
        let labels: Vec<String> = agent_labels.iter().map(|l| l.to_lowercase()).collect();
        let capabilities =
            labels::capabilities(&labels, &config.base_image, &config.image_mappings);
        Self {
            config,
            agent_labels,
            labels,
            capabilities,
            ssh_config,
            log_dir,
            runner_cache,
            changed: Arc::new(Notify::new()),
            external: RwLock::new(Usage::new()),
            image_os: RwLock::new(HashMap::new()),
            ours: RwLock::new(HashSet::new()),
        }
    }

    /// Guest OS of an image. An image `tart get` doesn't know yet (an OCI image that isn't pulled) counts as macOS
    /// until a clone of it tells us otherwise.
    pub async fn image_os(&self, image: &str) -> GuestOs {
        self.known_image_os(image).await.unwrap_or(GuestOs::MacOS)
    }

    /// Learn the OS of each advertised image, pulling OCI images tart doesn't have yet. Returns true once every
    /// image is known.
    pub async fn resolve_image_os(&self) -> bool {
        let mut all_known = true;
        for capability in &self.capabilities {
            let image = &capability.id;
            if self.known_image_os(image).await.is_some() {
                continue;
            }
            if !crate::host_checks::is_oci_image(image) {
                warn!("Image {} isn't local or OCI, can't find its OS", image);
                all_known = false;
                continue;
            }

            info!("Pulling {} to find its OS", image);
            if let Err(e) = tart_pull(image).await {
                warn!("Failed to pull {}: {}", image, e);
                all_known = false;
                continue;
            }
            match self.known_image_os(image).await {
                Some(os) => {
                    info!("Image {} is {:?}", image, os);

                    // re-send status, the image's limits may have changed
                    self.changed.notify_one();
                }
                None => all_known = false,
            }
        }
        all_known
    }

    async fn known_image_os(&self, image: &str) -> Option<GuestOs> {
        if let Some(os) = self.image_os.read().await.get(image) {
            return Some(*os);
        }
        match tart_os(image).await {
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

    /// The clone pulled the image if it wasn't local, so now tart knows its OS.
    async fn learn_os_from_clone(&self, vm_name: &str, image: &str) -> GuestOs {
        let os = match tart_os(vm_name).await {
            Ok(os) => GuestOs::from_tart(&os),
            Err(e) => {
                warn!("Couldn't get OS of VM {}, assuming macOS: {}", vm_name, e);
                return GuestOs::MacOS;
            }
        };
        info!("Image {} is {:?}", image, os);
        self.image_os.write().await.insert(image.to_string(), os);
        self.changed.notify_one();
        os
    }

    async fn count_external(&self) -> Result<Usage> {
        // ours can start or finish while `tart list` runs, so take our names from both sides of it
        let mut ours = self.ours.read().await.clone();
        let running = tart_running_vms().await?;
        ours.extend(self.ours.read().await.iter().cloned());

        let mut external = Usage::new();
        for name in running.iter().filter(|name| !ours.contains(*name)) {
            let os = match tart_os(name).await {
                Ok(os) => GuestOs::from_tart(&os),
                Err(e) => {
                    warn!("Couldn't get OS of tart VM {}, assuming macOS: {}", name, e);
                    GuestOs::MacOS
                }
            };
            for limit in os.limit_names() {
                *external.entry(limit).or_default() += 1;
            }
        }
        Ok(external)
    }

    async fn clone_vm(&self, name: &str, image: &str) -> Result<TartVm> {
        let known_os = self.known_image_os(image).await;
        self.ours.write().await.insert(name.to_string());
        info!("Creating VM {} from image {}", name, image);
        if let Err(e) = tart_clone(image, name).await {
            self.ours.write().await.remove(name);
            return Err(e);
        }
        let os = match known_os {
            Some(os) => os,
            None => self.learn_os_from_clone(name, image).await,
        };
        Ok(TartVm {
            name: name.to_string(),
            os,
        })
    }

    async fn run(&self, name: &str) -> Result<()> {
        let mut cmd = Command::new("tart");
        cmd.args(["run", name, "--no-graphics"]);

        // Add shared directory if configured
        if let Some(ref cache_dir) = self.config.shared_cache_dir {
            cmd.arg("--dir")
                .arg(format!("cache:{}", cache_dir.display()));
        }

        // Spawn in background (detached)
        cmd.spawn()?;
        info!("VM {} started", name);
        Ok(())
    }

    /// Wait for a started VM to get an IP and accept SSH.
    async fn wait_for_ready(&self, name: &str, wait: Duration) -> Result<Ipv4Addr> {
        let deadline = Instant::now() + wait;
        info!("Waiting for VM {} to be ready (timeout: {:?})", name, wait);

        while Instant::now() < deadline {
            if let Some(ip) = tart_ip(name).await? {
                let remaining = deadline - Instant::now();
                match ssh::wait_for_ssh(ip, remaining).await {
                    Ok(_) => {
                        info!("VM {} ready at {}", name, ip);
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

    /// Set up the job's runner, then start it and wait for it to finish. On macOS the runner runs in the
    /// Terminal.app GUI context for macOS services (code signing, keychain, notarization).
    async fn configure_and_run(
        &self,
        vm: &TartVm,
        ip: Ipv4Addr,
        job: &Job<'_>,
        ctx: &JobContext,
    ) -> Result<()> {
        let program = if let Some(forgejo) = job.forgejo {
            info!(
                "[{}] Preparing forgejo-runner for handle {}...",
                vm.name, forgejo.handle
            );
            let one_job = kuiper_agent_lib::forgejo_runner::OneJob {
                instance_url: &forgejo.instance_url,
                uuid: &forgejo.uuid,
                token: &forgejo.token,
                handle: &forgejo.handle,
                labels: job.labels,
            };
            ssh::prepare_forgejo_runner(
                ip,
                &self.ssh_config,
                &one_job,
                &self.config.forgejo_runner_version,
                &self.config.forgejo_runner_url,
                &self.runner_cache,
                vm.os,
                ctx,
            )
            .await?;
            RunnerProgram::Forgejo
        } else if !job.jit_config.is_empty() {
            // JIT: only install the runner, the JIT config goes to run.sh when it starts
            info!(
                "[{}] JIT mode: ensuring runner is installed (skipping config.sh)",
                vm.name
            );
            ssh::ensure_runner_installed(
                ip,
                &self.ssh_config,
                &self.config.runner_version,
                &self.runner_cache,
                vm.os,
                ctx,
            )
            .await?;
            RunnerProgram::GitHub
        } else {
            info!("[{}] Configuring GitHub runner...", vm.name);
            ssh::configure_runner(
                ip,
                &self.ssh_config,
                job.registration_token,
                job.labels,
                job.runner_scope_url,
                job.runner_name,
                &self.config.runner_version,
                &self.runner_cache,
                vm.os,
                ctx,
            )
            .await?;
            RunnerProgram::GitHub
        };
        ctx.runner_started().await;

        let timestamp = chrono::Local::now().format("%Y-%m-%d_%H%M%S");
        let log_file = self
            .log_dir
            .join(format!("runner-{}-{timestamp}.log", vm.name));
        info!(
            "[{}] Starting runner ({:?}) and waiting for completion",
            vm.name, vm.os
        );
        ssh::start_runner_and_wait(
            ip,
            &self.ssh_config,
            &log_file,
            job.jit_config,
            vm.os,
            program,
        )
        .await?;
        info!(
            "[{}] Runner completed (log: {})",
            vm.name,
            log_file.display()
        );
        Ok(())
    }

    /// Stop and delete a VM by name. Failures are only logged, it may already be gone.
    async fn stop_and_delete(&self, name: &str) {
        let _ = tart_stop(name).await;
        match tart_delete(name).await {
            Ok(()) => info!("VM {} deleted", name),
            Err(e) => warn!("Failed to delete VM {}: {}", name, e),
        }
        self.ours.write().await.remove(name);
    }
}

#[async_trait]
impl VmBackend for TartBackend {
    type Vm = TartVm;

    fn labels(&self) -> Vec<String> {
        self.labels.clone()
    }

    fn capabilities(&self) -> Vec<Capability> {
        self.capabilities.clone()
    }

    /// A fixed-capacity pool command names its capability's image. Otherwise it's the image of the first mapping
    /// whose capability set (agent labels plus that mapping's labels) covers the job's labels, mirroring how the
    /// coordinator routes, or `base_image` if none does. See [`labels::mapping_for`].
    fn select_image(&self, cmd: &CreateRunnerCommand) -> String {
        match labels::mapping_for(
            &self.agent_labels,
            &self.config.base_image,
            &self.config.image_mappings,
            &cmd.label_set_id,
            &cmd.labels,
        ) {
            Some(mapping) => {
                info!(
                    "Selected image '{}' for labels {:?} (matched mapping labels {:?})",
                    mapping.image, cmd.labels, mapping.labels
                );
                mapping.image.clone()
            }
            None => {
                info!(
                    "No image mapping for labels {:?}, using default '{}'",
                    cmd.labels, self.config.base_image
                );
                self.config.base_image.clone()
            }
        }
    }

    fn limits(&self) -> Vec<Limit> {
        vec![
            Limit {
                name: LIMIT_MACOS,
                max: self.config.max_macos_vms,
            },
            Limit {
                name: LIMIT_TOTAL,
                max: self.config.max_total_vms,
            },
        ]
    }

    async fn image_limits(&self, image: &str) -> LimitNames {
        self.image_os(image).await.limit_names()
    }

    /// Re-count running tart VMs this agent doesn't manage. Keeps the last count if tart fails.
    async fn refresh_external(&self) {
        let external = match self.count_external().await {
            Ok(external) => external,
            Err(e) => {
                warn!("Failed to count external tart VMs: {}", e);
                return;
            }
        };
        let old = std::mem::replace(&mut *self.external.write().await, external.clone());
        if old != external {
            info!("External tart VMs changed: {:?} (was {:?})", external, old);
            self.changed.notify_one();
        }
    }

    async fn external_usage(&self) -> Usage {
        self.external.read().await.clone()
    }

    fn changes(&self) -> Option<Arc<Notify>> {
        Some(self.changed.clone())
    }

    async fn create(&self, name: &str, image: &str) -> BackendResult<TartVm> {
        Ok(self.clone_vm(name, image).await?)
    }

    fn vm_id(&self, vm: &TartVm) -> String {
        vm.name.clone()
    }

    async fn start(&self, vm: &TartVm) -> BackendResult<IpAddr> {
        self.run(&vm.name).await?;
        Ok(self.wait_for_ready(&vm.name, READY_TIMEOUT).await?.into())
    }

    async fn run_job(
        &self,
        vm: &TartVm,
        ip: IpAddr,
        job: &Job<'_>,
        ctx: &JobContext,
    ) -> BackendResult<()> {
        let IpAddr::V4(ip) = ip else {
            return Err(format!("tart VM {} has an IPv6 address {ip}", vm.name).into());
        };
        Ok(self.configure_and_run(vm, ip, job, ctx).await?)
    }

    async fn destroy(&self, vm: &TartVm, _name: &str) -> BackendResult<()> {
        self.stop_and_delete(&vm.name).await;
        Ok(())
    }

    /// tart VMs are named after their runner, so one left over from before a restart can still go.
    async fn destroy_untracked(&self, vm_id: &str) -> BackendResult<()> {
        self.stop_and_delete(vm_id).await;
        Ok(())
    }
}

// --- Tart CLI wrappers ---

/// Clone a VM from template.
async fn tart_clone(source: &str, target: &str) -> Result<()> {
    let output = timeout(
        TART_COMMAND_TIMEOUT,
        Command::new("tart")
            .args(["clone", source, target])
            .output(),
    )
    .await
    .map_err(|_| Error::Timeout("tart clone"))??;

    if output.status.success() {
        info!("VM {} cloned successfully", target);
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(Error::CloneFailed(stderr.to_string()))
    }
}

/// Stop a VM.
async fn tart_stop(name: &str) -> Result<()> {
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
async fn tart_delete(name: &str) -> Result<()> {
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

/// Pull an OCI image into tart's cache.
async fn tart_pull(image: &str) -> Result<()> {
    let output = timeout(
        TART_PULL_TIMEOUT,
        Command::new("tart").args(["pull", image]).output(),
    )
    .await
    .map_err(|_| Error::Timeout("tart pull"))??;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(Error::Tart(format!("pull failed: {stderr}")))
    }
}

/// Names of running local VMs (any owner).
async fn tart_running_vms() -> Result<Vec<String>> {
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
async fn tart_os(name: &str) -> Result<String> {
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
async fn tart_ip(name: &str) -> Result<Option<Ipv4Addr>> {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tart_config(max_macos_vms: u32, max_total_vms: u32) -> TartConfig {
        TartConfig {
            base_image: "test".to_string(),
            max_macos_vms,
            max_total_vms,
            shared_cache_dir: None,
            ssh: Default::default(),
            runner_version: "latest".to_string(),
            forgejo_runner_version: "latest".to_string(),
            forgejo_runner_url: kuiper_agent_lib::forgejo_runner::DEFAULT_URL_TEMPLATE.to_string(),
            image_mappings: Vec::new(),
        }
    }

    #[test]
    fn test_limits() {
        let backend = TartBackend::new(
            tart_config(2, 3),
            vec!["MacOS".to_string()],
            SshConfig::default(),
            std::env::temp_dir(),
            RunnerCache::new(std::env::temp_dir(), &Default::default()).unwrap(),
        );
        assert_eq!(
            backend.limits(),
            vec![
                Limit {
                    name: "macos",
                    max: 2
                },
                Limit {
                    name: "total",
                    max: 3
                },
            ]
        );
        // advertised labels are lowercased
        assert_eq!(backend.labels(), ["macos"]);
    }

    #[test]
    fn test_guest_os_from_tart() {
        assert_eq!(GuestOs::from_tart("linux"), GuestOs::Linux);
        assert_eq!(GuestOs::from_tart("darwin"), GuestOs::MacOS);
        assert_eq!(GuestOs::from_tart(""), GuestOs::MacOS);
    }

    #[test]
    fn test_guest_os_limit_names() {
        assert_eq!(GuestOs::MacOS.limit_names(), ["macos", "total"]);
        assert_eq!(GuestOs::Linux.limit_names(), ["total"]);
    }

    #[test]
    fn test_parse_tart_json_lowercase_keys() {
        let entries: Vec<TartListEntry> =
            serde_json::from_str(r#"[{"name": "a", "running": true}]"#).unwrap();
        assert_eq!(entries[0].name, "a");
        assert!(entries[0].running);

        let info: TartGetInfo = serde_json::from_str(r#"{"os": "linux", "cpu": 4}"#).unwrap();
        assert_eq!(info.os, "linux");
        let info: TartGetInfo = serde_json::from_str(r#"{"OS": "darwin", "CPU": 4}"#).unwrap();
        assert_eq!(info.os, "darwin");
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
