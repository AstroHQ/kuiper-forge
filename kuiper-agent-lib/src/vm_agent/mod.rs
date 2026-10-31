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
use kuiper_agent_proto::{
    AgentStatus, CreateRunnerCommand, LabelSet, RunnerEventType, RunnerTimings,
};
use tokio::sync::Notify;
use tokio::task::JoinSet;
use tokio::time::Instant;
use tracing::{error, info, warn};

use crate::guest::Job;
use crate::labels::Capability;
use crate::runner_cache::InstallSource;
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
    clock: Arc<PhaseClock>,
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
        self.clock.next(Some(step::JOB));
        if let Err(e) = self
            .events
            .runner_event(
                self.name.clone(),
                self.vm_id.clone(),
                RunnerEventType::Started,
                String::new(),
                Some(self.clock.timings()),
            )
            .await
        {
            warn!("Failed to send runner started event: {}", e);
        }
    }

    /// Time what follows as `step` until the next call or [`Self::runner_started`], say [`step::INSTALL`] around the
    /// runner download and then [`step::SETUP`] again.
    pub fn step(&self, step: &str) {
        self.clock.next(Some(step));
    }

    /// `step` wasn't needed, say [`step::INSTALL`] when the image already has the runner. Shows as skipped rather than
    /// missing.
    pub fn skip_step(&self, step: &str) {
        self.clock.skip(step);
    }

    /// Where the runner came from, for the metrics page. Not needed if the image has it, that's a skipped
    /// [`step::INSTALL`].
    pub fn install_source(&self, source: InstallSource) {
        self.clock.install_source(source);
    }
}

/// Names of the steps a runner's time is split into, see `RunnerTimings` in the proto. Backends can time their own
/// steps with [`JobContext::step`].
pub mod step {
    pub const CREATE: &str = "create";
    pub const BOOT: &str = "boot";
    /// Getting the runner onto the VM, inside setup. Skipped when the image already has it
    pub const INSTALL: &str = "install";
    pub const SETUP: &str = "setup";
    pub const JOB: &str = "job";
    pub const DESTROY: &str = "destroy";
}

/// Times a runner's steps for the coordinator's metrics. Shared with [`JobContext`], so backends can mark steps inside
/// their run.
struct PhaseClock {
    state: std::sync::Mutex<ClockState>,
}

struct ClockState {
    timings: RunnerTimings,
    /// None once the runner is done
    step: Option<String>,
    since: Instant,
    /// the current step failed, so how long it ran isn't a real duration for that step
    failed: bool,
}

impl PhaseClock {
    fn new(image: &str) -> Self {
        Self {
            state: std::sync::Mutex::new(ClockState {
                timings: RunnerTimings {
                    image: image.to_string(),
                    ..Default::default()
                },
                step: Some(step::CREATE.to_string()),
                since: Instant::now(),
                failed: false,
            }),
        }
    }

    /// End the current step and start `next`, or stop timing for None. A step started again adds to its time.
    fn next(&self, next: Option<&str>) {
        let state = &mut *self.state.lock().unwrap();
        let ms = state.since.elapsed().as_millis() as u64;
        if !state.failed
            && let Some(step) = &state.step
        {
            *state.timings.steps_ms.entry(step.clone()).or_default() += ms;
        }
        state.step = next.map(String::from);
        state.since = Instant::now();
        state.failed = false;
    }

    /// Record `step` as skipped, unless it already has a time.
    fn skip(&self, step: &str) {
        let mut state = self.state.lock().unwrap();
        state.timings.steps_ms.entry(step.to_string()).or_default();
    }

    fn install_source(&self, source: InstallSource) {
        self.state.lock().unwrap().timings.install_source = source.as_str().to_string();
    }

    /// The current step failed. Only the first failure is kept: the destroy after a failed boot doesn't replace it.
    fn fail(&self) {
        let state = &mut *self.state.lock().unwrap();
        state.failed = true;
        if let Some(step) = &state.step
            && state.timings.failed_phase.is_empty()
        {
            // an earlier stint of a step that later failed isn't its duration either
            state.timings.steps_ms.remove(step);
            state.timings.failed_phase = step.clone();
        }
    }

    fn timings(&self) -> RunnerTimings {
        self.state.lock().unwrap().timings.clone()
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

        let clock = Arc::new(PhaseClock::new(&image));
        match self.lifecycle(&cmd, &image, &events, &clock).await {
            Ok(vm_id) => {
                info!("Runner {} completed successfully", name);
                send_event(
                    &events,
                    name,
                    vm_id,
                    RunnerEventType::Completed,
                    String::new(),
                    Some(clock.timings()),
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
                    Some(clock.timings()),
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
        clock: &Arc<PhaseClock>,
    ) -> BackendResult<String> {
        let name = cmd.vm_name.as_str();
        let vm = match self.backend.create(name, image).await {
            Ok(vm) => vm,
            Err(e) => {
                error!("[{}] VM creation failed: {}", name, e);
                clock.fail();
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
            clock.fail();
            let result = self.backend.destroy(&vm, name).await;
            self.tracker.untrack(name, outcome(&result)).await;
            result?;
            return Err(format!("VM {name} was destroyed while being created").into());
        }

        clock.next(Some(step::BOOT));
        let result = tokio::select! {
            result = self.start_and_run(&vm, &vm_id, cmd, events, clock) => result,
            () = self.tracker.torn_down(name) => {
                warn!("[{}] VM was destroyed while its runner was running", name);
                Err(format!("VM {vm_id} ({name}) was destroyed while its runner was running").into())
            }
        };
        if result.is_err() {
            clock.fail();
            if self.keep_failed_vms {
                return result.map(|()| vm_id);
            }
        }

        // a no-op if a DestroyRunner or cleanup already has it
        clock.next(Some(step::DESTROY));
        if let Err(e) = self.destroy(name).await {
            warn!("[{}] Destroying VM failed: {}", name, e);
            clock.fail();
        }
        clock.next(None);
        result.map(|()| vm_id)
    }

    async fn start_and_run(
        &self,
        vm: &B::Vm,
        vm_id: &str,
        cmd: &CreateRunnerCommand,
        events: &EventSender,
        clock: &Arc<PhaseClock>,
    ) -> BackendResult<()> {
        let name = cmd.vm_name.as_str();
        self.tracker.set_state(name, VmState::Starting).await;
        let ip = self.backend.start(vm).await.inspect_err(|e| {
            error!("[{}] VM failed to become ready: {}", name, e);
        })?;
        info!("[{}] VM ready at {}", name, ip);
        self.tracker.set_running(name, ip).await;
        clock.next(Some(step::SETUP));

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
            clock: clock.clone(),
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
                    None,
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
                    None,
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

    /// Destroy every VM (used during shutdown). They go in parallel, so shutdown takes as long as the slowest
    /// one (a create still in flight, say) rather than all of them added up.
    pub async fn destroy_all(self: &Arc<Self>) {
        let names = self.tracker.names().await;
        if names.is_empty() {
            info!("No active VMs to clean up");
            return;
        }
        info!("Destroying {} active VM(s)...", names.len());
        let mut destroys = JoinSet::new();
        for name in names {
            let agent = self.clone();
            destroys.spawn(async move {
                if let Err(e) = agent.destroy(&name).await {
                    error!("Failed to destroy VM {}: {}", name, e);
                }
            });
        }
        destroys.join_all().await;
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
    timings: Option<RunnerTimings>,
) {
    if let Err(e) = events
        .runner_event(runner_name, vm_id, event_type, error, timings)
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
            let clock = Arc::new(PhaseClock::new("image"));
            tokio::spawn(async move { agent.lifecycle(&cmd, "image", &events, &clock).await })
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

    fn ms(t: &RunnerTimings, step: &str) -> Option<u64> {
        t.steps_ms.get(step).copied()
    }

    #[test]
    fn test_phase_clock_skips_the_failed_step() {
        let clock = PhaseClock::new("macos-15");
        clock.next(Some(step::BOOT));
        clock.fail();
        clock.next(Some(step::DESTROY));
        clock.next(None);

        let t = clock.timings();
        assert_eq!(t.image, "macos-15");
        assert_eq!(t.failed_phase, "boot");
        assert!(ms(&t, step::CREATE).is_some());
        assert_eq!(ms(&t, step::BOOT), None, "a failed boot isn't a boot time");
        assert!(ms(&t, step::DESTROY).is_some());
        assert_eq!(t.steps_ms.len(), 2);
    }

    #[test]
    fn test_phase_clock_keeps_the_first_failure() {
        let clock = PhaseClock::new("image");
        clock.next(Some(step::BOOT));
        clock.next(Some(step::SETUP));
        clock.next(Some(step::JOB));
        clock.fail();
        clock.next(Some(step::DESTROY));
        clock.fail();
        clock.next(None);

        let t = clock.timings();
        assert_eq!(t.failed_phase, "job");
        assert!(ms(&t, step::SETUP).is_some());
        assert_eq!(ms(&t, step::JOB), None);
        assert_eq!(ms(&t, step::DESTROY), None);
    }

    #[tokio::test(start_paused = true)]
    async fn test_phase_clock_install_inside_setup() {
        let clock = PhaseClock::new("image");
        clock.next(Some(step::SETUP));
        tokio::time::advance(Duration::from_millis(100)).await;
        clock.next(Some(step::INSTALL));
        tokio::time::advance(Duration::from_millis(5_000)).await;
        clock.next(Some(step::SETUP));
        tokio::time::advance(Duration::from_millis(200)).await;
        clock.next(Some(step::JOB));

        let t = clock.timings();
        assert_eq!(ms(&t, step::INSTALL), Some(5_000));
        assert_eq!(
            ms(&t, step::SETUP),
            Some(300),
            "setup adds up around the install"
        );
    }

    #[test]
    fn test_phase_clock_skipped_step() {
        let clock = PhaseClock::new("image");
        clock.next(Some(step::SETUP));
        clock.skip(step::INSTALL);
        clock.next(Some(step::JOB));
        assert_eq!(ms(&clock.timings(), step::INSTALL), Some(0));
    }

    #[test]
    fn test_phase_clock_install_source() {
        let clock = PhaseClock::new("image");
        assert_eq!(clock.timings().install_source, "");
        clock.install_source(InstallSource::FellBack);
        assert_eq!(clock.timings().install_source, "fell_back");
    }

    #[test]
    fn test_phase_clock_failed_install_isnt_timed() {
        let clock = PhaseClock::new("image");
        clock.next(Some(step::SETUP));
        clock.next(Some(step::INSTALL));
        clock.fail();
        clock.next(Some(step::DESTROY));
        let t = clock.timings();
        assert_eq!(t.failed_phase, "install");
        assert_eq!(ms(&t, step::INSTALL), None);
        assert!(ms(&t, step::SETUP).is_some());
    }
}
