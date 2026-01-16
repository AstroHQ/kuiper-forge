//! Persistent runner state for crash recovery.
//!
//! Tracks active runners so we can clean them up from GitHub if the
//! coordinator restarts while runners are active.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

use crate::config::RunnerScope;

/// Information about an active runner.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerInfo {
    /// Agent ID where this runner is running
    pub agent_id: String,
    /// VM name on the agent
    pub vm_name: String,
    /// Runner scope for GitHub API calls
    pub runner_scope: RunnerScope,
    /// When the runner was created
    pub created_at: DateTime<Utc>,
}

/// Persistent store for active runner state.
///
/// This allows the coordinator to recover after a restart and clean up
/// any runners that completed or whose agents disconnected while we were down.
pub struct RunnerStateStore {
    /// Path to the state file
    state_file: PathBuf,
    /// In-memory state (runner_name -> info)
    runners: Arc<RwLock<HashMap<String, RunnerInfo>>>,
}

impl RunnerStateStore {
    /// Create a new runner state store.
    ///
    /// Loads existing state from disk if available.
    pub fn new(data_dir: &Path) -> Self {
        let state_file = data_dir.join("runner_state.json");
        let runners = Self::load_from_file(&state_file).unwrap_or_default();

        if !runners.is_empty() {
            info!(
                "Loaded {} active runner(s) from state file",
                runners.len()
            );
            for (name, info) in &runners {
                debug!(
                    "  {} on agent {} (created {})",
                    name, info.agent_id, info.created_at
                );
            }
        }

        Self {
            state_file,
            runners: Arc::new(RwLock::new(runners)),
        }
    }

    /// Load state from file.
    fn load_from_file(path: &Path) -> Option<HashMap<String, RunnerInfo>> {
        if !path.exists() {
            return None;
        }

        match std::fs::read_to_string(path) {
            Ok(content) => match serde_json::from_str(&content) {
                Ok(state) => Some(state),
                Err(e) => {
                    warn!("Failed to parse runner state file: {}", e);
                    None
                }
            },
            Err(e) => {
                warn!("Failed to read runner state file: {}", e);
                None
            }
        }
    }

    /// Save state to file.
    async fn save(&self) {
        let runners = self.runners.read().await;
        let content = match serde_json::to_string_pretty(&*runners) {
            Ok(c) => c,
            Err(e) => {
                error!("Failed to serialize runner state: {}", e);
                return;
            }
        };

        // Ensure parent directory exists
        if let Some(parent) = self.state_file.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                error!("Failed to create state directory: {}", e);
                return;
            }
        }

        if let Err(e) = std::fs::write(&self.state_file, content) {
            error!("Failed to write runner state file: {}", e);
        } else {
            debug!("Saved runner state ({} runners)", runners.len());
        }
    }

    /// Add a runner to the state.
    pub async fn add_runner(
        &self,
        runner_name: String,
        agent_id: String,
        vm_name: String,
        runner_scope: RunnerScope,
    ) {
        let info = RunnerInfo {
            agent_id,
            vm_name,
            runner_scope,
            created_at: Utc::now(),
        };

        {
            let mut runners = self.runners.write().await;
            runners.insert(runner_name.clone(), info);
        }

        debug!("Added runner {} to state", runner_name);
        self.save().await;
    }

    /// Remove a runner from the state.
    pub async fn remove_runner(&self, runner_name: &str) {
        let removed = {
            let mut runners = self.runners.write().await;
            runners.remove(runner_name).is_some()
        };

        if removed {
            debug!("Removed runner {} from state", runner_name);
            self.save().await;
        }
    }

    /// Get all runners for a specific agent.
    pub async fn get_runners_for_agent(&self, agent_id: &str) -> Vec<(String, RunnerInfo)> {
        let runners = self.runners.read().await;
        runners
            .iter()
            .filter(|(_, info)| info.agent_id == agent_id)
            .map(|(name, info)| (name.clone(), info.clone()))
            .collect()
    }

    /// Get all runners (for startup recovery).
    pub async fn get_all_runners(&self) -> Vec<(String, RunnerInfo)> {
        let runners = self.runners.read().await;
        runners
            .iter()
            .map(|(name, info)| (name.clone(), info.clone()))
            .collect()
    }

    /// Check if a runner exists in the state.
    pub async fn has_runner(&self, runner_name: &str) -> bool {
        let runners = self.runners.read().await;
        runners.contains_key(runner_name)
    }
}
