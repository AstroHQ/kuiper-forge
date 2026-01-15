//! Fleet manager for maintaining runner pools.
//!
//! The fleet manager is responsible for:
//! - Maintaining a target number of runners per configuration
//! - Requesting new runners from agents when below target
//! - Getting registration tokens from GitHub for runners

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use agent_proto::{AgentPayload, CoordinatorMessage, CoordinatorPayload, CreateRunnerCommand};

use crate::agent_registry::AgentRegistry;
use crate::config::{Config, RunnerConfig};
use crate::github::GitHubClient;

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
    /// GitHub client for getting registration tokens
    github: GitHubClient,
    /// Agent registry for sending commands
    agent_registry: Arc<AgentRegistry>,
    /// Runner pool state
    pool: Arc<RwLock<RunnerPool>>,
}

impl FleetManager {
    /// Create a new fleet manager.
    pub fn new(
        config: Config,
        github: GitHubClient,
        agent_registry: Arc<AgentRegistry>,
    ) -> Self {
        Self {
            config,
            github,
            agent_registry,
            pool: Arc::new(RwLock::new(RunnerPool::default())),
        }
    }

    /// Start the fleet manager loop.
    ///
    /// This runs forever, periodically checking and maintaining runner pools.
    pub async fn run(&self) {
        let check_interval = Duration::from_secs(30);
        let mut ticker = tokio::time::interval(check_interval);

        info!("Fleet manager started with {} runner configurations", self.config.runners.len());

        loop {
            ticker.tick().await;
            self.reconcile().await;
        }
    }

    /// Reconcile the current state with the desired state.
    ///
    /// For each runner configuration, ensure we have the target number of runners.
    async fn reconcile(&self) {
        debug!("Fleet manager reconciling runner pools");

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
            debug!(
                "Pool {:?}: need {} more runners but no agent capacity available",
                runner_config.labels, needed
            );
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

            // Get registration token from GitHub
            let token = match self
                .github
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
                template: runner_config.template.clone(),
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
