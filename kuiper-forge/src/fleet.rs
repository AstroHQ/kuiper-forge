//! Fleet manager for maintaining runner pools.
//!
//! The fleet manager is responsible for:
//! - Maintaining a target number of runners per configuration (fixed capacity mode)
//! - Creating runners on-demand from webhook events (webhook mode)
//! - Getting registration tokens from GitHub (or mock provider in dry-run mode)
//! - Recovering runners when agents reconnect after coordinator restart

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{RwLock, mpsc};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use kuiper_agent_proto::{
    AgentPayload, CoordinatorMessage, CoordinatorPayload, CreateRunnerCommand, RunnerEvent,
    RunnerEventType,
};

use crate::agent_registry::{AgentRegistry, PoolDefinition};
use crate::config::{Config, ProvisioningMode, RunnerScope};
use crate::github::RunnerTokenProvider;
use crate::pending_jobs::PendingJobStore;
use crate::runner_state::RunnerStateStore;
use crate::webhook::{WebhookEvent, WebhookNotifier};

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
        let _ = self.notify_tx.try_send(());
    }

    /// Notify with recovery info when an agent has existing VMs.
    ///
    /// Called when an agent connects with VMs that may match persisted runners.
    /// The fleet manager will spawn watchers for matching runners.
    pub async fn notify_with_recovery(&self, agent_id: String, vm_names: Vec<String>) {
        let _ = self
            .recovery_tx
            .try_send(AgentRecoveryInfo { agent_id, vm_names });
        // Also trigger a reconcile
        let _ = self.notify_tx.try_send(());
    }

    /// Notify with a runner lifecycle event from an agent.
    pub async fn notify_runner_event(&self, agent_id: String, event: RunnerEvent) {
        let _ = self
            .runner_event_tx
            .try_send(AgentRunnerEvent { agent_id, event });
        let _ = self.notify_tx.try_send(());
    }
}

/// Tracks active runners per configuration.
#[derive(Debug, Default)]
struct RunnerPool {
    /// Pending runners (waiting for creation to complete)
    pending_runners: HashMap<usize, u32>,
}

/// Fleet manager that maintains runner pools.
pub struct FleetManager {
    /// Configuration
    config: Config,
    /// Token provider for getting registration tokens (GitHub API or mock)
    token_provider: Arc<dyn RunnerTokenProvider>,
    /// Agent registry for sending commands
    agent_registry: Arc<AgentRegistry>,
    /// Runner pool state
    pool: Arc<RwLock<RunnerPool>>,
    /// Persistent runner state for crash recovery
    runner_state: Arc<RunnerStateStore>,
    /// Persistent pending jobs store (webhook mode only)
    pending_job_store: Arc<PendingJobStore>,
    /// Channel for receiving reconciliation notifications
    notify_rx: mpsc::Receiver<()>,
    /// Channel for receiving recovery notifications
    recovery_rx: mpsc::Receiver<AgentRecoveryInfo>,
    /// Channel for receiving runner lifecycle events
    runner_event_rx: mpsc::Receiver<AgentRunnerEvent>,
    /// Channel for receiving webhook events (webhook mode only)
    webhook_rx: Option<mpsc::Receiver<WebhookEvent>>,
}

impl FleetManager {
    /// Create a new fleet manager and notification handles.
    ///
    /// Returns the fleet manager, a notifier for triggering reconciliation,
    /// and optionally a webhook notifier (only in webhook provisioning mode).
    pub fn new(
        config: Config,
        token_provider: Arc<dyn RunnerTokenProvider>,
        agent_registry: Arc<AgentRegistry>,
        runner_state: Arc<RunnerStateStore>,
        pending_job_store: Arc<PendingJobStore>,
    ) -> (Self, FleetNotifier, Option<WebhookNotifier>) {
        let (notify_tx, notify_rx) = mpsc::channel(16);
        let (recovery_tx, recovery_rx) = mpsc::channel(16);
        let (runner_event_tx, runner_event_rx) = mpsc::channel(32);

        // Create webhook channel only in webhook mode
        let (webhook_rx, webhook_notifier) = match config.provisioning.mode {
            ProvisioningMode::Webhook => {
                let (tx, rx) = mpsc::channel(32);
                (Some(rx), Some(WebhookNotifier::new(tx)))
            }
            ProvisioningMode::FixedCapacity => (None, None),
        };

        let manager = Self {
            config,
            token_provider,
            agent_registry,
            pool: Arc::new(RwLock::new(RunnerPool::default())),
            runner_state,
            pending_job_store,
            notify_rx,
            recovery_rx,
            runner_event_rx,
            webhook_rx,
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
    /// - **Webhook**: Waits passively for webhook-triggered runner creation (not yet implemented).
    ///   Still handles runner events and recovery for any runners that were created.
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
                self.run_webhook_loop().await;
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

    /// Run the webhook provisioning loop.
    ///
    /// In webhook mode, the fleet manager does not actively create runners.
    /// Instead, it waits for webhook events to trigger runner creation.
    /// This loop still handles recovery and runner lifecycle events.
    ///
    /// A periodic check ensures pending jobs are processed even if notifications
    /// are lost (e.g., channel was full when webhook handler tried to notify).
    async fn run_webhook_loop(&mut self) {
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

        info!("Webhook provisioning mode active - waiting for webhook events");

        // Process any pending jobs from previous run on startup
        self.process_pending_jobs().await;

        // Poll GitHub API for queued jobs that may have been missed during downtime
        self.recover_queued_jobs_from_github().await;

        // Periodic check interval - ensures pending jobs are processed even if
        // notifications are lost (belt-and-suspenders with the DB-backed queue)
        let check_interval = Duration::from_secs(30);
        let mut ticker = tokio::time::interval(check_interval);

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    // Periodic check for pending jobs (in case notifications were lost)
                    self.process_pending_jobs().await;
                }
                Some(()) = self.notify_rx.recv() => {
                    // Agent connected - check if we can assign pending jobs
                    debug!("Agent connected - checking pending job queue");
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
                self.pending_job_store.remove_job(job_id).await;
                debug!("Cleaned up state for completed job {}", job_id);

                // Job completed means an agent may be free - try pending jobs
                self.process_pending_jobs().await;
            }
        }
    }

    /// Recover queued jobs from GitHub API that may have been missed.
    ///
    /// This is called on startup to catch jobs that were queued while the
    /// coordinator was down or if webhooks were lost.
    async fn recover_queued_jobs_from_github(&self) {
        use crate::webhook::{has_required_labels, WebhookRunnerRequest};

        // Get required_labels from webhook config
        let required_labels = match &self.config.provisioning.webhook {
            Some(webhook_config) => webhook_config.required_labels.clone(),
            None => {
                debug!("No webhook config - skipping GitHub job recovery");
                return;
            }
        };

        info!("Polling GitHub API for queued jobs that may have been missed...");

        // Query GitHub for queued jobs
        let queued_jobs = match self.token_provider.list_queued_jobs().await {
            Ok(jobs) => jobs,
            Err(e) => {
                warn!("Failed to list queued jobs from GitHub: {}", e);
                return;
            }
        };

        if queued_jobs.is_empty() {
            info!("No queued jobs found in GitHub");
            return;
        }

        info!("Found {} queued job(s) in GitHub - checking against filters", queued_jobs.len());

        let mut added_count = 0;
        for job in queued_jobs {
            // Check if job matches required_labels
            if !has_required_labels(&job.labels, &required_labels) {
                debug!(
                    "Job {} doesn't match required labels {:?} - skipping",
                    job.job_id, required_labels
                );
                continue;
            }

            // Check if we already have this job in pending queue
            if self.pending_job_store.has_job(job.job_id).await {
                debug!("Job {} already in pending queue - skipping", job.job_id);
                continue;
            }

            // Check if we already have a runner for this job
            if self.runner_state.has_runner_for_job(job.job_id).await {
                debug!("Job {} already has active runner - skipping", job.job_id);
                continue;
            }

            // Add to pending queue
            let request = WebhookRunnerRequest {
                job_id: job.job_id,
                job_labels: job.labels.clone(),
                agent_labels: job.labels.clone(), // Use job labels for agent matching
                runner_scope: job.runner_scope,
                runner_group: None,
            };

            if self.pending_job_store.add_job(&request).await {
                info!(
                    "Recovered queued job {} from GitHub (labels: {:?})",
                    job.job_id, job.labels
                );
                added_count += 1;
            }
        }

        if added_count > 0 {
            info!("Recovered {} job(s) from GitHub API - processing", added_count);
            self.process_pending_jobs().await;
        }
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

        for (job_id, job_info) in pending_jobs {
            // Check retry count - abandon jobs that have failed too many times
            if job_info.retry_count >= MAX_RETRIES {
                warn!(
                    "Pending job {} has exceeded max retries ({}) - abandoning",
                    job_id, MAX_RETRIES
                );
                self.pending_job_store.remove_job(job_id).await;
                continue;
            }

            // Skip if a runner already exists for this job (it's already being processed)
            if self.runner_state.has_runner_for_job(job_id).await {
                debug!(
                    "Pending job {} already has active runner - skipping",
                    job_id
                );
                continue;
            }

            // Try to create a runner for this job
            match self
                .create_runner_on_demand(
                    &job_info.agent_labels,
                    &job_info.runner_scope,
                    job_info.runner_group.as_deref(),
                    job_id,
                )
                .await
            {
                Ok(()) => {
                    // Runner creation initiated - job stays in DB until runner completes/fails
                    // The has_runner_for_job check will skip it on subsequent iterations
                    info!("Created runner for pending job {}", job_id);
                }
                Err(e) => {
                    let err_msg = e.to_string();
                    if err_msg.contains("No available agent") {
                        // No more capacity - stop processing
                        debug!("No more capacity for pending jobs");
                        return;
                    } else {
                        // Different error (token fetch failed, etc.) - increment retry count
                        if let Some(new_count) =
                            self.pending_job_store.increment_retry_count(job_id).await
                        {
                            if new_count >= MAX_RETRIES {
                                warn!(
                                    "Job {} failed to create runner and exceeded max retries: {}",
                                    job_id, e
                                );
                                self.pending_job_store.remove_job(job_id).await;
                            } else {
                                warn!(
                                    "Job {} failed to create runner (attempt {}/{}): {}",
                                    job_id,
                                    new_count,
                                    MAX_RETRIES,
                                    e
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
    async fn create_runner_on_demand(
        &self,
        labels: &[String],
        runner_scope: &RunnerScope,
        _runner_group: Option<&str>,
        job_id: u64,
    ) -> anyhow::Result<()> {
        // Find an available agent with matching labels
        let agent_id = self
            .agent_registry
            .find_available_agent(labels)
            .await
            .ok_or_else(|| anyhow::anyhow!("No available agent for labels {labels:?}"))?;

        info!(
            "Found available agent {} for job {} with labels {:?}",
            agent_id, job_id, labels
        );

        // Reserve a slot on the agent
        if !self.agent_registry.reserve_slot(&agent_id).await {
            anyhow::bail!("Failed to reserve slot on agent {agent_id} (might be at capacity)");
        }

        // Get registration token from provider (GitHub API or mock)
        let token = match self
            .token_provider
            .get_registration_token(runner_scope)
            .await
        {
            Ok(t) => t,
            Err(e) => {
                // Release the reserved slot since we won't use it
                self.agent_registry.release_slot(&agent_id).await;
                return Err(e);
            }
        };

        // Generate runner name and command ID
        let runner_name = format!("runner-{}", &Uuid::new_v4().to_string()[..8]);
        let command_id = Uuid::new_v4().to_string();

        // Save runner state for crash recovery (include job_id for dedup cleanup on failure)
        self.runner_state
            .add_runner(
                runner_name.clone(),
                agent_id.clone(),
                runner_name.clone(), // vm_name is same as runner_name
                runner_scope.clone(),
                Some(job_id),
            )
            .await;

        // Build CreateRunner command
        let create_cmd = CreateRunnerCommand {
            command_id: command_id.clone(),
            vm_name: runner_name.clone(),
            registration_token: token,
            labels: labels.to_vec(),
            runner_scope_url: runner_scope.to_url(),
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
        let token_provider = self.token_provider.clone();
        let pending_job_store = self.pending_job_store.clone();
        let runner_scope = runner_scope.clone();
        let runner_name_clone = runner_name.clone();
        let runner_state = self.runner_state.clone();
        let agent_id_clone = agent_id.clone();

        // Helper to handle retry counting on command-level failures
        // (agent rejection, timeout, unexpected response)
        async fn handle_command_failure(
            pending_job_store: &crate::pending_jobs::PendingJobStore,
            job_id: u64,
            error_context: &str,
        ) {
            use crate::pending_jobs::MAX_RETRIES;

            if let Some(new_count) = pending_job_store.increment_retry_count(job_id).await {
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
                            agent_registry.release_slot(&agent_id_clone).await;
                            if let Err(e) = token_provider
                                .remove_runner(&runner_scope, &runner_name_clone)
                                .await
                            {
                                warn!(
                                    "Could not remove runner '{}' from GitHub: {}",
                                    runner_name_clone, e
                                );
                            }
                            runner_state.remove_runner(&runner_name_clone).await;
                            handle_command_failure(
                                &pending_job_store,
                                job_id,
                                &format!("agent rejection: {}", ack.error),
                            )
                            .await;
                        }
                    }
                    Some(AgentPayload::Result(result)) => {
                        // Legacy agents may respond with a full lifecycle result
                        agent_registry.release_slot(&agent_id_clone).await;
                        if result.success {
                            info!(
                                "Runner {} completed successfully (webhook)",
                                runner_name_clone
                            );
                            // Success - remove from pending queue
                            pending_job_store.remove_job(job_id).await;
                        } else {
                            warn!("Runner {} failed: {}", runner_name_clone, result.error);
                            handle_command_failure(
                                &pending_job_store,
                                job_id,
                                &format!("runner failure: {}", result.error),
                            )
                            .await;
                        }
                        if let Err(e) = token_provider
                            .remove_runner(&runner_scope, &runner_name_clone)
                            .await
                        {
                            warn!(
                                "Could not remove runner '{}' from GitHub: {}",
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
                        agent_registry.release_slot(&agent_id_clone).await;
                        if let Err(e) = token_provider
                            .remove_runner(&runner_scope, &runner_name_clone)
                            .await
                        {
                            warn!(
                                "Could not remove runner '{}' from GitHub: {}",
                                runner_name_clone, e
                            );
                        }
                        runner_state.remove_runner(&runner_name_clone).await;
                        handle_command_failure(
                            &pending_job_store,
                            job_id,
                            "unexpected agent response",
                        )
                        .await;
                    }
                },
                Err(e) => {
                    error!("CreateRunner command failed (webhook): {}", e);
                    agent_registry.release_slot(&agent_id_clone).await;
                    if let Err(e_inner) = token_provider
                        .remove_runner(&runner_scope, &runner_name_clone)
                        .await
                    {
                        warn!(
                            "Could not remove runner '{}' from GitHub: {}",
                            runner_name_clone, e_inner
                        );
                    }
                    runner_state.remove_runner(&runner_name_clone).await;
                    handle_command_failure(&pending_job_store, job_id, &format!("command error: {}", e))
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
                );
            } else {
                // VM is gone - the runner completed or failed while coordinator was down
                warn!(
                    "Persisted runner '{}' not found on agent '{}' - cleaning up from GitHub",
                    runner_name, recovery_info.agent_id
                );

                // Clean up from GitHub
                let token_provider = self.token_provider.clone();
                let runner_scope = runner_info.runner_scope.clone();
                let runner_name_clone = runner_name.clone();
                let runner_state = self.runner_state.clone();

                tokio::spawn(async move {
                    if let Err(e) = token_provider
                        .remove_runner(&runner_scope, &runner_name_clone)
                        .await
                    {
                        warn!(
                            "Could not remove stale runner '{}' from GitHub: {}",
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
                let Some(runner_info) = runner_info else {
                    warn!(
                        agent_id = %event.agent_id,
                        runner_name = %runner_name,
                        "Runner event received for unknown runner"
                    );
                    return;
                };

                // Note: Pool index lookup removed - pools are now dynamically derived.
                // Pending counts are managed during reconcile_pool based on current agents.

                self.agent_registry.release_slot(&event.agent_id).await;

                // Agent slot freed - try to process pending webhook jobs
                self.process_pending_jobs().await;

                // Handle pending job state based on runner outcome
                if let Some(job_id) = runner_info.job_id {
                    use crate::pending_jobs::MAX_RETRIES;

                    match event_type {
                        RunnerEventType::Completed => {
                            // Success - remove from pending queue
                            self.pending_job_store.remove_job(job_id).await;
                            info!(
                                agent_id = %event.agent_id,
                                runner_name = %runner_name,
                                job_id = %job_id,
                                "Runner completed successfully"
                            );
                        }
                        RunnerEventType::Failed | RunnerEventType::Destroyed => {
                            // Failure - increment retry count for potential retry
                            if let Some(new_count) =
                                self.pending_job_store.increment_retry_count(job_id).await
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
                                    self.pending_job_store.remove_job(job_id).await;
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

                let token_provider = self.token_provider.clone();
                let runner_scope = runner_info.runner_scope.clone();
                let runner_name_clone = runner_name.clone();
                tokio::spawn(async move {
                    if let Err(e) = token_provider
                        .remove_runner(&runner_scope, &runner_name_clone)
                        .await
                    {
                        warn!(
                            "Could not remove runner '{}' from GitHub: {}",
                            runner_name_clone, e
                        );
                    }
                });
            }
        }
    }

    /// Spawn a watcher task for a recovered runner.
    ///
    /// The watcher will poll the agent's VM list until the runner completes,
    /// then clean up from GitHub and update state.
    fn spawn_runner_watcher(
        &self,
        runner_name: String,
        agent_id: String,
        runner_scope: crate::config::RunnerScope,
    ) {
        let agent_registry = self.agent_registry.clone();
        let token_provider = self.token_provider.clone();
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

            // Clean up from GitHub
            info!(
                "Recovery watcher for '{}' exiting, cleaning up from GitHub",
                runner_name
            );
            if let Err(e) = token_provider
                .remove_runner(&runner_scope, &runner_name)
                .await
            {
                warn!(
                    "Could not remove runner '{}' from GitHub: {}",
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
        let agent_count = self.agent_registry.count().await;
        let pool_defs = self.agent_registry.get_pool_definitions().await;

        info!(
            "Fleet manager reconciling: {} pools from {} connected agents",
            pool_defs.len(),
            agent_count
        );

        for (idx, pool_def) in pool_defs.iter().enumerate() {
            if let Err(e) = self
                .reconcile_pool(
                    idx,
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

    /// Reconcile a single runner pool.
    ///
    /// Creates runners up to the target count for this pool, using agents that
    /// match the required labels.
    async fn reconcile_pool(
        &self,
        config_idx: usize,
        pool_def: &PoolDefinition,
        runner_scope: &RunnerScope,
        _runner_group: Option<&String>,
    ) -> anyhow::Result<()> {
        let pending = {
            let pool = self.pool.read().await;
            *pool.pending_runners.get(&config_idx).unwrap_or(&0)
        };

        let target = pool_def.target_count;

        info!(
            "Pool {:?}: checking - pending={}, target={}",
            pool_def.labels, pending, target
        );

        if pending >= target {
            info!(
                "Pool {:?}: {}/{} pending (target met, nothing to do)",
                pool_def.labels, pending, target
            );
            return Ok(());
        }

        let needed = target - pending;

        // Check available capacity before trying to create runners
        let capacity = self
            .agent_registry
            .available_capacity(&pool_def.labels)
            .await;

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
            "Pool {:?}: {}/{} pending, need {} more, capacity for {}, will try to create {}",
            pool_def.labels, pending, target, needed, capacity, to_create
        );

        // Try to create runners up to the available capacity
        for i in 0..to_create {
            info!(
                "Pool {:?}: creating runner {}/{}",
                pool_def.labels,
                i + 1,
                to_create
            );

            // Find an available agent with matching labels
            let agent_id = match self
                .agent_registry
                .find_available_agent(&pool_def.labels)
                .await
            {
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

            // Reserve a slot on the agent to prevent over-scheduling
            if !self.agent_registry.reserve_slot(&agent_id).await {
                warn!(
                    "Failed to reserve slot on agent {} (might be at capacity now)",
                    agent_id
                );
                continue;
            }
            info!("Reserved slot on agent {}", agent_id);

            // Get registration token from provider (GitHub API or mock)
            let token = match self
                .token_provider
                .get_registration_token(runner_scope)
                .await
            {
                Ok(t) => t,
                Err(e) => {
                    error!("Failed to get registration token: {}", e);
                    // Release the reserved slot since we won't use it
                    self.agent_registry.release_slot(&agent_id).await;
                    break;
                }
            };

            // Generate runner name and command ID
            let runner_name = format!("runner-{}", &Uuid::new_v4().to_string()[..8]);
            let command_id = Uuid::new_v4().to_string();

            // Mark as pending
            {
                let mut pool = self.pool.write().await;
                *pool.pending_runners.entry(config_idx).or_insert(0) += 1;
            }

            // Save runner state for crash recovery (no job_id in fixed capacity mode)
            self.runner_state
                .add_runner(
                    runner_name.clone(),
                    agent_id.clone(),
                    runner_name.clone(), // vm_name is same as runner_name
                    runner_scope.clone(),
                    None,
                )
                .await;

            // Build CreateRunner command
            let create_cmd = CreateRunnerCommand {
                command_id: command_id.clone(),
                vm_name: runner_name.clone(),
                registration_token: token,
                labels: pool_def.labels.clone(),
                runner_scope_url: runner_scope.to_url(),
            };

            let coordinator_msg = CoordinatorMessage {
                payload: Some(CoordinatorPayload::CreateRunner(create_cmd)),
            };

            info!(
                "Sending CreateRunner to agent {} for {} (agent-driven mode)",
                agent_id, runner_name
            );

            // Clone for async task
            let pool = self.pool.clone();
            let config_idx_copy = config_idx;
            let agent_registry = self.agent_registry.clone();
            let agent_id_clone = agent_id.clone();
            let token_provider = self.token_provider.clone();
            let runner_scope = runner_scope.clone();
            let runner_name_clone = runner_name.clone();
            let runner_state = self.runner_state.clone();

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
                                {
                                    let mut pool = pool.write().await;
                                    if let Some(p) = pool.pending_runners.get_mut(&config_idx_copy)
                                    {
                                        *p = p.saturating_sub(1);
                                    }
                                }
                                agent_registry.release_slot(&agent_id_clone).await;
                                if let Err(e) = token_provider
                                    .remove_runner(&runner_scope, &runner_name_clone)
                                    .await
                                {
                                    warn!(
                                        "Could not remove runner '{}' from GitHub: {}",
                                        runner_name_clone, e
                                    );
                                }
                                runner_state.remove_runner(&runner_name_clone).await;
                            }
                        }
                        Some(AgentPayload::Result(result)) => {
                            {
                                let mut pool = pool.write().await;
                                if let Some(p) = pool.pending_runners.get_mut(&config_idx_copy) {
                                    *p = p.saturating_sub(1);
                                }
                            }
                            agent_registry.release_slot(&agent_id_clone).await;
                            if result.success {
                                info!("Runner {} completed successfully", runner_name_clone);
                            } else {
                                warn!("Runner {} failed: {}", runner_name_clone, result.error);
                            }
                            if let Err(e) = token_provider
                                .remove_runner(&runner_scope, &runner_name_clone)
                                .await
                            {
                                warn!(
                                    "Could not remove runner '{}' from GitHub (may have self-removed): {}",
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
                            {
                                let mut pool = pool.write().await;
                                if let Some(p) = pool.pending_runners.get_mut(&config_idx_copy) {
                                    *p = p.saturating_sub(1);
                                }
                            }
                            agent_registry.release_slot(&agent_id_clone).await;
                            if let Err(e) = token_provider
                                .remove_runner(&runner_scope, &runner_name_clone)
                                .await
                            {
                                warn!(
                                    "Could not remove runner '{}' from GitHub: {}",
                                    runner_name_clone, e
                                );
                            }
                            runner_state.remove_runner(&runner_name_clone).await;
                        }
                    },
                    Err(e) => {
                        error!("CreateRunner command failed: {}", e);
                        {
                            let mut pool = pool.write().await;
                            if let Some(p) = pool.pending_runners.get_mut(&config_idx_copy) {
                                *p = p.saturating_sub(1);
                            }
                        }
                        agent_registry.release_slot(&agent_id_clone).await;
                        if let Err(e) = token_provider
                            .remove_runner(&runner_scope, &runner_name_clone)
                            .await
                        {
                            warn!(
                                "Could not remove runner '{}' from GitHub: {}",
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
