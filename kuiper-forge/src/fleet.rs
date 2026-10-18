//! Fleet manager for maintaining runner pools.
//!
//! The fleet manager is responsible for:
//! - Maintaining a target number of runners per configuration (fixed capacity mode)
//! - Creating runners on-demand from webhook events (webhook mode)
//! - Getting registration tokens from GitHub (or mock provider in dry-run mode)
//! - Recovering runners when agents reconnect after coordinator restart

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use kuiper_agent_proto::{
    AgentPayload, CoordinatorMessage, CoordinatorPayload, CreateRunnerCommand,
    DestroyRunnerCommand, ForgejoRunner, RunnerEvent, RunnerEventType,
};

use crate::agent_failures::{AgentFailureStore, FailureKind};
use crate::agent_registry::{AgentRegistry, PoolDefinition, normalize_labels};
use crate::config::{Config, ProvisioningMode, RunnerScope};
use crate::forge::{Forge, ForgeRegistry, JobKey, RunnerKind, RunnerLaunch};
use crate::pending_jobs::{PendingJobInfo, PendingJobStore};
use crate::runner_state::RunnerStateStore;
use crate::webhook::{WebhookEvent, WebhookNotifier};

/// How often webhook mode scans forges for queued jobs it missed.
pub const RECOVERY_SCAN_INTERVAL: Duration = Duration::from_secs(300);

/// Longest wait before a job whose runner sat idle gets another one.
const MAX_IDLE_BACKOFF: Duration = Duration::from_secs(3600);

/// How long a destroy command for an idle runner may go unanswered before it's sent again.
const IDLE_DESTROY_RETRY: Duration = Duration::from_secs(120);

/// How long late events for a torn-down idle runner are ignored.
const TORN_DOWN_MEMORY: Duration = Duration::from_secs(600);

/// What to do with a job-bound runner past the idle timeout, from its job's status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IdleAction {
    /// The job is running: the runner isn't idle, stop checking it.
    Started,
    /// The job is still waiting (or gone, e.g. cancelled): the runner can't do anything, destroy it.
    TearDown,
    /// Couldn't tell, or the job is finishing: leave it and check next time.
    Wait,
}

fn idle_action(status: &anyhow::Result<Option<String>>) -> IdleAction {
    match status {
        Ok(Some(s)) if s == "in_progress" => IdleAction::Started,
        Ok(Some(s)) if s == "queued" => IdleAction::TearDown,
        Ok(None) => IdleAction::TearDown,
        Ok(Some(_)) | Err(_) => IdleAction::Wait,
    }
}

/// The delay before a job gets another runner after an idle teardown: the idle
/// timeout the first time, doubling each time after, at most [`MAX_IDLE_BACKOFF`].
fn next_idle_backoff(previous: Option<Duration>, timeout: Duration) -> Duration {
    match previous {
        None => timeout,
        Some(delay) => delay * 2,
    }
    .min(MAX_IDLE_BACKOFF)
}

/// In-memory bookkeeping for idle job-bound runners. Lost on restart, which at worst
/// means one more runner for a job that's being held back.
#[derive(Default)]
struct IdleRunners {
    /// Runners whose job was seen running: not idle, not checked again
    started: HashSet<String>,
    /// Runners being destroyed for idling, and when the destroy was sent. Their
    /// events aren't failures
    tearing_down: HashMap<String, std::time::Instant>,
    /// Runners recently torn down for idling, so their late events (the agent sends
    /// one for the killed job and one for the destroy) aren't recorded as failures
    torn_down: HashMap<String, std::time::Instant>,
    /// Jobs whose runner idled: not given a new runner before `until`
    backoff: HashMap<JobKey, IdleBackoff>,
}

#[derive(Debug, Clone, Copy)]
struct IdleBackoff {
    until: std::time::Instant,
    delay: Duration,
}

/// Information about an agent's VMs for recovery.
#[derive(Debug, Clone)]
pub struct AgentRecoveryInfo {
    /// Agent ID
    pub agent_id: String,
    /// VM names currently running on the agent
    pub vm_names: Vec<String>,
}

/// Event emitted by an agent about a runner lifecycle.
#[derive(Debug, Clone)]
pub struct AgentRunnerEvent {
    /// Agent ID
    pub agent_id: String,
    /// Runner event payload
    pub event: RunnerEvent,
}

/// Handle for triggering fleet manager reconciliation.
#[derive(Clone)]
pub struct FleetNotifier {
    notify_tx: mpsc::Sender<()>,
    recovery_tx: mpsc::Sender<AgentRecoveryInfo>,
    runner_event_tx: mpsc::Sender<AgentRunnerEvent>,
}

impl FleetNotifier {
    /// Notify the fleet manager to reconcile immediately.
    ///
    /// Called when an agent connects to check if runners need to be created.
    pub async fn notify(&self) {
        // Use try_send to avoid blocking - if channel is full, reconciliation is already pending
        if self.notify_tx.try_send(()).is_err() {
            debug!("Fleet notify channel full - reconciliation already pending");
        }
    }

    /// Notify with recovery info when an agent has existing VMs.
    ///
    /// Called when an agent connects with VMs that may match persisted runners.
    /// The fleet manager will spawn watchers for matching runners.
    pub async fn notify_with_recovery(&self, agent_id: String, vm_names: Vec<String>) {
        if self
            .recovery_tx
            .try_send(AgentRecoveryInfo {
                agent_id: agent_id.clone(),
                vm_names,
            })
            .is_err()
        {
            warn!(
                agent_id = %agent_id,
                "Fleet recovery channel full - recovery info dropped"
            );
        }
        // Also trigger a reconcile
        let _ = self.notify_tx.try_send(());
    }

    /// Notify with a runner lifecycle event from an agent.
    pub async fn notify_runner_event(&self, agent_id: String, event: RunnerEvent) {
        if self
            .runner_event_tx
            .try_send(AgentRunnerEvent {
                agent_id: agent_id.clone(),
                event,
            })
            .is_err()
        {
            warn!(
                agent_id = %agent_id,
                "Fleet runner event channel full - event dropped, may delay cleanup"
            );
        }
        let _ = self.notify_tx.try_send(());
    }

    /// Same as `notify_runner_event` but waits for channel space. For synthetic one-shot events
    /// (disconnect failover) that nothing regenerates if dropped.
    pub async fn send_runner_event(&self, agent_id: String, event: RunnerEvent) {
        if self
            .runner_event_tx
            .send(AgentRunnerEvent {
                agent_id: agent_id.clone(),
                event,
            })
            .await
            .is_err()
        {
            error!(agent_id = %agent_id, "Fleet runner event channel closed - event lost");
        }
        let _ = self.notify_tx.try_send(());
    }
}

/// What the forge says about a job after the runner we made for it finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobCheck {
    /// Still queued: the runner ran some other same-label job, ours needs a new runner.
    Queued,
    /// Running, finished, or gone: safe to drop the pending record.
    Done,
    /// The forge didn't answer: keep the record, the periodic verify settles it.
    Unknown,
}

/// JIT runners aren't bound to a job: any queued job with matching labels can grab any idle runner.
/// So "the runner we made for job X finished" doesn't mean X ran. Check before dropping X from the
/// pending queue, or the last job in a same-label burst is left queued with no runner and no record.
async fn check_job_after_runner(
    forge: &dyn Forge,
    repository: Option<&str>,
    job_id: &JobKey,
) -> JobCheck {
    let Some(repo) = repository else {
        // nothing to ask with, ever. keeping such a record would re-provision a runner every time
        // one finished until the stale sweep removed it, so fall back to trusting the runner
        return JobCheck::Done;
    };
    match forge.get_job_status(repo, job_id.id()).await {
        Ok(Some(status)) if status == "queued" => JobCheck::Queued,
        Ok(_) => JobCheck::Done,
        Err(e) => {
            warn!(job_id = %job_id, "Could not verify job status after runner completion: {e}");
            JobCheck::Unknown
        }
    }
}

/// Fleet manager that maintains runner pools.
pub struct FleetManager {
    /// Configuration
    config: Config,
    /// Forges runners register with (GitHub/Forgejo API or mock), by configured name
    forges: Arc<ForgeRegistry>,
    /// Agent registry for sending commands
    agent_registry: Arc<AgentRegistry>,
    /// Persistent runner state for crash recovery
    runner_state: Arc<RunnerStateStore>,
    /// Persistent pending jobs store (webhook mode only)
    pending_job_store: Arc<PendingJobStore>,
    /// Recent per-agent failures for the admin UI
    failures: Arc<AgentFailureStore>,
    /// Channel for receiving reconciliation notifications
    notify_rx: mpsc::Receiver<()>,
    /// Channel for receiving recovery notifications
    recovery_rx: mpsc::Receiver<AgentRecoveryInfo>,
    /// Channel for receiving runner lifecycle events
    runner_event_rx: mpsc::Receiver<AgentRunnerEvent>,
    /// Channel for receiving webhook events (webhook mode only)
    webhook_rx: Option<mpsc::Receiver<WebhookEvent>>,
    /// Sender for self-notification (e.g., from spawned tasks that need to trigger reprocessing)
    notify_tx: mpsc::Sender<()>,
    /// Agents already warned about label sets that fixed-capacity provisioning
    /// can't pre-create. Warn once per agent to avoid log spam on each reconcile.
    warned_capability_agents: std::sync::Mutex<std::collections::HashSet<String>>,
    /// Idle job-bound runners being watched or torn down, and jobs backing off
    idle: std::sync::Mutex<IdleRunners>,
}

impl FleetManager {
    /// The forge a stored runner is registered with. A runner recorded against a
    /// forge that's no longer configured falls back to the default, where cleanup
    /// calls fail harmlessly.
    fn runner_forge(&self, forge: Option<&str>) -> Arc<dyn Forge> {
        self.forges.for_runner(forge).unwrap_or_else(|e| {
            warn!("{e} - using the default forge for runner cleanup");
            self.forges.default_forge()
        })
    }

    /// Picks a unique runner/VM name that starts with the agent's short hostname, so you can tell where a runner
    /// lives from GitHub's runner list.
    async fn new_runner_name(&self, agent_id: &str) -> String {
        let hostname = match self.agent_registry.get(agent_id).await {
            Some(agent) => agent.read().await.hostname.clone(),
            None => String::new(),
        };
        runner_name_for_host(&hostname, &Uuid::new_v4().simple().to_string()[..8])
    }

    /// Create a new fleet manager and notification handles.
    ///
    /// Returns the fleet manager, a notifier for triggering reconciliation,
    /// and optionally a webhook notifier (only in webhook provisioning mode).
    pub fn new(
        config: Config,
        forges: Arc<ForgeRegistry>,
        agent_registry: Arc<AgentRegistry>,
        runner_state: Arc<RunnerStateStore>,
        pending_job_store: Arc<PendingJobStore>,
        failures: Arc<AgentFailureStore>,
    ) -> (Self, FleetNotifier, Option<WebhookNotifier>) {
        let (notify_tx, notify_rx) = mpsc::channel(64);
        let (recovery_tx, recovery_rx) = mpsc::channel(64);
        let (runner_event_tx, runner_event_rx) = mpsc::channel(128);

        // Create webhook channel only in webhook mode
        let (webhook_rx, webhook_notifier) = match config.provisioning.mode {
            ProvisioningMode::Webhook => {
                let (tx, rx) = mpsc::channel(32);
                (Some(rx), Some(WebhookNotifier::new(tx)))
            }
            ProvisioningMode::FixedCapacity | ProvisioningMode::Polling => (None, None),
        };

        let manager = Self {
            config,
            forges,
            agent_registry,
            runner_state,
            pending_job_store,
            failures,
            notify_rx,
            recovery_rx,
            runner_event_rx,
            webhook_rx,
            notify_tx: notify_tx.clone(),
            warned_capability_agents: std::sync::Mutex::new(std::collections::HashSet::new()),
            idle: std::sync::Mutex::new(IdleRunners::default()),
        };

        let notifier = FleetNotifier {
            notify_tx,
            recovery_tx,
            runner_event_tx,
        };

        (manager, notifier, webhook_notifier)
    }

    /// Start the fleet manager loop.
    ///
    /// This runs forever, periodically checking and maintaining runner pools.
    /// Also responds to notifications for immediate reconciliation and recovery.
    ///
    /// Behavior depends on provisioning mode:
    /// - **Fixed Capacity**: Actively reconciles runner pools to maintain target counts.
    /// - **Webhook**: Creates runners on-demand for jobs delivered by webhook events.
    /// - **Polling**: Creates runners on-demand for jobs found by scanning the forges' APIs.
    ///
    /// All modes handle runner events and recovery for runners that were created.
    pub async fn run(mut self) {
        let provisioning_mode = self.config.provisioning.mode;

        match provisioning_mode {
            ProvisioningMode::FixedCapacity => {
                info!("Fleet manager started in FIXED CAPACITY mode");
                info!("Pools will be derived from connected agents");
                self.run_fixed_capacity_loop().await;
            }
            ProvisioningMode::Webhook => {
                info!("Fleet manager started in WEBHOOK mode");
                info!("Runners will be created on-demand via GitHub webhook events");
                if self.config.provisioning.webhook.is_none() {
                    warn!("Webhook mode enabled but no webhook configuration found!");
                    warn!(
                        "Add [provisioning.webhook] section to your config to receive webhook events"
                    );
                }
                self.run_on_demand_loop().await;
            }
            ProvisioningMode::Polling => {
                let interval = self.config.provisioning.polling_config().interval();
                info!("Fleet manager started in POLLING mode");
                info!(
                    "Runners will be created on-demand for queued jobs found by scanning forges every {}s",
                    interval.as_secs()
                );
                let polls_github = self
                    .forges
                    .iter()
                    .any(|(_, forge)| forge.runner_kind() == crate::forge::RunnerKind::GitHub);
                if polls_github && interval < Duration::from_secs(60) {
                    warn!(
                        "Polling interval {}s is short: each scan lists workflow runs per repo and may exhaust GitHub API rate limits",
                        interval.as_secs()
                    );
                }
                self.run_on_demand_loop().await;
            }
        }
    }

    /// Run the fixed capacity provisioning loop.
    ///
    /// Periodically reconciles runner pools to maintain target counts.
    async fn run_fixed_capacity_loop(&mut self) {
        let check_interval = Duration::from_secs(30);
        let mut ticker = tokio::time::interval(check_interval);

        // On startup, log any persisted runners (from previous coordinator run)
        match self.runner_state.get_all_runners().await {
            Ok(persisted) if !persisted.is_empty() => {
                info!(
                    "Found {} persisted runner(s) from previous run - will recover when agents reconnect",
                    persisted.len()
                );
                for (name, info) in &persisted {
                    info!(
                        "  Persisted runner '{}' on agent '{}' (created {})",
                        name, info.agent_id, info.created_at
                    );
                }
            }
            Ok(_) => {}
            Err(e) => {
                error!("Failed to load persisted runners: {}", e);
            }
        }

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    self.reconcile().await;
                }
                Some(()) = self.notify_rx.recv() => {
                    info!("Fleet manager triggered by agent connection");
                    self.reconcile().await;
                }
                Some(recovery_info) = self.recovery_rx.recv() => {
                    info!(
                        "Fleet manager received recovery info from agent '{}' with {} VMs",
                        recovery_info.agent_id, recovery_info.vm_names.len()
                    );
                    self.handle_agent_recovery(recovery_info).await;
                }
                Some(event) = self.runner_event_rx.recv() => {
                    self.handle_runner_event(event).await;
                }
            }
        }
    }

    /// Run the on-demand provisioning loop (webhook and polling modes).
    ///
    /// Runners are created for jobs in the pending queue. In webhook mode the queue
    /// is fed by webhook events, with a forge scan every 5 minutes as a safety net.
    /// In polling mode the scan is the only source and runs every `interval_secs`.
    /// This loop also handles recovery and runner lifecycle events.
    ///
    /// A periodic check ensures pending jobs are processed even if notifications
    /// are lost (e.g., channel was full when webhook handler tried to notify).
    async fn run_on_demand_loop(&mut self) {
        let polling = self.config.provisioning.mode == ProvisioningMode::Polling;
        // On startup, log any persisted runners (from previous coordinator run)
        match self.runner_state.get_all_runners().await {
            Ok(persisted) if !persisted.is_empty() => {
                info!(
                    "Found {} persisted runner(s) from previous run - will recover when agents reconnect",
                    persisted.len()
                );
                for (name, info) in &persisted {
                    info!(
                        "  Persisted runner '{}' on agent '{}' (created {})",
                        name, info.agent_id, info.created_at
                    );
                }
            }
            Ok(_) => {}
            Err(e) => {
                error!("Failed to load persisted runners: {}", e);
            }
        }

        if polling {
            info!("Polling provisioning mode active - scanning forges for queued jobs");
        } else {
            info!("Webhook provisioning mode active - waiting for webhook events");
        }

        // Process any pending jobs from previous run on startup
        info!("Checking for pending jobs from previous run...");
        self.process_pending_jobs().await;

        // Scan forges for queued jobs: in webhook mode, ones missed during downtime;
        // in polling mode, this is the first poll
        self.scan_forges().await;

        // Forge scan interval - in webhook mode, verify pending jobs are still queued
        // and recover missed ones; in polling mode, the poll itself.
        // Tracked as an elapsed-time check on the ticker rather than a separate
        // select! branch, because select! can starve lower-priority branches when
        // higher-priority ones (notifications, events) have constant traffic.
        // also the backstop that re-adds jobs lost to runner steals, so keep it short-ish
        let forge_scan_interval = if polling {
            self.config.provisioning.polling_config().interval()
        } else {
            RECOVERY_SCAN_INTERVAL
        };
        let mut last_forge_scan = tokio::time::Instant::now();

        // Periodic check interval - ensures pending jobs are processed even if
        // notifications are lost (belt-and-suspenders with the DB-backed queue).
        // Never slower than the scan, so a short poll interval is honoured.
        let check_interval = Duration::from_secs(30).min(forge_scan_interval);
        let mut ticker = tokio::time::interval(check_interval);

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    // Periodic check for pending jobs (in case notifications were lost)
                    self.process_pending_jobs().await;

                    // Destroy job-bound runners whose job never started
                    self.check_idle_runners().await;

                    // Piggyback the forge scan on the ticker to avoid select!
                    // starvation (guaranteed to run within one tick of being due)
                    if last_forge_scan.elapsed() >= forge_scan_interval {
                        last_forge_scan = tokio::time::Instant::now();
                        self.scan_forges().await;
                    }
                }
                Some(()) = self.notify_rx.recv() => {
                    // Agent connected - check if we can assign pending jobs
                    info!("Agent notification received - checking pending job queue");
                    self.process_pending_jobs().await;
                }
                Some(recovery_info) = self.recovery_rx.recv() => {
                    info!(
                        "Fleet manager received recovery info from agent '{}' with {} VMs",
                        recovery_info.agent_id, recovery_info.vm_names.len()
                    );
                    self.handle_agent_recovery(recovery_info).await;
                }
                Some(event) = self.runner_event_rx.recv() => {
                    self.handle_runner_event(event).await;
                }
                Some(event) = async {
                    match &mut self.webhook_rx {
                        Some(rx) => rx.recv().await,
                        None => std::future::pending::<Option<WebhookEvent>>().await,
                    }
                } => {
                    self.handle_webhook_event(event).await;
                }
            }
        }
    }

    /// Destroy job-bound runners whose job hasn't started within the idle timeout.
    ///
    /// Only runners bound to one job (Forgejo one-job runners) are checked: such a
    /// runner can only ever run its own job, so if that job is still waiting (held
    /// back by a concurrency group, say) or gone (cancelled), the runner and its VM
    /// are idle. The job stays pending and gets a new runner after a growing delay.
    /// GitHub JIT runners can take any queued job with matching labels, so "its job
    /// is still queued" doesn't mean they're idle; they're left alone.
    async fn check_idle_runners(&self) {
        let Some(timeout) = self.config.provisioning.idle_runner_timeout() else {
            return;
        };
        let runners = match self.runner_state.get_all_runners().await {
            Ok(runners) => runners,
            Err(e) => {
                warn!("Idle runner check: failed to list runners: {}", e);
                return;
            }
        };
        let now = chrono::Utc::now();

        // forget runners that no longer exist
        {
            let names: HashSet<&str> = runners.iter().map(|(n, _)| n.as_str()).collect();
            let mut idle = self.idle.lock().unwrap();
            idle.started.retain(|n| names.contains(n.as_str()));
            idle.tearing_down.retain(|n, _| names.contains(n.as_str()));
            idle.torn_down
                .retain(|_, at| at.elapsed() < TORN_DOWN_MEMORY);
        }

        for (runner_name, info) in runners {
            let Some(job_id) = info.job_id.clone() else {
                continue;
            };
            let Ok(forge) = self.forges.for_job(&job_id) else {
                continue;
            };
            if forge.runner_kind() != RunnerKind::Forgejo {
                continue;
            }
            let age = (now - info.created_at).to_std().unwrap_or_default();
            if age < timeout {
                continue;
            }
            {
                let idle = self.idle.lock().unwrap();
                if idle.started.contains(&runner_name)
                    || idle
                        .tearing_down
                        .get(&runner_name)
                        .is_some_and(|sent| sent.elapsed() < IDLE_DESTROY_RETRY)
                {
                    continue;
                }
            }
            let Some(repo) = info.repository.as_deref() else {
                continue;
            };

            let status = forge.get_job_status(repo, job_id.id()).await;
            match idle_action(&status) {
                IdleAction::Started => {
                    debug!(runner_name = %runner_name, job_id = %job_id, "Runner's job is running");
                    self.idle.lock().unwrap().started.insert(runner_name);
                }
                IdleAction::Wait => {
                    if let Err(e) = status {
                        debug!(
                            runner_name = %runner_name,
                            job_id = %job_id,
                            "Idle runner check: couldn't get job status: {e:#}"
                        );
                    }
                }
                IdleAction::TearDown => {
                    let job_state = match &status {
                        Ok(Some(_)) => "still waiting",
                        _ => "gone (finished or cancelled)",
                    };
                    let delay = {
                        let mut idle = self.idle.lock().unwrap();
                        idle.tearing_down
                            .insert(runner_name.clone(), std::time::Instant::now());
                        let delay =
                            next_idle_backoff(idle.backoff.get(&job_id).map(|b| b.delay), timeout);
                        idle.backoff.insert(
                            job_id.clone(),
                            IdleBackoff {
                                until: std::time::Instant::now() + delay,
                                delay,
                            },
                        );
                        delay
                    };
                    warn!(
                        agent_id = %info.agent_id,
                        runner_name = %runner_name,
                        job_id = %job_id,
                        idle_secs = age.as_secs(),
                        retry_in_secs = delay.as_secs(),
                        "Runner's job is {job_state} after the idle timeout - destroying its VM"
                    );
                    self.send_destroy(&info.agent_id, &runner_name).await;
                }
            }
        }

        // drop backoff for jobs that left the queue
        if let Ok(pending) = self.pending_job_store.get_all_pending_jobs().await {
            let keys: HashSet<&JobKey> = pending.iter().map(|(k, _)| k).collect();
            self.idle
                .lock()
                .unwrap()
                .backoff
                .retain(|k, _| keys.contains(k));
        }
    }

    /// Ask an agent to destroy a runner's VM. If it doesn't happen, the idle check
    /// sends it again after [`IDLE_DESTROY_RETRY`].
    async fn send_destroy(&self, agent_id: &str, runner_name: &str) {
        let command_id = Uuid::new_v4().to_string();
        let msg = CoordinatorMessage {
            payload: Some(CoordinatorPayload::DestroyRunner(DestroyRunnerCommand {
                command_id: command_id.clone(),
                vm_id: runner_name.to_string(),
            })),
        };
        match self
            .agent_registry
            .send_command(agent_id, msg, &command_id, Duration::from_secs(30))
            .await
        {
            Ok(response) => match response.payload {
                Some(AgentPayload::Ack(ack)) if ack.accepted => {
                    info!(agent_id = %agent_id, runner_name = %runner_name, "DestroyRunner accepted");
                }
                Some(AgentPayload::Ack(ack)) => {
                    warn!(
                        agent_id = %agent_id,
                        runner_name = %runner_name,
                        "DestroyRunner rejected: {}",
                        ack.error
                    );
                }
                other => {
                    warn!(
                        agent_id = %agent_id,
                        runner_name = %runner_name,
                        "Unexpected response to DestroyRunner: {other:?}"
                    );
                }
            },
            Err(e) => {
                warn!(
                    agent_id = %agent_id,
                    runner_name = %runner_name,
                    "DestroyRunner failed: {e:#}"
                );
            }
        }
    }

    /// Whether a pending job is still waiting out its delay after an idle runner.
    fn job_backing_off(&self, job_id: &JobKey) -> bool {
        self.idle
            .lock()
            .unwrap()
            .backoff
            .get(job_id)
            .is_some_and(|b| b.until > std::time::Instant::now())
    }

    /// Handle a webhook event (new job notification or job completion).
    async fn handle_webhook_event(&self, event: WebhookEvent) {
        match event {
            WebhookEvent::NewJobAvailable => {
                // New job(s) persisted to DB by webhook handler - process them
                debug!("Received NewJobAvailable notification - processing pending jobs");
                self.process_pending_jobs().await;
            }
            WebhookEvent::JobCompleted { job_id } => {
                // Remove from pending queue if it was waiting
                self.pending_job_store.remove_job(&job_id).await;
                debug!("Cleaned up state for completed job {}", job_id);

                // Job completed means an agent may be free - try pending jobs
                self.process_pending_jobs().await;
            }
        }
    }

    /// Verify pending jobs are still queued on their forge.
    ///
    /// Jobs may have been cancelled, picked up by another runner, or expired.
    /// This method queries the forge for each pending job's status and removes
    /// any that are no longer queued.
    async fn verify_pending_jobs(&self) {
        let pending = match self.pending_job_store.get_all_pending_jobs().await {
            Ok(jobs) => jobs,
            Err(e) => {
                warn!("Failed to get pending jobs for forge verification: {}", e);
                return;
            }
        };

        if pending.is_empty() {
            debug!("No pending jobs to verify with forges");
            return;
        }

        info!(
            "Verifying {} pending job(s) are still queued on their forge",
            pending.len()
        );

        for (job_id, info) in pending {
            self.verify_pending_job(&job_id, &info).await;
        }
    }

    /// Scan forges for queued jobs, as the active on-demand mode needs.
    ///
    /// - **Webhook**: verify every pending job is still queued, then recover missed jobs.
    /// - **Polling**: the poll. Add newly queued jobs (applying label mappings like the
    ///   webhook handler does), then verify only the pending jobs the scan no longer saw.
    async fn scan_forges(&self) {
        if self.config.provisioning.mode == ProvisioningMode::Polling {
            if let Some(scan) = self.recover_queued_jobs(true).await {
                self.verify_dropped_pending_jobs(&scan).await;
            }
        } else {
            self.verify_pending_jobs().await;
            self.recover_queued_jobs(false).await;
        }
    }

    /// Verify pending jobs that a successful poll didn't list as queued.
    ///
    /// A job missing from the scan was probably cancelled or started, but the scan
    /// skips repos whose API calls fail and repos outside the lookback window, so a
    /// missing job is checked with its forge before it's dropped. Jobs on forges the
    /// scan couldn't list are left alone.
    async fn verify_dropped_pending_jobs(&self, scan: &QueueScan) {
        let pending = match self.pending_job_store.get_all_pending_jobs().await {
            Ok(jobs) => jobs,
            Err(e) => {
                warn!("Failed to get pending jobs for forge verification: {}", e);
                return;
            }
        };
        let dropped = dropped_pending_jobs(pending, scan);
        if dropped.is_empty() {
            return;
        }
        info!(
            "{} pending job(s) no longer listed as queued - verifying with their forge",
            dropped.len()
        );
        for (job_id, info) in dropped {
            self.verify_pending_job(&job_id, &info).await;
        }
    }

    /// Check one pending job's status on its forge and drop it from the queue unless
    /// it's still queued (or the forge couldn't be asked).
    async fn verify_pending_job(&self, job_id: &JobKey, info: &PendingJobInfo) {
        // Age threshold for jobs without repository info (can't check forge status)
        let max_age_without_repo = chrono::Duration::hours(24);
        let now = chrono::Utc::now();
        // Jobs without repository info can't be checked on their forge
        // Remove them if they're older than the threshold
        let Some(ref repo) = info.repository else {
            let age = now - info.created_at;
            let age_hours = age.num_hours();
            if age > max_age_without_repo {
                warn!(
                    "Pending job {} has no repository info and is {}h old - removing as stale",
                    job_id, age_hours
                );
                self.pending_job_store.remove_job(job_id).await;
            } else {
                debug!(
                    "Skipping job {} - no repository info for forge status check ({}h old)",
                    job_id, age_hours
                );
            }
            return;
        };

        let forge = match self.forges.for_job(job_id) {
            Ok(forge) => forge,
            Err(e) => {
                warn!("Pending job {}: {} - removing from queue", job_id, e);
                self.pending_job_store.remove_job(job_id).await;
                return;
            }
        };

        match forge.get_job_status(repo, job_id.id()).await {
            Ok(Some(status)) if status == "queued" || status == "waiting" => {
                // Job is still pending on its forge - keep it
                debug!("Job {} is still {} on its forge", job_id, status);
            }
            Ok(Some(status)) => {
                // Job has progressed or completed - remove from our queue
                info!(
                    "Pending job {} has status '{}' on its forge - removing from queue",
                    job_id, status
                );
                self.pending_job_store.remove_job(job_id).await;
            }
            Ok(None) => {
                // Job not found (404) - probably deleted or expired
                info!(
                    "Pending job {} not found on its forge - removing from queue",
                    job_id
                );
                self.pending_job_store.remove_job(job_id).await;
            }
            Err(e) => {
                // API error - log but don't remove (could be transient)
                warn!(
                    "Failed to check status for job {} on its forge: {} - keeping in queue",
                    job_id, e
                );
            }
        }
    }

    /// Add queued jobs found by scanning the forges' APIs to the pending queue.
    ///
    /// In webhook mode this recovers jobs queued while the coordinator was down or
    /// whose webhooks were lost. In polling mode it's the poll itself (`polling`
    /// only quiets the per-scan logging). Either way, jobs go through the label
    /// mappings exactly as the webhook handler does, so a scan never accepts a job
    /// a webhook would have ignored.
    ///
    /// Returns which forges were listed and every job they listed as queued, or
    /// `None` if the scan didn't run or no forge could be listed.
    async fn recover_queued_jobs(&self, polling: bool) -> Option<QueueScan> {
        use crate::webhook::{WebhookRunnerRequest, has_required_labels, resolve_runner_scope};

        let Some(filters) = self.config.provisioning.on_demand_filters() else {
            debug!("No webhook config - skipping forge job recovery");
            return None;
        };
        let required_labels = filters.required_labels;

        if polling {
            debug!("Polling forges for queued jobs...");
        } else {
            info!("Polling forges for queued jobs that may have been missed...");
        }

        // Query every forge for queued jobs; a forge that fails is left out of this scan
        let mut scan = QueueScan::default();
        let mut queued_jobs = Vec::new();
        for (name, forge) in self.forges.iter() {
            match forge.list_queued_jobs().await {
                Ok(jobs) => {
                    scan.forges.insert(name.to_string());
                    for job in jobs {
                        let key = JobKey::new(name, job.id.clone());
                        scan.seen.insert(key.clone());
                        queued_jobs.push((key, job));
                    }
                }
                Err(e) => {
                    warn!("Failed to list queued jobs from forge '{}': {}", name, e);
                }
            }
        }
        if scan.forges.is_empty() {
            return None;
        }

        if queued_jobs.is_empty() {
            if polling {
                debug!("No queued jobs found on any forge");
            } else {
                info!("No queued jobs found on any forge");
            }
            return Some(scan);
        }

        let total_jobs = queued_jobs.len();
        info!(
            "Found {} queued job(s) across forges - checking against filters",
            total_jobs
        );

        let mut added_count = 0;
        for (job_id, job) in queued_jobs {
            // Check if job matches required_labels
            if !has_required_labels(&job.labels, &required_labels) {
                debug!(
                    "Job {} doesn't match required labels {:?} - skipping",
                    job_id, required_labels
                );
                continue;
            }

            // Check if we already have this job in pending queue
            if self.pending_job_store.has_job(&job_id).await {
                debug!("Job {} already in pending queue - skipping", job_id);
                continue;
            }

            // Check if we already have a runner for this job
            let existing = self.runner_state.runners_for_job(&job_id).await;
            if !existing.is_empty() {
                if existing.len() > 1 {
                    error!(
                        job_id = %job_id,
                        runners = ?existing,
                        "Multiple runners exist for the same job — state corruption"
                    );
                }
                info!(
                    job_id = %job_id,
                    runners = ?existing,
                    "Job already has active runner - skipping forge recovery"
                );
                continue;
            }

            // Accept only jobs a label mapping accepts, as the webhook handler would
            let scan_scope = job.runner_scope;
            let Some((runner_scope, runner_group)) =
                resolve_runner_scope(&job_id, &job.labels, &filters.label_mappings, || scan_scope)
            else {
                continue;
            };

            // Add to pending queue
            let request = WebhookRunnerRequest {
                job_id: job_id.clone(),
                job_labels: job.labels.clone(),
                agent_labels: job.labels.clone(), // Use job labels for agent matching
                runner_scope,
                runner_group,
                repository: job.repository,
                job_name: None,      // Not available from list_queued_jobs
                workflow_name: None, // Not available from list_queued_jobs
            };

            if self.pending_job_store.add_job(&request).await {
                if polling {
                    info!("Queued job {} found (labels: {:?})", job_id, job.labels);
                } else {
                    info!("Recovered queued job {} (labels: {:?})", job_id, job.labels);
                }
                added_count += 1;
            }
        }

        if added_count > 0 {
            info!(
                "Recovered {} job(s) from forge APIs - processing",
                added_count
            );
            self.process_pending_jobs().await;
        } else {
            debug!(
                "All {} queued job(s) already in pending queue, have runners, or were filtered out - no new jobs to add",
                total_jobs
            );
        }
        Some(scan)
    }

    /// Process pending jobs when capacity becomes available.
    async fn process_pending_jobs(&self) {
        use crate::pending_jobs::MAX_RETRIES;

        // Get all pending jobs from DB (ordered by created_at, FIFO)
        let pending_jobs = match self.pending_job_store.get_all_pending_jobs().await {
            Ok(jobs) => jobs,
            Err(e) => {
                error!("Failed to get pending jobs: {}", e);
                return;
            }
        };

        if pending_jobs.is_empty() {
            debug!("No pending jobs to process");
            return;
        }

        info!("Processing {} pending job(s)", pending_jobs.len());

        for (job_id, job_info) in pending_jobs {
            // Check retry count - abandon jobs that have failed too many times
            if job_info.retry_count >= MAX_RETRIES {
                warn!(
                    "Pending job {} has exceeded max retries ({}) - abandoning",
                    job_id, MAX_RETRIES
                );
                self.pending_job_store.remove_job(&job_id).await;
                continue;
            }

            // Skip jobs whose last runner idled until their delay is up
            if self.job_backing_off(&job_id) {
                debug!(
                    "Pending job {} is backing off after an idle runner - skipping",
                    job_id
                );
                continue;
            }

            // Skip if a runner already exists for this job (it's already being processed)
            let existing = self.runner_state.runners_for_job(&job_id).await;
            if !existing.is_empty() {
                if existing.len() > 1 {
                    error!(
                        job_id = %job_id,
                        runners = ?existing,
                        "Multiple runners exist for the same job — state corruption"
                    );
                }
                info!(
                    job_id = %job_id,
                    runners = ?existing,
                    "Pending job already has active runner - skipping"
                );
                continue;
            }

            // Try to create a runner for this job
            match self
                .create_runner_on_demand(
                    &job_info.agent_labels,
                    &job_info.failed_agents,
                    &job_info.runner_scope,
                    job_info
                        .runner_group
                        .as_deref()
                        .and_then(|g| g.parse::<u64>().ok())
                        .unwrap_or(1),
                    &job_id,
                    job_info.job_name.as_deref(),
                    job_info.repository.as_deref(),
                    job_info.workflow_name.as_deref(),
                )
                .await
            {
                Ok(()) => {
                    // Runner creation initiated - job stays in DB until runner completes/fails
                    // The runners_for_job check will skip it on subsequent iterations
                    info!("Created runner for pending job {}", job_id);
                }
                Err(e) => {
                    let err_msg = e.to_string();
                    if err_msg.contains("No available agent") {
                        // No agent matches this job's labels - try other jobs
                        // (different jobs may have different label requirements)
                        info!(
                            "No available agent for job {} with labels {:?} - trying next job",
                            job_id, job_info.agent_labels
                        );
                        continue;
                    } else {
                        // Different error (token fetch failed, etc.) - increment retry count
                        if let Some(new_count) = self
                            .pending_job_store
                            .increment_retry_count(&job_id, None)
                            .await
                        {
                            if new_count >= MAX_RETRIES {
                                warn!(
                                    "Job {} failed to create runner and exceeded max retries: {}",
                                    job_id, e
                                );
                                self.pending_job_store.remove_job(&job_id).await;
                            } else {
                                warn!(
                                    "Job {} failed to create runner (attempt {}/{}): {}",
                                    job_id, new_count, MAX_RETRIES, e
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    /// Create a single runner on-demand (for webhook mode).
    ///
    /// Unlike fixed-capacity reconciliation, this doesn't track pending counts
    /// since there's no target to maintain.
    #[allow(clippy::too_many_arguments)]
    async fn create_runner_on_demand(
        &self,
        labels: &[String],
        avoid_agents: &[String],
        runner_scope: &RunnerScope,
        runner_group_id: u64,
        job_id: &JobKey,
        job_name: Option<&str>,
        repository: Option<&str>,
        workflow_name: Option<&str>,
    ) -> anyhow::Result<()> {
        let forge = self.forges.for_job(job_id)?;

        // Find the least loaded agent with matching labels, steering away from ones that already
        // failed this job
        let kind = forge.runner_kind();
        let agent_id = self
            .agent_registry
            .select_agent_for_kind(labels, avoid_agents, kind)
            .await
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "No available agent for labels {labels:?} that runs {} runners",
                    kind.as_str()
                )
            })?;

        info!(
            "Found available agent {} for job {} with labels {:?}",
            agent_id, job_id, labels
        );

        // Generate runner name and command ID, the reservation is per runner
        let runner_name = self.new_runner_name(&agent_id).await;
        let command_id = Uuid::new_v4().to_string();

        // Reserve a slot on the agent
        if !self
            .agent_registry
            .reserve_slot(&agent_id, labels, "", &runner_name)
            .await
        {
            anyhow::bail!("Failed to reserve slot on agent {agent_id} (might be at capacity)");
        }

        // Verify the job is still queued before committing resources
        if let Some(repo) = repository {
            match forge.get_job_status(repo, job_id.id()).await {
                Ok(Some(status)) if status != "queued" => {
                    info!(
                        "Job {} is no longer queued (status: {}) — skipping runner creation",
                        job_id, status
                    );
                    self.agent_registry
                        .release_slot(&agent_id, &runner_name)
                        .await;
                    self.pending_job_store.remove_job(job_id).await;
                    return Ok(());
                }
                Ok(None) => {
                    info!(
                        "Job {} not found on its forge — removing from pending queue",
                        job_id
                    );
                    self.agent_registry
                        .release_slot(&agent_id, &runner_name)
                        .await;
                    self.pending_job_store.remove_job(job_id).await;
                    return Ok(());
                }
                Err(e) => {
                    // Don't block on status check failure — proceed with runner creation
                    warn!(
                        "Failed to check job {} status: {} — proceeding anyway",
                        job_id, e
                    );
                }
                _ => {} // Still queued, continue
            }
        }

        // Register a single-use runner: a GitHub JIT config, or a Forgejo runner for this job attempt
        let launch = match forge
            .create_job_runner(
                runner_scope,
                &runner_name,
                labels,
                runner_group_id,
                job_id.id(),
            )
            .await
        {
            Ok(launch) => launch,
            Err(e) => {
                // Release the reserved slot since we won't use it
                self.agent_registry
                    .release_slot(&agent_id, &runner_name)
                    .await;
                return Err(e);
            }
        };

        // Save runner state for crash recovery (include job_id for dedup cleanup on failure)
        self.runner_state
            .add_runner(
                runner_name.clone(),
                agent_id.clone(),
                runner_name.clone(), // vm_name is same as runner_name
                runner_scope.clone(),
                job_id.forge(),
                Some(job_id.clone()),
                job_name.map(String::from),
                repository.map(String::from),
                workflow_name.map(String::from),
                None, // pool
            )
            .await;

        // Build CreateRunner command with the JIT config or Forgejo runner
        let (jit_config, forgejo) = match launch {
            RunnerLaunch::GitHubJit(jit_config) => (jit_config, None),
            RunnerLaunch::Forgejo(f) => (
                String::new(),
                Some(Box::new(ForgejoRunner {
                    instance_url: f.instance_url,
                    uuid: f.uuid,
                    token: f.token,
                    handle: f.handle,
                })),
            ),
        };
        let create_cmd = CreateRunnerCommand {
            command_id: command_id.clone(),
            vm_name: runner_name.clone(),
            registration_token: String::new(), // Not needed for JIT
            labels: labels.to_vec(),
            runner_scope_url: forge.scope_url(runner_scope),
            jit_config,
            label_set_id: String::new(), // the agent picks by job labels
            forgejo,
        };

        let coordinator_msg = CoordinatorMessage {
            payload: Some(CoordinatorPayload::CreateRunner(create_cmd)),
        };

        info!(
            "Sending CreateRunner to agent {} for {} (job {})",
            agent_id, runner_name, job_id
        );

        // Clone for async task
        let agent_registry = self.agent_registry.clone();
        let forge = forge.clone();
        let job_id = job_id.clone();
        let pending_job_store = self.pending_job_store.clone();
        let runner_scope = runner_scope.clone();
        let runner_name_clone = runner_name.clone();
        let runner_state = self.runner_state.clone();
        let agent_id_clone = agent_id.clone();
        let notify_tx = self.notify_tx.clone();
        let repository_clone = repository.map(String::from);
        let failures = self.failures.clone();

        // Helper to handle retry counting on command-level failures
        // (agent rejection, timeout, unexpected response)
        async fn handle_command_failure(
            pending_job_store: &crate::pending_jobs::PendingJobStore,
            job_id: &JobKey,
            agent_id: &str,
            error_context: &str,
        ) {
            use crate::pending_jobs::MAX_RETRIES;

            if let Some(new_count) = pending_job_store
                .increment_retry_count(job_id, Some(agent_id))
                .await
            {
                if new_count >= MAX_RETRIES {
                    warn!(
                        job_id = %job_id,
                        "Job exceeded max retries ({}) after {error_context} - abandoning",
                        MAX_RETRIES
                    );
                    pending_job_store.remove_job(job_id).await;
                } else {
                    warn!(
                        job_id = %job_id,
                        retry_count = %new_count,
                        max_retries = %MAX_RETRIES,
                        "Job will be retried after {error_context}"
                    );
                }
            }
        }

        // Spawn task to send command and handle response
        tokio::spawn(async move {
            let timeout = Duration::from_secs(30);

            match agent_registry
                .send_command(&agent_id_clone, coordinator_msg, &command_id, timeout)
                .await
            {
                Ok(response) => match response.payload {
                    Some(AgentPayload::Ack(ack)) => {
                        if ack.accepted {
                            info!(
                                "CreateRunner accepted by agent {} for {} (webhook)",
                                agent_id_clone, runner_name_clone
                            );
                        } else {
                            warn!(
                                "CreateRunner rejected by agent {} for {}: {}",
                                agent_id_clone, runner_name_clone, ack.error
                            );
                            failures
                                .record(
                                    &agent_id_clone,
                                    FailureKind::CommandRejected,
                                    Some(&runner_name_clone),
                                    Some(&job_id),
                                    &ack.error,
                                )
                                .await;
                            agent_registry
                                .release_slot(&agent_id_clone, &runner_name_clone)
                                .await;
                            if let Err(e) =
                                forge.remove_runner(&runner_scope, &runner_name_clone).await
                            {
                                warn!(
                                    "Could not remove runner '{}' from its forge: {}",
                                    runner_name_clone, e
                                );
                            }
                            runner_state.remove_runner(&runner_name_clone).await;
                            // Don't count agent rejections (e.g. capacity exceeded) as
                            // job failures — the job is fine, this agent just can't
                            // take it right now. Notify the fleet to reprocess pending
                            // jobs so another agent can pick it up.
                            let _ = notify_tx.try_send(());
                        }
                    }
                    Some(AgentPayload::Result(result)) => {
                        // Legacy agents may respond with a full lifecycle result
                        agent_registry
                            .release_slot(&agent_id_clone, &runner_name_clone)
                            .await;
                        let mut reprovision = false;
                        if result.success {
                            match check_job_after_runner(
                                forge.as_ref(),
                                repository_clone.as_deref(),
                                &job_id,
                            )
                            .await
                            {
                                JobCheck::Queued => {
                                    warn!(
                                        "Runner {} completed but job {} is still queued on its forge - it ran another job, re-provisioning",
                                        runner_name_clone, job_id
                                    );
                                    reprovision = true;
                                }
                                JobCheck::Unknown => {
                                    info!(
                                        "Runner {} completed but job {} status is unverified - keeping it pending",
                                        runner_name_clone, job_id
                                    );
                                    reprovision = true;
                                }
                                JobCheck::Done => {
                                    info!(
                                        "Runner {} completed successfully (webhook)",
                                        runner_name_clone
                                    );
                                    pending_job_store.remove_job(&job_id).await;
                                }
                            }
                        } else {
                            warn!("Runner {} failed: {}", runner_name_clone, result.error);
                            failures
                                .record(
                                    &agent_id_clone,
                                    FailureKind::RunnerFailed,
                                    Some(&runner_name_clone),
                                    Some(&job_id),
                                    &result.error,
                                )
                                .await;
                            handle_command_failure(
                                &pending_job_store,
                                &job_id,
                                &agent_id_clone,
                                &format!("runner failure: {}", result.error),
                            )
                            .await;
                        }
                        if let Err(e) = forge.remove_runner(&runner_scope, &runner_name_clone).await
                        {
                            warn!(
                                "Could not remove runner '{}' from its forge: {}",
                                runner_name_clone, e
                            );
                        }
                        runner_state.remove_runner(&runner_name_clone).await;
                        if reprovision {
                            // only after the runner record is gone, or the pass this wakes finds
                            // it via runners_for_job and skips the job
                            let _ = notify_tx.try_send(());
                        }
                    }
                    other => {
                        warn!(
                            "Unexpected CreateRunner response from agent {} for {}: {:?}",
                            agent_id_clone, runner_name_clone, other
                        );
                        failures
                            .record(
                                &agent_id_clone,
                                FailureKind::CommandFailed,
                                Some(&runner_name_clone),
                                Some(&job_id),
                                &format!("unexpected response to CreateRunner: {other:?}"),
                            )
                            .await;
                        agent_registry
                            .release_slot(&agent_id_clone, &runner_name_clone)
                            .await;
                        if let Err(e) = forge.remove_runner(&runner_scope, &runner_name_clone).await
                        {
                            warn!(
                                "Could not remove runner '{}' from its forge: {}",
                                runner_name_clone, e
                            );
                        }
                        runner_state.remove_runner(&runner_name_clone).await;
                        handle_command_failure(
                            &pending_job_store,
                            &job_id,
                            &agent_id_clone,
                            "unexpected agent response",
                        )
                        .await;
                    }
                },
                Err(e) => {
                    error!("CreateRunner command failed (webhook): {}", e);
                    failures
                        .record(
                            &agent_id_clone,
                            FailureKind::CommandFailed,
                            Some(&runner_name_clone),
                            Some(&job_id),
                            &format!("CreateRunner: {e:#}"),
                        )
                        .await;
                    agent_registry
                        .release_slot(&agent_id_clone, &runner_name_clone)
                        .await;
                    if let Err(e_inner) =
                        forge.remove_runner(&runner_scope, &runner_name_clone).await
                    {
                        warn!(
                            "Could not remove runner '{}' from its forge: {}",
                            runner_name_clone, e_inner
                        );
                    }
                    runner_state.remove_runner(&runner_name_clone).await;
                    handle_command_failure(
                        &pending_job_store,
                        &job_id,
                        &agent_id_clone,
                        &format!("command error: {e}"),
                    )
                    .await;
                }
            }
        });

        Ok(())
    }

    /// Handle recovery when an agent reconnects with existing VMs.
    ///
    /// For each VM that matches a persisted runner, spawn a watcher task to
    /// monitor the runner's completion.
    async fn handle_agent_recovery(&self, recovery_info: AgentRecoveryInfo) {
        let persisted_runners = self
            .runner_state
            .get_runners_for_agent(&recovery_info.agent_id)
            .await;

        if persisted_runners.is_empty() {
            info!(
                "Agent '{}' has {} VMs but no persisted runners to recover",
                recovery_info.agent_id,
                recovery_info.vm_names.len()
            );
            return;
        }

        info!(
            "Checking {} VMs from agent '{}' against {} persisted runners",
            recovery_info.vm_names.len(),
            recovery_info.agent_id,
            persisted_runners.len()
        );

        for (runner_name, runner_info) in persisted_runners {
            if recovery_info.vm_names.contains(&runner_name) {
                info!(
                    "Recovering runner '{}' - VM still active on agent '{}', spawning watcher",
                    runner_name, recovery_info.agent_id
                );

                // Note: We don't track pending counts for recovered runners since pools
                // are now dynamically derived from agents. The reconciliation loop will
                // naturally avoid over-provisioning based on agent capacity.

                // Spawn a watcher task for this runner
                self.spawn_runner_watcher(
                    runner_name.clone(),
                    recovery_info.agent_id.clone(),
                    runner_info.runner_scope,
                    self.runner_forge(runner_info.forge.as_deref()),
                );
            } else {
                // VM is gone - the runner completed or failed while coordinator was down
                warn!(
                    "Persisted runner '{}' not found on agent '{}' - cleaning up from its forge",
                    runner_name, recovery_info.agent_id
                );

                // Clean up from the forge
                let token_provider = self.runner_forge(runner_info.forge.as_deref());
                let runner_scope = runner_info.runner_scope.clone();
                let runner_name_clone = runner_name.clone();
                let runner_state = self.runner_state.clone();

                tokio::spawn(async move {
                    if let Err(e) = token_provider
                        .remove_runner(&runner_scope, &runner_name_clone)
                        .await
                    {
                        warn!(
                            "Could not remove stale runner '{}' from its forge: {}",
                            runner_name_clone, e
                        );
                    }
                    runner_state.remove_runner(&runner_name_clone).await;
                    info!("Cleaned up stale runner '{}'", runner_name_clone);
                });
            }
        }
    }

    /// Handle a runner lifecycle event emitted by an agent.
    async fn handle_runner_event(&self, event: AgentRunnerEvent) {
        let runner_name = if event.event.runner_name.is_empty() {
            event.event.vm_id.clone()
        } else {
            event.event.runner_name.clone()
        };

        if runner_name.is_empty() {
            warn!(
                agent_id = %event.agent_id,
                "Runner event missing runner_name and vm_id"
            );
            return;
        }

        let event_type = RunnerEventType::try_from(event.event.event_type)
            .unwrap_or(RunnerEventType::Unspecified);

        match event_type {
            RunnerEventType::Unspecified => {
                warn!(
                    agent_id = %event.agent_id,
                    runner_name = %runner_name,
                    "Runner event has unspecified type"
                );
            }
            RunnerEventType::Started => {
                info!(
                    agent_id = %event.agent_id,
                    runner_name = %runner_name,
                    "Runner started"
                );
            }
            RunnerEventType::Completed | RunnerEventType::Failed | RunnerEventType::Destroyed => {
                let runner_info = self.runner_state.get_runner(&runner_name).await;

                // We destroyed this one for idling: not a failure, and its job keeps its backoff
                let (idled, late) = {
                    let mut idle = self.idle.lock().unwrap();
                    idle.started.remove(&runner_name);
                    let idled = idle.tearing_down.remove(&runner_name).is_some();
                    if idled {
                        idle.torn_down
                            .insert(runner_name.clone(), std::time::Instant::now());
                    }
                    (idled, !idled && idle.torn_down.contains_key(&runner_name))
                };
                if late {
                    debug!(
                        agent_id = %event.agent_id,
                        runner_name = %runner_name,
                        "Late event for a torn-down idle runner - ignoring"
                    );
                    return;
                }
                if idled {
                    if let Some(runner_info) = runner_info {
                        self.agent_registry
                            .release_slot(&event.agent_id, &runner_name)
                            .await;
                        self.runner_state.remove_runner(&runner_name).await;
                        info!(
                            agent_id = %event.agent_id,
                            runner_name = %runner_name,
                            job_id = ?runner_info.job_id,
                            "Idle runner torn down"
                        );
                        let forge = self.runner_forge(runner_info.forge.as_deref());
                        let scope = runner_info.runner_scope.clone();
                        let name = runner_name.clone();
                        tokio::spawn(async move {
                            if let Err(e) = forge.remove_runner(&scope, &name).await {
                                warn!(
                                    "Could not remove idle runner '{}' from its forge: {}",
                                    name, e
                                );
                            }
                        });
                    }
                    self.process_pending_jobs().await;
                    return;
                }

                // record before the early return below, a failed destroy arrives after the runner is untracked
                if event_type == RunnerEventType::Failed {
                    self.failures
                        .record(
                            &event.agent_id,
                            FailureKind::RunnerFailed,
                            Some(&runner_name),
                            runner_info.as_ref().and_then(|r| r.job_id.as_ref()),
                            &event.event.error,
                        )
                        .await;
                }
                let Some(runner_info) = runner_info else {
                    // Runner already removed from state (e.g., by reconciliation).
                    // Reconciliation already released the slot, so don't double-release.
                    // Still try to process pending jobs in case this frees something.
                    self.process_pending_jobs().await;
                    debug!(
                        agent_id = %event.agent_id,
                        runner_name = %runner_name,
                        "Runner event for runner already removed from state (reconciliation handled cleanup)"
                    );
                    return;
                };

                // Runner still in state - we're the first to handle cleanup.
                // Release the reserved slot on the agent.
                self.agent_registry
                    .release_slot(&event.agent_id, &runner_name)
                    .await;

                // a plain destroy of a fixed-capacity runner is normal, anything else means it died mid-job
                if event_type == RunnerEventType::Destroyed
                    && (runner_info.job_id.is_some() || !event.event.error.is_empty())
                {
                    let message = if event.event.error.is_empty() {
                        "runner destroyed before finishing"
                    } else {
                        event.event.error.as_str()
                    };
                    self.failures
                        .record(
                            &event.agent_id,
                            FailureKind::RunnerLost,
                            Some(&runner_name),
                            runner_info.job_id.as_ref(),
                            message,
                        )
                        .await;
                }

                // Handle pending job state based on runner outcome
                if let Some(job_id) = runner_info.job_id.clone() {
                    use crate::pending_jobs::MAX_RETRIES;

                    match event_type {
                        RunnerEventType::Completed => {
                            let check = match self.forges.for_job(&job_id) {
                                Ok(forge) => {
                                    check_job_after_runner(
                                        forge.as_ref(),
                                        runner_info.repository.as_deref(),
                                        &job_id,
                                    )
                                    .await
                                }
                                Err(e) => {
                                    // the forge is no longer configured, nobody can run the job here
                                    warn!(job_id = %job_id, "Can't verify job after runner completion: {e}");
                                    JobCheck::Done
                                }
                            };
                            match check {
                                JobCheck::Queued => {
                                    // the runner ran some sibling job; leave ours pending so the
                                    // reprocess below makes it a fresh runner
                                    warn!(
                                        agent_id = %event.agent_id,
                                        runner_name = %runner_name,
                                        job_id = %job_id,
                                        "Runner completed but its job is still queued on its forge - it ran another job, re-provisioning"
                                    );
                                }
                                JobCheck::Unknown => {
                                    // deleting on a guess is the one irreversible move here, so
                                    // don't. worst case is one spare runner for a job that did run
                                    info!(
                                        agent_id = %event.agent_id,
                                        runner_name = %runner_name,
                                        job_id = %job_id,
                                        "Runner completed but job status is unverified - keeping it pending"
                                    );
                                }
                                JobCheck::Done => {
                                    self.pending_job_store.remove_job(&job_id).await;
                                    info!(
                                        agent_id = %event.agent_id,
                                        runner_name = %runner_name,
                                        job_id = %job_id,
                                        "Runner completed successfully"
                                    );
                                }
                            }
                        }
                        RunnerEventType::Failed | RunnerEventType::Destroyed => {
                            // Failure - increment retry count for potential retry
                            if let Some(new_count) = self
                                .pending_job_store
                                .increment_retry_count(&job_id, Some(&event.agent_id))
                                .await
                            {
                                if new_count >= MAX_RETRIES {
                                    warn!(
                                        agent_id = %event.agent_id,
                                        runner_name = %runner_name,
                                        job_id = %job_id,
                                        error = %event.event.error,
                                        "Runner failed and job exceeded max retries ({}) - abandoning",
                                        MAX_RETRIES
                                    );
                                    self.pending_job_store.remove_job(&job_id).await;
                                } else {
                                    warn!(
                                        agent_id = %event.agent_id,
                                        runner_name = %runner_name,
                                        job_id = %job_id,
                                        error = %event.event.error,
                                        retry_count = %new_count,
                                        max_retries = %MAX_RETRIES,
                                        "Runner failed - job will be retried"
                                    );
                                }
                            } else {
                                // Job not in pending store (already completed/removed externally)
                                warn!(
                                    agent_id = %event.agent_id,
                                    runner_name = %runner_name,
                                    job_id = %job_id,
                                    error = %event.event.error,
                                    "Runner failed (job no longer in pending queue)"
                                );
                            }
                        }
                        _ => {}
                    }
                } else {
                    // No job_id means this is a fixed-capacity runner, not webhook-triggered
                    match event_type {
                        RunnerEventType::Failed => {
                            warn!(
                                agent_id = %event.agent_id,
                                runner_name = %runner_name,
                                error = %event.event.error,
                                "Runner failed"
                            );
                        }
                        RunnerEventType::Destroyed => {
                            info!(
                                agent_id = %event.agent_id,
                                runner_name = %runner_name,
                                "Runner destroyed"
                            );
                        }
                        _ => {
                            info!(
                                agent_id = %event.agent_id,
                                runner_name = %runner_name,
                                "Runner completed"
                            );
                        }
                    }
                }

                self.runner_state.remove_runner(&runner_name).await;

                let token_provider = self.runner_forge(runner_info.forge.as_deref());
                let runner_scope = runner_info.runner_scope.clone();
                let runner_name_clone = runner_name.clone();
                tokio::spawn(async move {
                    if let Err(e) = token_provider
                        .remove_runner(&runner_scope, &runner_name_clone)
                        .await
                    {
                        warn!(
                            "Could not remove runner '{}' from its forge: {}",
                            runner_name_clone, e
                        );
                    }
                });

                // only now: the runner is gone from state and the retry is recorded, so the job is
                // eligible again. doing this earlier just logged "already has active runner" and
                // left the retry to whatever notification came next
                self.process_pending_jobs().await;
            }
        }
    }

    /// Spawn a watcher task for a recovered runner.
    ///
    /// The watcher will poll the agent's VM list until the runner completes,
    /// then clean up from the forge and update state.
    fn spawn_runner_watcher(
        &self,
        runner_name: String,
        agent_id: String,
        runner_scope: crate::config::RunnerScope,
        token_provider: Arc<dyn Forge>,
    ) {
        let agent_registry = self.agent_registry.clone();
        let runner_state = self.runner_state.clone();

        tokio::spawn(async move {
            // Poll until the runner/VM is gone (runner completed its job)
            let poll_interval = Duration::from_secs(30);
            let mut interval = tokio::time::interval(poll_interval);

            info!(
                "Started recovery watcher for runner '{}' on agent '{}'",
                runner_name, agent_id
            );

            loop {
                interval.tick().await;

                // Check if agent is still connected
                let agent = agent_registry.get(&agent_id).await;
                if agent.is_none() {
                    warn!(
                        "Agent '{}' disconnected while watching runner '{}' - stopping watcher",
                        agent_id, runner_name
                    );
                    // Agent disconnected - runner state will be handled when agent reconnects
                    // or by the disconnect cleanup logic
                    break;
                }

                // For now, we can't easily check if the VM is still running without
                // the status messages coming through the normal channel. The runner
                // watcher will be stopped when the agent reports the VM is gone in
                // its status updates.
                //
                // A more sophisticated approach would be to have the agent report
                // VM completions explicitly, but for now we rely on:
                // 1. Agent disconnect cleanup
                // 2. Stale runner cleanup based on age
                //
                // The important thing is that pending_runners count is tracked,
                // so we won't over-provision runners.

                // Check if this runner is still in persisted state
                // (It will be removed when the agent reports completion or disconnects)
                if !runner_state.has_runner(&runner_name).await {
                    info!(
                        "Runner '{}' removed from state - watcher task exiting",
                        runner_name
                    );
                    break;
                }
            }

            // Clean up from the forge
            info!(
                "Recovery watcher for '{}' exiting, cleaning up from its forge",
                runner_name
            );
            if let Err(e) = token_provider
                .remove_runner(&runner_scope, &runner_name)
                .await
            {
                warn!(
                    "Could not remove runner '{}' from its forge: {}",
                    runner_name, e
                );
            }

            // Remove from persistent state
            runner_state.remove_runner(&runner_name).await;
        });
    }

    /// Reconcile the current state with the desired state.
    ///
    /// Derives pools from connected agents and ensures target counts are met.
    async fn reconcile(&self) {
        self.warn_unprovisionable_capabilities().await;

        let agent_count = self.agent_registry.count().await;
        let pool_defs = self.agent_registry.get_pool_definitions().await;

        info!(
            "Fleet manager reconciling: {} pools from {} connected agents",
            pool_defs.len(),
            agent_count
        );

        for pool_def in &pool_defs {
            if let Err(e) = self
                .reconcile_pool(
                    pool_def,
                    &self.config.runner_scope,
                    self.config.runner_group.as_ref(),
                )
                .await
            {
                error!("Failed to reconcile pool for {:?}: {}", pool_def.labels, e);
            }
        }
    }

    /// Warn (once per agent) about capabilities fixed-capacity mode can't pre-create.
    ///
    /// Legacy fixed-capacity pools are derived from each agent's flat `labels` (its base
    /// labels), not its `label_sets`. So an agent advertising mapping-specific
    /// capabilities — e.g. via `vm.template_mappings` — gets runners pre-created
    /// with only its base labels, and jobs requiring a mapped label won't match.
    /// Setting `pool` on the mappings or using webhook provisioning both match on
    /// `label_sets`; surface this so the mismatch isn't silent.
    async fn warn_unprovisionable_capabilities(&self) {
        let agents = self.agent_registry.list_all().await;
        let mut warned = self
            .warned_capability_agents
            .lock()
            .expect("warned_capability_agents mutex poisoned");

        for agent in agents.iter().filter(|a| !a.explicit_pools) {
            let base: std::collections::HashSet<String> =
                agent.labels.iter().map(|l| l.to_lowercase()).collect();
            let mut extra: Vec<String> = agent
                .label_sets
                .iter()
                .flatten()
                .map(|l| l.to_lowercase())
                .filter(|l| !base.contains(l))
                .collect();
            extra.sort();
            extra.dedup();

            if !extra.is_empty() && warned.insert(agent.agent_id.clone()) {
                warn!(
                    agent_id = %agent.agent_id,
                    "Agent advertises capabilities {:?} only via label sets (e.g. \
                     vm.template_mappings), but fixed-capacity provisioning pre-creates runners \
                     from base labels {:?} only — jobs requiring those extra labels won't match \
                     pre-created runners. Set `pool` on the mappings to pre-create runners per \
                     mapping, or use webhook provisioning.",
                    extra, agent.labels
                );
            }
        }
    }

    /// Count current runners (pending + active) for a pool from the runner_state DB. An explicit pool counts its
    /// agent's runners tagged with the pool, a legacy one all runners of the agents with its base labels.
    async fn count_runners_for_pool(&self, pool_def: &PoolDefinition) -> u32 {
        if let Some(agent_id) = &pool_def.agent_id {
            return self
                .runner_state
                .count_runners_for_pool(agent_id, &pool_def.key())
                .await as u32;
        }
        let agents = self.agent_registry.list_all().await;
        let mut total = 0u32;
        for agent in &agents {
            // same members as legacy_pool_capacity and select_legacy_pool_agent
            if !agent.explicit_pools && normalize_labels(&agent.labels) == pool_def.labels {
                total += self
                    .runner_state
                    .count_runners_for_agent(&agent.agent_id)
                    .await as u32;
            }
        }
        total
    }

    /// Reconcile a single runner pool.
    ///
    /// Creates runners up to the target count for this pool, using agents that
    /// match the required labels.
    async fn reconcile_pool(
        &self,
        pool_def: &PoolDefinition,
        runner_scope: &RunnerScope,
        _runner_group: Option<&String>,
    ) -> anyhow::Result<()> {
        // Count current runners from the database (source of truth).
        // This replaces the old in-memory pending_runners counter which could
        // diverge from reality when runners completed without decrementing.
        let current = self.count_runners_for_pool(pool_def).await;

        let target = pool_def.target_count;

        info!(
            "Pool {:?}: checking - current={}, target={}",
            pool_def.labels, current, target
        );

        if current >= target {
            info!(
                "Pool {:?}: {}/{} runners (target met, nothing to do)",
                pool_def.labels, current, target
            );
            return Ok(());
        }

        let needed = target - current;

        // Check available capacity before trying to create runners
        let capacity = match &pool_def.agent_id {
            Some(agent_id) => {
                self.agent_registry
                    .agent_capacity(agent_id, &pool_def.labels, &pool_def.label_set_id)
                    .await
            }
            None => {
                self.agent_registry
                    .legacy_pool_capacity(&pool_def.labels)
                    .await
            }
        };

        // Log all agents for debugging
        let all_agents = self.agent_registry.list_all().await;
        info!(
            "Pool {:?}: need {} runners, found {} agents, total capacity={}",
            pool_def.labels,
            needed,
            all_agents.len(),
            capacity
        );
        for agent in &all_agents {
            info!(
                "  Agent {}: labels={:?}, active={}/{}, matches={}",
                agent.agent_id,
                agent.labels,
                agent.active_vms,
                agent.max_vms,
                pool_def
                    .labels
                    .iter()
                    .all(|l| agent.labels.iter().any(|al| al.eq_ignore_ascii_case(l)))
            );
        }

        if capacity == 0 {
            warn!(
                "Pool {:?}: need {} more runners but no agent capacity available",
                pool_def.labels, needed
            );
            return Ok(());
        }

        // Only try to create as many as we have capacity for
        let to_create = std::cmp::min(needed as usize, capacity) as u32;

        info!(
            "Pool {:?}: {}/{} runners, need {} more, capacity for {}, will try to create {}",
            pool_def.labels, current, target, needed, capacity, to_create
        );

        // Try to create runners up to the available capacity
        for i in 0..to_create {
            info!(
                "Pool {:?}: creating runner {}/{}",
                pool_def.labels,
                i + 1,
                to_create
            );

            // Find an available agent with matching labels, an explicit pool always uses its own agent
            let found = match &pool_def.agent_id {
                Some(agent_id) => Some(agent_id.clone()),
                None => {
                    self.agent_registry
                        .select_legacy_pool_agent(&pool_def.labels)
                        .await
                }
            };
            let agent_id = match found {
                Some(id) => {
                    info!("Found available agent: {}", id);
                    id
                }
                None => {
                    warn!(
                        "No available agent for labels {:?} on iteration {}/{}",
                        pool_def.labels,
                        i + 1,
                        to_create
                    );
                    break;
                }
            };

            // Generate runner name and command ID, the reservation is per runner
            let runner_name = self.new_runner_name(&agent_id).await;
            let command_id = Uuid::new_v4().to_string();

            // Reserve a slot on the agent to prevent over-scheduling
            if !self
                .agent_registry
                .reserve_slot(
                    &agent_id,
                    &pool_def.labels,
                    &pool_def.label_set_id,
                    &runner_name,
                )
                .await
            {
                warn!(
                    "Failed to reserve slot on agent {} (might be at capacity now)",
                    agent_id
                );
                continue;
            }
            info!("Reserved slot on agent {}", agent_id);

            // Get registration token from the default forge (GitHub API or mock):
            // fixed-capacity pools register there
            let forge = self.forges.default_forge();
            let token = match forge.get_registration_token(runner_scope).await {
                Ok(t) => t,
                Err(e) => {
                    error!("Failed to get registration token: {}", e);
                    // Release the reserved slot since we won't use it
                    self.agent_registry
                        .release_slot(&agent_id, &runner_name)
                        .await;
                    break;
                }
            };

            // Save runner state for crash recovery (no job_id in fixed capacity mode)
            self.runner_state
                .add_runner(
                    runner_name.clone(),
                    agent_id.clone(),
                    runner_name.clone(), // vm_name is same as runner_name
                    runner_scope.clone(),
                    self.forges.default_name(),
                    None, // job_id
                    None, // job_name
                    None, // repository
                    None, // workflow_name
                    pool_def.agent_id.as_ref().map(|_| pool_def.key()),
                )
                .await;

            // Build CreateRunner command (fixed capacity uses registration token, not JIT)
            let create_cmd = CreateRunnerCommand {
                command_id: command_id.clone(),
                vm_name: runner_name.clone(),
                registration_token: token,
                labels: pool_def.labels.clone(),
                runner_scope_url: forge.scope_url(runner_scope),
                jit_config: String::new(),
                forgejo: None,
                label_set_id: pool_def.label_set_id.clone(),
            };

            let coordinator_msg = CoordinatorMessage {
                payload: Some(CoordinatorPayload::CreateRunner(create_cmd)),
            };

            info!(
                "Sending CreateRunner to agent {} for {} (agent-driven mode)",
                agent_id, runner_name
            );

            // Clone for async task
            let agent_registry = self.agent_registry.clone();
            let agent_id_clone = agent_id.clone();
            let token_provider = forge.clone();
            let runner_scope = runner_scope.clone();
            let runner_name_clone = runner_name.clone();
            let runner_state = self.runner_state.clone();
            let failures = self.failures.clone();

            // Spawn task to send command and handle response
            tokio::spawn(async move {
                let timeout = Duration::from_secs(30);

                match agent_registry
                    .send_command(&agent_id_clone, coordinator_msg, &command_id, timeout)
                    .await
                {
                    Ok(response) => match response.payload {
                        Some(AgentPayload::Ack(ack)) => {
                            if ack.accepted {
                                info!(
                                    "CreateRunner accepted by agent {} for {} (agent-driven)",
                                    agent_id_clone, runner_name_clone
                                );
                            } else {
                                warn!(
                                    "CreateRunner rejected by agent {} for {}: {}",
                                    agent_id_clone, runner_name_clone, ack.error
                                );
                                failures
                                    .record(
                                        &agent_id_clone,
                                        FailureKind::CommandRejected,
                                        Some(&runner_name_clone),
                                        None,
                                        &ack.error,
                                    )
                                    .await;
                                agent_registry
                                    .release_slot(&agent_id_clone, &runner_name_clone)
                                    .await;
                                if let Err(e) = token_provider
                                    .remove_runner(&runner_scope, &runner_name_clone)
                                    .await
                                {
                                    warn!(
                                        "Could not remove runner '{}' from its forge: {}",
                                        runner_name_clone, e
                                    );
                                }
                                runner_state.remove_runner(&runner_name_clone).await;
                            }
                        }
                        Some(AgentPayload::Result(result)) => {
                            agent_registry
                                .release_slot(&agent_id_clone, &runner_name_clone)
                                .await;
                            if result.success {
                                info!("Runner {} completed successfully", runner_name_clone);
                            } else {
                                warn!("Runner {} failed: {}", runner_name_clone, result.error);
                                failures
                                    .record(
                                        &agent_id_clone,
                                        FailureKind::RunnerFailed,
                                        Some(&runner_name_clone),
                                        None,
                                        &result.error,
                                    )
                                    .await;
                            }
                            if let Err(e) = token_provider
                                .remove_runner(&runner_scope, &runner_name_clone)
                                .await
                            {
                                warn!(
                                    "Could not remove runner '{}' from its forge (may have self-removed): {}",
                                    runner_name_clone, e
                                );
                            }
                            runner_state.remove_runner(&runner_name_clone).await;
                        }
                        other => {
                            warn!(
                                "Unexpected CreateRunner response from agent {} for {}: {:?}",
                                agent_id_clone, runner_name_clone, other
                            );
                            failures
                                .record(
                                    &agent_id_clone,
                                    FailureKind::CommandFailed,
                                    Some(&runner_name_clone),
                                    None,
                                    &format!("unexpected response to CreateRunner: {other:?}"),
                                )
                                .await;
                            agent_registry
                                .release_slot(&agent_id_clone, &runner_name_clone)
                                .await;
                            if let Err(e) = token_provider
                                .remove_runner(&runner_scope, &runner_name_clone)
                                .await
                            {
                                warn!(
                                    "Could not remove runner '{}' from its forge: {}",
                                    runner_name_clone, e
                                );
                            }
                            runner_state.remove_runner(&runner_name_clone).await;
                        }
                    },
                    Err(e) => {
                        error!("CreateRunner command failed: {}", e);
                        failures
                            .record(
                                &agent_id_clone,
                                FailureKind::CommandFailed,
                                Some(&runner_name_clone),
                                None,
                                &format!("CreateRunner: {e:#}"),
                            )
                            .await;
                        agent_registry
                            .release_slot(&agent_id_clone, &runner_name_clone)
                            .await;
                        if let Err(e) = token_provider
                            .remove_runner(&runner_scope, &runner_name_clone)
                            .await
                        {
                            warn!(
                                "Could not remove runner '{}' from its forge: {}",
                                runner_name_clone, e
                            );
                        }
                        runner_state.remove_runner(&runner_name_clone).await;
                    }
                }
            });
        }

        Ok(())
    }
}

/// Longest hostname prefix kept in a runner name, keeps names short in GitHub's UI
const MAX_RUNNER_HOST_LEN: usize = 20;

/// Builds `<short-host>-<suffix>`, falling back to `runner-<suffix>` when there's no usable hostname.
fn runner_name_for_host(hostname: &str, suffix: &str) -> String {
    // drop the domain (`.local`, `.lan`, fqdn) and anything proxmox/tart/github won't take in a name
    let short = hostname.split('.').next().unwrap_or_default();
    let mut host: String = short
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .take(MAX_RUNNER_HOST_LEN)
        .collect();
    host = host.trim_matches('-').to_string();
    if host.is_empty() {
        host = "runner".to_string();
    }
    format!("{host}-{suffix}")
}

/// What one scan of the forges' queues found.
#[derive(Debug, Default)]
struct QueueScan {
    /// Forges whose queues were listed successfully
    forges: HashSet<String>,
    /// Every job those forges listed as queued
    seen: HashSet<JobKey>,
}

/// Pending jobs the scan no longer listed as queued, oldest first. Only jobs on
/// forges the scan actually listed count: an unreachable forge drops nothing.
fn dropped_pending_jobs(
    pending: Vec<(JobKey, PendingJobInfo)>,
    scan: &QueueScan,
) -> Vec<(JobKey, PendingJobInfo)> {
    pending
        .into_iter()
        .filter(|(job_id, _)| scan.forges.contains(job_id.forge()) && !scan.seen.contains(job_id))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{IdleAction, MAX_IDLE_BACKOFF, idle_action, next_idle_backoff};
    use super::{QueueScan, dropped_pending_jobs, runner_name_for_host};
    use crate::config::RunnerScope;
    use crate::forge::JobKey;
    use crate::pending_jobs::PendingJobInfo;
    use std::time::Duration;

    #[test]
    fn idle_action_from_job_status() {
        assert_eq!(
            idle_action(&Ok(Some("in_progress".into()))),
            IdleAction::Started
        );
        assert_eq!(
            idle_action(&Ok(Some("queued".into()))),
            IdleAction::TearDown
        );
        assert_eq!(idle_action(&Ok(None)), IdleAction::TearDown);
        assert_eq!(idle_action(&Ok(Some("completed".into()))), IdleAction::Wait);
        assert_eq!(idle_action(&Err(anyhow::anyhow!("boom"))), IdleAction::Wait);
    }

    #[test]
    fn idle_backoff_doubles_up_to_the_cap() {
        let timeout = Duration::from_secs(600);
        let first = next_idle_backoff(None, timeout);
        assert_eq!(first, timeout);
        let second = next_idle_backoff(Some(first), timeout);
        assert_eq!(second, Duration::from_secs(1200));
        assert_eq!(
            next_idle_backoff(Some(second), timeout),
            Duration::from_secs(2400)
        );
        assert_eq!(
            next_idle_backoff(Some(Duration::from_secs(2400)), timeout),
            MAX_IDLE_BACKOFF
        );
        assert_eq!(
            next_idle_backoff(None, Duration::from_secs(7200)),
            MAX_IDLE_BACKOFF
        );
    }

    fn pending_job() -> PendingJobInfo {
        PendingJobInfo {
            job_labels: vec!["self-hosted".into()],
            agent_labels: vec!["self-hosted".into()],
            runner_scope: RunnerScope::Organization { name: "o".into() },
            runner_group: None,
            created_at: chrono::Utc::now(),
            retry_count: 0,
            repository: Some("o/r".into()),
            job_name: None,
            workflow_name: None,
            failed_agents: Vec::new(),
        }
    }

    fn key(forge: &str, id: &str) -> JobKey {
        JobKey::new(forge, id)
    }

    fn scan(forges: &[&str], seen: &[JobKey]) -> QueueScan {
        QueueScan {
            forges: forges.iter().map(|f| f.to_string()).collect(),
            seen: seen.iter().cloned().collect(),
        }
    }

    #[test]
    fn dropped_pending_jobs_are_those_the_scan_missed() {
        let pending = vec![
            (key("gh", "1"), pending_job()),
            (key("gh", "2"), pending_job()),
            (key("gh", "3"), pending_job()),
        ];
        let scan = scan(&["gh"], &[key("gh", "2"), key("gh", "99")]);
        let dropped: Vec<JobKey> = dropped_pending_jobs(pending, &scan)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(dropped, vec![key("gh", "1"), key("gh", "3")]);
    }

    #[test]
    fn nothing_dropped_when_scan_lists_everything() {
        let pending = vec![(key("gh", "1"), pending_job())];
        let scan = scan(&["gh"], &[key("gh", "1")]);
        assert!(dropped_pending_jobs(pending, &scan).is_empty());
    }

    #[test]
    fn jobs_on_unlisted_forges_are_not_dropped() {
        // "fj" failed to list this scan, so its job's absence means nothing
        let pending = vec![
            (key("gh", "1"), pending_job()),
            (key("fj", "1"), pending_job()),
        ];
        let scan = scan(&["gh"], &[]);
        let dropped: Vec<JobKey> = dropped_pending_jobs(pending, &scan)
            .into_iter()
            .map(|(id, _)| id)
            .collect();
        assert_eq!(dropped, vec![key("gh", "1")]);
    }

    #[test]
    fn runner_name_uses_short_hostname() {
        assert_eq!(
            runner_name_for_host("Jeremys-Mac-mini.local", "abcd1234"),
            "jeremys-mac-mini-abcd1234"
        );
        assert_eq!(
            runner_name_for_host("pve01.example.com", "abcd1234"),
            "pve01-abcd1234"
        );
        assert_eq!(
            runner_name_for_host("build_box 2", "abcd1234"),
            "build-box-2-abcd1234"
        );
    }

    #[test]
    fn runner_name_truncates_long_hostnames() {
        let name = runner_name_for_host("a-really-long-hostname-for-a-build-machine", "abcd1234");
        assert_eq!(name, "a-really-long-hostna-abcd1234");
        assert_eq!(
            runner_name_for_host("a-really-long-host--x", "abcd1234"),
            "a-really-long-host-abcd1234"
        );
    }

    #[test]
    fn runner_name_falls_back_without_hostname() {
        assert_eq!(runner_name_for_host("", "abcd1234"), "runner-abcd1234");
        assert_eq!(runner_name_for_host("...", "abcd1234"), "runner-abcd1234");
    }
}
