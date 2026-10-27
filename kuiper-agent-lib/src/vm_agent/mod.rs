//! A generic coordinator-driven VM agent: an agent supplies a [`VmBackend`] (how to clone, start, run a job on
//! and destroy its kind of VM) and [`VmAgent`] does the rest. It tracks VMs and capacity, acks and runs
//! coordinator commands, reports status and runner events, cleans up stale VMs, and destroys its VMs on
//! shutdown.
//!
//! One runner's lifecycle: reserve a slot (before acking, so capacity can't be overbooked), create, start,
//! run the job, destroy. A DestroyRunner, stale cleanup or shutdown can tear the VM down at any point;
//! the lifecycle notices and reports the runner failed.

mod tracker;

pub use tracker::{Claim, Limit, LimitNames, Removal, ReserveError, Tracker, Usage, VmState};

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use kuiper_agent_proto::{AgentStatus, CreateRunnerCommand, LabelSet, RunnerEventType};
use tokio::sync::Notify;
use tokio::time::Instant;
use tracing::{error, info, warn};

use crate::guest::Job;
use crate::labels::Capability;
use crate::runtime::{self, EventSender};
use crate::{AgentCertStore, AgentConfig, LogCapture};

/// A backend's error, shown to the coordinator as a string.
pub type BackendError = Box<dyn std::error::Error + Send + Sync>;
pub type BackendResult<T> = std::result::Result<T, BackendError>;

/// One kind of VM host: Proxmox, tart, libvirt, ...
#[async_trait]
pub trait VmBackend: Send + Sync + 'static {
    /// The backend's handle on one of its VMs.
    type Vm: Clone + Send + Sync + 'static;

    /// Base labels, sent in `AgentStatus`.
    fn labels(&self) -> Vec<String>;

    /// The label sets this agent can run, one per image (see [`crate::labels::capabilities`]).
    fn capabilities(&self) -> Vec<Capability>;

    /// The image (template, base image) to create a runner VM from.
    fn select_image(&self, cmd: &CreateRunnerCommand) -> String;

    /// Host-wide VM limits.
    fn limits(&self) -> Vec<Limit>;

    /// The limits a VM from `image` counts against.
    async fn image_limits(&self, image: &str) -> LimitNames;

    /// Re-count VMs on the host this agent doesn't manage. Called before taking a slot.
    async fn refresh_external(&self) {}

    /// VMs on the host this agent doesn't manage, per limit, as last counted.
    async fn external_usage(&self) -> Usage {
        Usage::new()
    }

    /// Fired when backend state that goes into `AgentStatus` changes (say, external usage).
    fn changes(&self) -> Option<Arc<Notify>> {
        None
    }

    /// Clone a VM named `name` from `image`, without starting it.
    async fn create(&self, name: &str, image: &str) -> BackendResult<Self::Vm>;

    /// The VM's ID as the coordinator sees it, e.g. a Proxmox VMID.
    fn vm_id(&self, vm: &Self::Vm) -> String;

    /// Start the VM and wait until it's reachable over SSH. Returns its IP.
    async fn start(&self, vm: &Self::Vm) -> BackendResult<IpAddr>;

    /// Set up the job's runner and run it to completion. Call [`JobContext::runner_started`] once the
    /// runner is set up and about to take the job.
    async fn run_job(
        &self,
        vm: &Self::Vm,
        ip: IpAddr,
        job: &Job<'_>,
        ctx: &JobContext,
    ) -> BackendResult<()>;

    /// Stop and delete the VM `name`.
    async fn destroy(&self, vm: &Self::Vm, name: &str) -> BackendResult<()>;

    /// A DestroyRunner named a VM this agent doesn't track (say, left over from before a restart).
    async fn destroy_untracked(&self, vm_id: &str) -> BackendResult<()> {
        Err(format!("No VM with ID or name '{vm_id}' on this agent").into())
    }
}

/// Lets [`VmBackend::run_job`] report progress.
pub struct JobContext {
    name: String,
    vm_id: String,
    events: EventSender,
    states: Arc<dyn StateSink>,
}

impl JobContext {
    /// The runner (and VM) name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The runner is set up and about to take its job: marks the VM runner_active and tells the
    /// coordinator the runner started.
    pub async fn runner_started(&self) {
        self.states
            .set_state(&self.name, VmState::RunnerActive)
            .await;
        if let Err(e) = self
            .events
            .runner_event(
                self.name.clone(),
                self.vm_id.clone(),
                RunnerEventType::Started,
                String::new(),
            )
            .await
        {
            warn!("Failed to send runner started event: {}", e);
        }
    }
}

#[async_trait]
trait StateSink: Send + Sync {
    async fn set_state(&self, name: &str, state: VmState);
}

#[async_trait]
impl<V: Clone + Send + Sync> StateSink for Tracker<V> {
    async fn set_state(&self, name: &str, state: VmState) {
        Tracker::set_state(self, name, state).await
    }
}

/// Identifies the agent in `AgentStatus`.
pub struct AgentInfo {
    /// "proxmox", "tart", ...
    pub agent_type: String,
    /// The agent binary's version
    pub version: String,
}

/// How [`VmAgent::run`] connects and cleans up.
pub struct RunOptions {
    pub agent_config: AgentConfig,
    pub initial_reconnect_delay: Duration,
    pub max_reconnect_delay: Duration,
    /// Upload the agent's logs to the coordinator
    pub log_capture: Option<LogCapture>,
    /// How often to look for stale VMs
    pub cleanup_interval: Duration,
    /// VMs older than this are destroyed as stale
    pub max_vm_age: Duration,
}

/// Runs runner VMs on a [`VmBackend`] for the coordinator.
pub struct VmAgent<B: VmBackend> {
    backend: Arc<B>,
    tracker: Arc<Tracker<B::Vm>>,
    cert_store: AgentCertStore,
    info: AgentInfo,
    keep_failed_vms: bool,
}

impl<B: VmBackend> VmAgent<B> {
    pub fn new(backend: Arc<B>, cert_store: AgentCertStore, info: AgentInfo) -> Self {
        let tracker = Arc::new(Tracker::new(backend.limits()));
        Self {
            backend,
            tracker,
            cert_store,
            info,
            keep_failed_vms: false,
        }
    }

    /// Debugging: leave a failed runner's VM running and exit the agent, so you can SSH in.
    pub fn keep_failed_vms(mut self, keep: bool) -> Self {
        self.keep_failed_vms = keep;
        self
    }

    /// Connect to the coordinator and serve it until the command stream ends or a shutdown signal,
    /// which destroys every VM first.
    pub async fn run(self: Arc<Self>, options: RunOptions) {
        let cleanup = self.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(options.cleanup_interval);
            loop {
                ticker.tick().await;
                info!("Running stale VM cleanup");
                cleanup.cleanup_stale(options.max_vm_age).await;
            }
        });

        // live status: re-sent on every change, and on a timer by the runtime
        let (status_tx, status_rx) = tokio::sync::watch::channel(self.status().await);
        let bridge = self.clone();
        tokio::spawn(async move {
            let ours = bridge.tracker.changes();
            let backend = bridge.backend.changes();
            loop {
                match &backend {
                    Some(backend) => tokio::select! {
                        () = ours.notified() => {}
                        () = backend.notified() => {}
                    },
                    None => ours.notified().await,
                }
                if status_tx.send(bridge.status().await).is_err() {
                    break;
                }
            }
        });

        let connection = runtime::connect_with_logs(
            options.agent_config,
            self.cert_store.clone(),
            status_rx,
            options.initial_reconnect_delay,
            options.max_reconnect_delay,
            options.log_capture,
        );

        tokio::select! {
            () = self.clone().serve(connection) => {
                info!("Command stream ended");
            }
            () = shutdown_signal() => {
                info!("Shutdown signal received, cleaning up VMs...");
                self.destroy_all().await;
                info!("Graceful shutdown complete");
            }
        }
    }

    /// Read commands from the coordinator and act on them: take a slot and ack, then spawn the
    /// (long-running) lifecycle so the loop keeps draining.
    pub async fn serve(self: Arc<Self>, mut connection: runtime::Connection) {
        while let Some(command) = connection.commands.recv().await {
            match command {
                runtime::RunnerCommand::Create(cmd) => {
                    let image = self.backend.select_image(&cmd);
                    let limits = self.backend.image_limits(&image).await;
                    // an external VM may have started since the last count
                    self.backend.refresh_external().await;
                    let external = self.backend.external_usage().await;
                    if let Err(e) = self.tracker.reserve(&cmd.vm_name, limits, &external).await {
                        warn!("Rejecting CreateRunner for vm={}: {}", cmd.vm_name, e);
                        let _ = connection
                            .events
                            .command_ack(cmd.command_id, false, e.to_string())
                            .await;
                        continue;
                    }
                    let _ = connection
                        .events
                        .command_ack(cmd.command_id.clone(), true, String::new())
                        .await;
                    let agent = self.clone();
                    let events = connection.events.clone();
                    tokio::spawn(async move { agent.handle_create(cmd, image, events).await });
                }
                runtime::RunnerCommand::Destroy(cmd) => {
                    let _ = connection
                        .events
                        .command_ack(cmd.command_id.clone(), true, String::new())
                        .await;
                    let agent = self.clone();
                    let events = connection.events.clone();
                    tokio::spawn(async move { agent.handle_destroy(cmd.vm_id, events).await });
                }
            }
        }
    }

    /// Run a reserved runner's lifecycle to completion and report how it ended.
    async fn handle_create(&self, cmd: CreateRunnerCommand, image: String, events: EventSender) {
        let name = cmd.vm_name.clone();
        if cmd.registration_token.is_empty() && cmd.jit_config.is_empty() && cmd.forgejo.is_none() {
            warn!(
                "CreateRunner for {} has no registration token, JIT config or Forgejo job - \
                 runner configuration will fail. Is the coordinator in dry-run mode?",
                name
            );
        }
        info!(
            "CreateRunner starting: vm={}, image={}, labels={:?}",
            name, image, cmd.labels
        );

        match self.lifecycle(&cmd, &image, &events).await {
            Ok(vm_id) => {
                info!("Runner {} completed successfully", name);
                send_event(
                    &events,
                    name,
                    vm_id,
                    RunnerEventType::Completed,
                    String::new(),
                )
                .await;
            }
            Err(e) => {
                error!("Runner {} failed: {}", name, e);
                if self.keep_failed_vms {
                    error!("Debug mode: VM left running for inspection. Exiting agent.");
                    std::process::exit(1);
                }
                send_event(
                    &events,
                    name.clone(),
                    name,
                    RunnerEventType::Failed,
                    e.to_string(),
                )
                .await;
            }
        }
    }

    /// Create, start and run the job on a reserved VM, then destroy it. Returns the VM ID.
    async fn lifecycle(
        &self,
        cmd: &CreateRunnerCommand,
        image: &str,
        events: &EventSender,
    ) -> BackendResult<String> {
        let name = cmd.vm_name.as_str();
        let vm = match self.backend.create(name, image).await {
            Ok(vm) => vm,
            Err(e) => {
                error!("[{}] VM creation failed: {}", name, e);
                // nothing was created, so a destroy waiting on it is done
                self.tracker.untrack(name, Ok(())).await;
                return Err(e);
            }
        };
        let vm_id = self.backend.vm_id(&vm);
        info!("[{}] VM {} created from '{}'", name, vm_id, image);

        // creating can tell the backend more about the image (tart learns a pulled image's OS)
        let limits = self.backend.image_limits(image).await;
        if !self
            .tracker
            .set_created(name, vm.clone(), vm_id.clone(), limits)
            .await
        {
            warn!("[{}] VM was destroyed while being created", name);
            let result = self.backend.destroy(&vm, name).await;
            self.tracker.untrack(name, outcome(&result)).await;
            result?;
            return Err(format!("VM {name} was destroyed while being created").into());
        }

        let result = tokio::select! {
            result = self.start_and_run(&vm, &vm_id, cmd, events) => result,
            () = self.tracker.torn_down(name) => {
                warn!("[{}] VM was destroyed while its runner was running", name);
                Err(format!("VM {vm_id} ({name}) was destroyed while its runner was running").into())
            }
        };
        if result.is_err() && self.keep_failed_vms {
            return result.map(|()| vm_id);
        }

        // a no-op if a DestroyRunner or cleanup already has it
        if let Err(e) = self.destroy(name).await {
            warn!("[{}] Destroying VM failed: {}", name, e);
        }
        result.map(|()| vm_id)
    }

    async fn start_and_run(
        &self,
        vm: &B::Vm,
        vm_id: &str,
        cmd: &CreateRunnerCommand,
        events: &EventSender,
    ) -> BackendResult<()> {
        let name = cmd.vm_name.as_str();
        self.tracker.set_state(name, VmState::Starting).await;
        let ip = self.backend.start(vm).await.inspect_err(|e| {
            error!("[{}] VM failed to become ready: {}", name, e);
        })?;
        info!("[{}] VM ready at {}", name, ip);
        self.tracker.set_running(name, ip).await;

        self.tracker.set_state(name, VmState::Configuring).await;
        let job = Job {
            registration_token: &cmd.registration_token,
            labels: &cmd.labels,
            runner_scope_url: &cmd.runner_scope_url,
            runner_name: name,
            jit_config: &cmd.jit_config,
            forgejo: cmd.forgejo.as_deref(),
        };
        let ctx = JobContext {
            name: name.to_string(),
            vm_id: vm_id.to_string(),
            events: events.clone(),
            states: self.tracker.clone(),
        };
        self.backend.run_job(vm, ip, &job, &ctx).await
    }

    /// Destroy a DestroyRunner's VM, named by runner name or VM ID, and report it.
    async fn handle_destroy(&self, vm_id: String, events: EventSender) {
        // report under the runner name so the coordinator finds the runner, whichever it sent
        let (runner_name, result) = match self.tracker.find(&vm_id).await {
            Some(name) => {
                info!("DestroyRunner received for {} (runner {})", vm_id, name);
                let result = self.destroy(&name).await;
                (name, result)
            }
            None => {
                info!("DestroyRunner received for untracked VM {}", vm_id);
                (vm_id.clone(), self.backend.destroy_untracked(&vm_id).await)
            }
        };
        match result {
            Ok(()) => {
                info!("VM {} destroyed", vm_id);
                send_event(
                    &events,
                    runner_name,
                    vm_id,
                    RunnerEventType::Destroyed,
                    String::new(),
                )
                .await;
            }
            Err(e) => {
                error!("Failed to destroy VM {}: {}", vm_id, e);
                send_event(
                    &events,
                    runner_name,
                    vm_id,
                    RunnerEventType::Failed,
                    e.to_string(),
                )
                .await;
            }
        }
    }

    /// Destroy the VM for runner `name`. A no-op if it's untracked. If it's still being created, or another
    /// destroy has it, waits for that to finish, so a VM is really gone when this returns (shutdown relies on it).
    pub async fn destroy(&self, name: &str) -> BackendResult<()> {
        let vm = match self.tracker.claim_for_destroy(name).await {
            Claim::Untracked => return Ok(()),
            Claim::Wait(removal) => {
                info!("Waiting for VM {} to be destroyed", name);
                return Ok(removal.wait().await?);
            }
            Claim::Destroy(vm) => vm,
        };
        info!("Destroying VM {}", name);
        let started = Instant::now();
        let result = self.backend.destroy(&vm, name).await;
        self.tracker.untrack(name, outcome(&result)).await;
        info!(
            "VM {} destroyed in {:.1}s",
            name,
            started.elapsed().as_secs_f64()
        );
        result
    }

    /// Destroy VMs created more than `max_age` ago.
    pub async fn cleanup_stale(&self, max_age: Duration) {
        for name in self.tracker.older_than(max_age).await {
            warn!("Cleaning up stale VM: {}", name);
            if let Err(e) = self.destroy(&name).await {
                error!("Failed to clean up stale VM {}: {}", name, e);
            }
        }
    }

    /// Destroy every VM (used during shutdown).
    pub async fn destroy_all(&self) {
        let names = self.tracker.names().await;
        if names.is_empty() {
            info!("No active VMs to clean up");
            return;
        }
        info!("Destroying {} active VM(s)...", names.len());
        for name in names {
            if let Err(e) = self.destroy(&name).await {
                error!("Failed to destroy VM {}: {}", name, e);
            }
        }
    }

    /// The agent's current status.
    pub async fn status(&self) -> AgentStatus {
        let external = self.backend.external_usage().await;

        // each set says which limits its VMs use, so the coordinator doesn't hold linux jobs to the
        // macOS limit, say. Fixed-capacity pools use max_vms as their target, so it's the most any one
        // set can run, not the sum of the limits
        let mut label_sets = Vec::new();
        let mut available_slots = 0;
        let mut max_vms = 0;
        for capability in self.backend.capabilities() {
            let limits = self.backend.image_limits(&capability.id).await;
            available_slots = available_slots.max(self.tracker.free_slots(limits, &external).await);
            max_vms = max_vms.max(self.tracker.max_vms(limits));
            label_sets.push(LabelSet {
                labels: capability.labels,
                limits: limits.iter().map(|l| l.to_string()).collect(),
                pool_size: capability.pool,
                id: capability.id,
                is_default: capability.is_default,
            });
        }

        AgentStatus {
            active_vms: self.tracker.count().await as u32,
            available_slots,
            vms: self.tracker.vm_infos().await,
            agent_id: self.cert_store.get_agent_id().unwrap_or_default(),
            hostname: gethostname::gethostname().to_string_lossy().to_string(),
            agent_type: self.info.agent_type.clone(),
            labels: self.backend.labels(),
            max_vms,
            label_sets,
            agent_version: self.info.version.clone(),
            limits: self.tracker.capacity_limits(&external).await,
            // forgejo-runner one-job, from kuiper-forge's forgejo-runner releases
            runner_kinds: vec!["forgejo".to_string()],
        }
    }
}

/// A destroy's result, for anyone waiting on it.
fn outcome(result: &BackendResult<()>) -> Result<(), String> {
    result.as_ref().map(|_| ()).map_err(|e| e.to_string())
}

async fn send_event(
    events: &EventSender,
    runner_name: String,
    vm_id: String,
    event_type: RunnerEventType,
    error: String,
) {
    if let Err(e) = events
        .runner_event(runner_name, vm_id, event_type, error)
        .await
    {
        error!("Failed to send runner event: {}", e);
    }
}

/// Wait for a shutdown signal (SIGTERM or SIGINT/Ctrl-C).
pub async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("Failed to install Ctrl-C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("Failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => info!("Received Ctrl-C"),
        () = terminate => info!("Received SIGTERM"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use tokio::sync::Semaphore;

    /// A backend whose creates block until `clones` hands out a permit.
    struct SlowCloneBackend {
        clones: Semaphore,
        destroyed: AtomicU32,
    }

    #[async_trait]
    impl VmBackend for SlowCloneBackend {
        type Vm = u32;

        fn labels(&self) -> Vec<String> {
            Vec::new()
        }
        fn capabilities(&self) -> Vec<Capability> {
            Vec::new()
        }
        fn select_image(&self, _cmd: &CreateRunnerCommand) -> String {
            "image".into()
        }
        fn limits(&self) -> Vec<Limit> {
            vec![Limit {
                name: "total",
                max: 4,
            }]
        }
        async fn image_limits(&self, _image: &str) -> LimitNames {
            &["total"]
        }
        async fn create(&self, _name: &str, _image: &str) -> BackendResult<u32> {
            self.clones.acquire().await.unwrap().forget();
            Ok(7)
        }
        fn vm_id(&self, vm: &u32) -> String {
            vm.to_string()
        }
        async fn start(&self, _vm: &u32) -> BackendResult<IpAddr> {
            unreachable!("destroyed before it starts")
        }
        async fn run_job(
            &self,
            _vm: &u32,
            _ip: IpAddr,
            _job: &Job<'_>,
            _ctx: &JobContext,
        ) -> BackendResult<()> {
            unreachable!("destroyed before it starts")
        }
        async fn destroy(&self, _vm: &u32, _name: &str) -> BackendResult<()> {
            self.destroyed.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_destroy_waits_for_create_in_flight() {
        let backend = Arc::new(SlowCloneBackend {
            clones: Semaphore::new(0),
            destroyed: AtomicU32::new(0),
        });
        let dir = std::env::temp_dir().join("kuiper-vm-agent-test-certs");
        let agent = Arc::new(VmAgent::new(
            backend.clone(),
            AgentCertStore::new(dir),
            AgentInfo {
                agent_type: "test".into(),
                version: "0".into(),
            },
        ));
        let (events, _rx) = EventSender::for_test();
        let cmd = CreateRunnerCommand {
            vm_name: "runner-1".into(),
            ..Default::default()
        };
        agent
            .tracker
            .reserve("runner-1", &["total"], &Usage::new())
            .await
            .unwrap();
        let lifecycle = {
            let agent = agent.clone();
            tokio::spawn(async move { agent.lifecycle(&cmd, "image", &events).await })
        };

        // a DestroyRunner (or shutdown) arrives mid-clone
        let destroy = {
            let agent = agent.clone();
            tokio::spawn(async move { agent.destroy("runner-1").await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!destroy.is_finished(), "destroy must wait for the clone");
        assert_eq!(backend.destroyed.load(Ordering::SeqCst), 0);

        // the clone finishes: the lifecycle destroys the new VM, then the destroy returns
        backend.clones.add_permits(1);
        destroy.await.unwrap().unwrap();
        assert_eq!(backend.destroyed.load(Ordering::SeqCst), 1);
        assert_eq!(agent.tracker.count().await, 0);
        assert!(lifecycle.await.unwrap().is_err());
    }
}
