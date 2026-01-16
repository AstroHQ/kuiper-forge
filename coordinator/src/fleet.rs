//! Fleet manager for maintaining runner pools.
//!
//! The fleet manager is responsible for:
//! - Maintaining a target number of runners per configuration
//! - Requesting new runners from agents when below target
//! - Getting registration tokens from GitHub (or mock provider in dry-run mode)

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, RwLock};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use agent_proto::{AgentPayload, CoordinatorMessage, CoordinatorPayload, CreateRunnerCommand};

use crate::agent_registry::AgentRegistry;
use crate::config::{Config, RunnerConfig};
use crate::github::RunnerTokenProvider;

/// Handle for triggering fleet manager reconciliation.
#[derive(Clone)]
pub struct FleetNotifier {
    tx: mpsc::Sender<()>,
}

impl FleetNotifier {
    /// Notify the fleet manager to reconcile immediately.
    ///
    /// Called when an agent connects to check if runners need to be created.
    pub async fn notify(&self) {
        // Use try_send to avoid blocking - if channel is full, reconciliation is already pending
        let _ = self.tx.try_send(());
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
    /// Channel for receiving reconciliation notifications
    notify_rx: mpsc::Receiver<()>,
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
    ) -> (Self, FleetNotifier) {
        let (tx, rx) = mpsc::channel(16);

        let manager = Self {
            config,
            token_provider,
            agent_registry,
            pool: Arc::new(RwLock::new(RunnerPool::default())),
            notify_rx: rx,
        };

        let notifier = FleetNotifier { tx };

        (manager, notifier)
    }

    /// Start the fleet manager loop.
    ///
    /// This runs forever, periodically checking and maintaining runner pools.
    /// Also responds to notifications for immediate reconciliation.
    pub async fn run(mut self) {
        let check_interval = Duration::from_secs(30);
        let mut ticker = tokio::time::interval(check_interval);

        info!("Fleet manager started with {} runner configurations", self.config.runners.len());

        loop {
            tokio::select! {
                _ = ticker.tick() => {
                    self.reconcile().await;
                }
                Some(()) = self.notify_rx.recv() => {
                    info!("Fleet manager triggered by agent connection");
                    self.reconcile().await;
                }
            }
        }
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

        if pending >= target {
            debug!(
                "Pool {:?}: {}/{} pending (target met)",
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

        if capacity == 0 {
            info!(
                "Pool {:?}: need {} more runners but no agent capacity available",
                runner_config.labels, needed
            );
            // Log all connected agents for debugging
            let all_agents = self.agent_registry.list_all().await;
            if all_agents.is_empty() {
                info!("  No agents connected");
            }
            for agent in &all_agents {
                info!(
                    "  Agent {}: labels={:?}, capacity={}/{}",
                    agent.agent_id, agent.labels, agent.max_vms - agent.active_vms, agent.max_vms
                );
            }
            return Ok(());
        }

        // Only try to create as many as we have capacity for
        let to_create = std::cmp::min(needed as usize, capacity) as u32;

        info!(
            "Pool {:?}: {}/{} pending, need {} more, capacity for {}",
            runner_config.labels, pending, target, needed, to_create
        );

        // Try to create runners up to the available capacity
        for _ in 0..to_create {
            // Find an available agent with matching labels
            let agent_id = match self
                .agent_registry
                .find_available_agent(&runner_config.labels)
                .await
            {
                Some(id) => id,
                None => {
                    debug!(
                        "No available agent for labels {:?}",
                        runner_config.labels
                    );
                    break;
                }
            };

            // Get registration token from provider (GitHub API or mock)
            let token = match self
                .token_provider
                .get_registration_token(&runner_config.runner_scope)
                .await
            {
                Ok(t) => t,
                Err(e) => {
                    error!("Failed to get registration token: {}", e);
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

            // Spawn task to send command and handle response
            // This allows us to continue creating other runners while waiting
            tokio::spawn(async move {
                let timeout = Duration::from_secs(3600); // 1 hour for runner lifecycle

                match agent_registry
                    .send_command(&agent_id_clone, coordinator_msg, &command_id, timeout)
                    .await
                {
                    Ok(response) => {
                        // Decrement pending when done
                        let mut pool = pool.write().await;
                        if let Some(p) = pool.pending_runners.get_mut(&config_idx_copy) {
                            *p = p.saturating_sub(1);
                        }

                        // Check if success
                        if let Some(AgentPayload::Result(result)) = response.payload {
                            if result.success {
                                info!("Runner completed successfully");
                            } else {
                                warn!("Runner failed: {}", result.error);
                            }
                        }
                    }
                    Err(e) => {
                        error!("CreateRunner command failed: {}", e);
                        let mut pool = pool.write().await;
                        if let Some(p) = pool.pending_runners.get_mut(&config_idx_copy) {
                            *p = p.saturating_sub(1);
                        }
                    }
                }
            });
        }

        Ok(())
    }
}
