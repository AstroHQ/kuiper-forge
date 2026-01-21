//! Fleet manager for maintaining runner pools.
//!
//! The fleet manager is responsible for:
//! - Maintaining a target number of runners per configuration
//! - Requesting new runners from agents when below target
//! - Getting registration tokens from GitHub (or mock provider in dry-run mode)
//! - Recovering runners when agents reconnect after coordinator restart

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, RwLock};
use tracing::{error, info, warn};
use uuid::Uuid;

use kuiper_agent_proto::{
    AgentPayload, CoordinatorMessage, CoordinatorPayload, CreateRunnerCommand, RunnerEvent,
    RunnerEventType,
};

use crate::agent_registry::AgentRegistry;
use crate::config::{Config, RunnerConfig};
use crate::github::RunnerTokenProvider;
use crate::runner_state::RunnerStateStore;

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
        let _ = self.recovery_tx.try_send(AgentRecoveryInfo { agent_id, vm_names });
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
    /// Channel for receiving reconciliation notifications
    notify_rx: mpsc::Receiver<()>,
    /// Channel for receiving recovery notifications
    recovery_rx: mpsc::Receiver<AgentRecoveryInfo>,
    /// Channel for receiving runner lifecycle events
    runner_event_rx: mpsc::Receiver<AgentRunnerEvent>,
}

impl FleetManager {
    /// Create a new fleet manager and notification handle.
    ///
    /// Returns the fleet manager and a notifier that can be used to trigger
    /// immediate reconciliation (e.g., when an agent connects).
    pub fn new(
        config: Config,
        token_provider: Arc<dyn RunnerTokenProvider>,
        agent_registry: Arc<AgentRegistry>,
        runner_state: Arc<RunnerStateStore>,
    ) -> (Self, FleetNotifier) {
        let (notify_tx, notify_rx) = mpsc::channel(16);
        let (recovery_tx, recovery_rx) = mpsc::channel(16);
        let (runner_event_tx, runner_event_rx) = mpsc::channel(32);

        let manager = Self {
            config,
            token_provider,
            agent_registry,
            pool: Arc::new(RwLock::new(RunnerPool::default())),
            runner_state,
            notify_rx,
            recovery_rx,
            runner_event_rx,
        };

        let notifier = FleetNotifier {
            notify_tx,
            recovery_tx,
            runner_event_tx,
        };

        (manager, notifier)
    }

    /// Start the fleet manager loop.
    ///
    /// This runs forever, periodically checking and maintaining runner pools.
    /// Also responds to notifications for immediate reconciliation and recovery.
    pub async fn run(mut self) {
        let check_interval = Duration::from_secs(30);
        let mut ticker = tokio::time::interval(check_interval);

        info!("Fleet manager started with {} runner configurations", self.config.runners.len());

        // On startup, log any persisted runners (from previous coordinator run)
        let persisted = self.runner_state.get_all_runners().await;
        if !persisted.is_empty() {
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

    /// Handle recovery when an agent reconnects with existing VMs.
    ///
    /// For each VM that matches a persisted runner, spawn a watcher task to
    /// monitor the runner's completion.
    async fn handle_agent_recovery(&self, recovery_info: AgentRecoveryInfo) {
        let persisted_runners = self.runner_state.get_runners_for_agent(&recovery_info.agent_id).await;

        if persisted_runners.is_empty() {
            info!(
                "Agent '{}' has {} VMs but no persisted runners to recover",
                recovery_info.agent_id, recovery_info.vm_names.len()
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

                // Find which config this runner matches (for pending count tracking)
                let config_idx = self.find_config_for_runner(&runner_info.runner_scope);

                // Mark as pending (so reconcile doesn't try to create more)
                if let Some(idx) = config_idx {
                    let mut pool = self.pool.write().await;
                    *pool.pending_runners.entry(idx).or_insert(0) += 1;
                    info!(
                        "Marked recovered runner '{}' as pending in pool idx {}",
                        runner_name, idx
                    );
                }

                // Spawn a watcher task for this runner
                self.spawn_runner_watcher(
                    runner_name.clone(),
                    recovery_info.agent_id.clone(),
                    runner_info.runner_scope,
                    config_idx,
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

                let config_idx = self.find_config_for_runner(&runner_info.runner_scope);
                if let Some(idx) = config_idx {
                    let mut pool = self.pool.write().await;
                    if let Some(p) = pool.pending_runners.get_mut(&idx) {
                        *p = p.saturating_sub(1);
                    }
                }

                self.agent_registry.release_slot(&event.agent_id).await;

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

    /// Find the runner config index that matches a runner scope.
    fn find_config_for_runner(&self, runner_scope: &crate::config::RunnerScope) -> Option<usize> {
        self.config.runners.iter().position(|rc| rc.runner_scope == *runner_scope)
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
        config_idx: Option<usize>,
    ) {
        let agent_registry = self.agent_registry.clone();
        let token_provider = self.token_provider.clone();
        let runner_state = self.runner_state.clone();
        let pool = self.pool.clone();

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

            // Decrement pending count when watcher exits
            if let Some(idx) = config_idx {
                let mut pool = pool.write().await;
                if let Some(p) = pool.pending_runners.get_mut(&idx) {
                    *p = p.saturating_sub(1);
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
    /// For each runner configuration, ensure we have the target number of runners.
    async fn reconcile(&self) {
        let agent_count = self.agent_registry.count().await;
        info!(
            "Fleet manager reconciling: {} runner configs, {} connected agents",
            self.config.runners.len(),
            agent_count
        );

        for (idx, runner_config) in self.config.runners.iter().enumerate() {
            if let Err(e) = self.reconcile_pool(idx, runner_config).await {
                error!(
                    "Failed to reconcile pool for {:?}: {}",
                    runner_config.labels, e
                );
            }
        }
    }

    /// Reconcile a single runner pool.
    async fn reconcile_pool(
        &self,
        config_idx: usize,
        runner_config: &RunnerConfig,
    ) -> anyhow::Result<()> {
        let pending = {
            let pool = self.pool.read().await;
            *pool.pending_runners.get(&config_idx).unwrap_or(&0)
        };

        let target = runner_config.count;

        info!(
            "Pool {:?}: checking - pending={}, target={}",
            runner_config.labels, pending, target
        );

        if pending >= target {
            info!(
                "Pool {:?}: {}/{} pending (target met, nothing to do)",
                runner_config.labels, pending, target
            );
            return Ok(());
        }

        let needed = target - pending;

        // Check available capacity before trying to create runners
        let capacity = self
            .agent_registry
            .available_capacity(&runner_config.labels)
            .await;

        // Log all agents for debugging
        let all_agents = self.agent_registry.list_all().await;
        info!(
            "Pool {:?}: need {} runners, found {} agents, total capacity={}",
            runner_config.labels, needed, all_agents.len(), capacity
        );
        for agent in &all_agents {
            info!(
                "  Agent {}: labels={:?}, active={}/{}, matches={}",
                agent.agent_id,
                agent.labels,
                agent.active_vms,
                agent.max_vms,
                runner_config.labels.iter().all(|l|
                    agent.labels.iter().any(|al| al.eq_ignore_ascii_case(l))
                )
            );
        }

        if capacity == 0 {
            warn!(
                "Pool {:?}: need {} more runners but no agent capacity available",
                runner_config.labels, needed
            );
            return Ok(());
        }

        // Only try to create as many as we have capacity for
        let to_create = std::cmp::min(needed as usize, capacity) as u32;

        info!(
            "Pool {:?}: {}/{} pending, need {} more, capacity for {}, will try to create {}",
            runner_config.labels, pending, target, needed, capacity, to_create
        );

        // Try to create runners up to the available capacity
        for i in 0..to_create {
            info!(
                "Pool {:?}: creating runner {}/{}",
                runner_config.labels, i + 1, to_create
            );

            // Find an available agent with matching labels
            let agent_id = match self
                .agent_registry
                .find_available_agent(&runner_config.labels)
                .await
            {
                Some(id) => {
                    info!("Found available agent: {}", id);
                    id
                }
                None => {
                    warn!(
                        "No available agent for labels {:?} on iteration {}/{}",
                        runner_config.labels, i + 1, to_create
                    );
                    break;
                }
            };

            // Reserve a slot on the agent to prevent over-scheduling
            // This is important because we send commands in parallel
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
                .get_registration_token(&runner_config.runner_scope)
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

            // Save runner state for crash recovery
            self.runner_state
                .add_runner(
                    runner_name.clone(),
                    agent_id.clone(),
                    runner_name.clone(), // vm_name is same as runner_name
                    runner_config.runner_scope.clone(),
                )
                .await;

            // Build CreateRunner command
            let create_cmd = CreateRunnerCommand {
                command_id: command_id.clone(),
                vm_name: runner_name.clone(),
                registration_token: token,
                labels: runner_config.labels.clone(),
                runner_scope_url: runner_config.runner_scope.to_url(),
            };

            let coordinator_msg = CoordinatorMessage {
                payload: Some(CoordinatorPayload::CreateRunner(create_cmd)),
            };

            info!(
                "Sending CreateRunner to agent {} for {}",
                agent_id, runner_name
            );

            // Clone for async task
            let pool = self.pool.clone();
            let config_idx_copy = config_idx;
            let agent_registry = self.agent_registry.clone();
            let agent_id_clone = agent_id.clone();
            let token_provider = self.token_provider.clone();
            let runner_scope = runner_config.runner_scope.clone();
            let runner_name_clone = runner_name.clone();
            let runner_state = self.runner_state.clone();

            // Spawn task to send command and handle response
            // This allows us to continue creating other runners while waiting
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
                                    "CreateRunner accepted by agent {} for {}",
                                    agent_id_clone, runner_name_clone
                                );
                            } else {
                                warn!(
                                    "CreateRunner rejected by agent {} for {}: {}",
                                    agent_id_clone, runner_name_clone, ack.error
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
                        }
                        Some(AgentPayload::Result(result)) => {
                            // Legacy agents may still respond with a full lifecycle result.
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
                                warn!(
                                    "Runner {} failed: {}",
                                    runner_name_clone, result.error
                                );
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
