//! Persistent runner state for crash recovery.
//!
//! Tracks active runners so we can clean them up from GitHub if the
//! coordinator restarts while runners are active.
//!
//! State is persisted to the database (SQLite or PostgreSQL).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use tracing::{debug, error, info, warn};

use crate::config::RunnerScope;
use crate::db::DbPool;
use crate::sql;

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
    /// GitHub job ID (webhook mode only, for deduplication cleanup on failure)
    #[serde(default)]
    pub job_id: Option<u64>,
}

/// Persistent store for active runner state.
///
/// This allows the coordinator to recover after a restart and clean up
/// any runners that completed or whose agents disconnected while we were down.
pub struct RunnerStateStore {
    /// Database pool
    pool: DbPool,
}

impl RunnerStateStore {
    /// Create a new runner state store using the provided database pool.
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    /// Load initial state and log what we found.
    pub async fn load_and_log(&self) {
        match self.get_all_runners().await {
            Ok(runners) if !runners.is_empty() => {
                info!(
                    "Loaded {} active runner(s) from database",
                    runners.len()
                );
                for (name, info) in &runners {
                    debug!(
                        "  {} on agent {} (created {})",
                        name, info.agent_id, info.created_at
                    );
                }
            }
            Ok(_) => {
                debug!("No active runners in database");
            }
            Err(e) => {
                error!("Failed to load runners from database: {}", e);
            }
        }
    }

    /// Add a runner to the state.
    pub async fn add_runner(
        &self,
        runner_name: String,
        agent_id: String,
        vm_name: String,
        runner_scope: RunnerScope,
        job_id: Option<u64>,
    ) {
        let created_at = Utc::now();
        let scope_json = match serde_json::to_string(&runner_scope) {
            Ok(json) => json,
            Err(e) => {
                error!("Failed to serialize runner scope: {}", e);
                return;
            }
        };
        let created_at_str = created_at.to_rfc3339();
        let job_id_i64 = job_id.map(|id| id as i64);

        let result = sqlx::query(sql::INSERT_RUNNER)
            .bind(&runner_name)
            .bind(&agent_id)
            .bind(&vm_name)
            .bind(&scope_json)
            .bind(&created_at_str)
            .bind(job_id_i64)
            .execute(&self.pool)
            .await;

        match result {
            Ok(_) => {
                debug!("Added runner {} to database (job_id={:?})", runner_name, job_id);
            }
            Err(e) => {
                error!("Failed to add runner {} to database: {}", runner_name, e);
            }
        }
    }

    /// Remove a runner from the state.
    pub async fn remove_runner(&self, runner_name: &str) {
        let result = sqlx::query(sql::DELETE_RUNNER)
            .bind(runner_name)
            .execute(&self.pool)
            .await;

        match result {
            Ok(r) if r.rows_affected() > 0 => {
                debug!("Removed runner {} from database", runner_name);
            }
            Ok(_) => {
                // Runner wasn't in the database - that's fine
            }
            Err(e) => {
                error!("Failed to remove runner {} from database: {}", runner_name, e);
            }
        }
    }

    /// Get all runners for a specific agent.
    pub async fn get_runners_for_agent(&self, agent_id: &str) -> Vec<(String, RunnerInfo)> {
        let result = sqlx::query(sql::SELECT_RUNNERS_BY_AGENT)
            .bind(agent_id)
            .fetch_all(&self.pool)
            .await;

        match result {
            Ok(rows) => rows
                .into_iter()
                .filter_map(|row| self.row_to_runner_info(row))
                .collect(),
            Err(e) => {
                error!("Failed to get runners for agent {}: {}", agent_id, e);
                Vec::new()
            }
        }
    }

    /// Get all runners (for startup recovery).
    pub async fn get_all_runners(&self) -> Result<Vec<(String, RunnerInfo)>, sqlx::Error> {
        let rows = sqlx::query(sql::SELECT_ALL_RUNNERS)
            .fetch_all(&self.pool)
            .await?;

        Ok(rows
            .into_iter()
            .filter_map(|row| self.row_to_runner_info(row))
            .collect())
    }

    /// Get a single runner by name.
    pub async fn get_runner(&self, runner_name: &str) -> Option<RunnerInfo> {
        let result = sqlx::query(sql::SELECT_RUNNER)
            .bind(runner_name)
            .fetch_optional(&self.pool)
            .await;

        match result {
            Ok(Some(row)) => self.row_to_runner_info(row).map(|(_, info)| info),
            Ok(None) => None,
            Err(e) => {
                error!("Failed to get runner {}: {}", runner_name, e);
                None
            }
        }
    }

    /// Check if a runner exists in the state.
    pub async fn has_runner(&self, runner_name: &str) -> bool {
        self.get_runner(runner_name).await.is_some()
    }

    /// Reconcile persisted runners for an agent against the current VM list.
    ///
    /// Returns the names of runners that were removed (VM no longer exists).
    /// This is called when we receive a status update from an agent - if a
    /// persisted runner's VM is no longer in the VM list, the runner has completed.
    pub async fn reconcile_agent_vms(&self, agent_id: &str, vm_names: &[String]) -> Vec<(String, RunnerInfo)> {
        let runners_for_agent = self.get_runners_for_agent(agent_id).await;
        let mut removed = Vec::new();

        for (runner_name, runner_info) in runners_for_agent {
            // Check if the runner's VM is still in the agent's VM list
            if !vm_names.contains(&runner_info.vm_name) {
                info!(
                    "Runner '{}' VM '{}' no longer on agent '{}' - marking for cleanup",
                    runner_name, runner_info.vm_name, agent_id
                );
                removed.push((runner_name, runner_info));
            }
        }

        // Remove the completed runners from state
        for (runner_name, _) in &removed {
            self.remove_runner(runner_name).await;
        }

        removed
    }

    /// Convert a database row to a (runner_name, RunnerInfo) tuple.
    fn row_to_runner_info(&self, row: crate::db::DbRow) -> Option<(String, RunnerInfo)> {
        let runner_name: String = row.try_get("runner_name").ok()?;
        let agent_id: String = row.try_get("agent_id").ok()?;
        let vm_name: String = row.try_get("vm_name").ok()?;
        let scope_json: String = row.try_get("runner_scope").ok()?;
        let created_at_str: String = row.try_get("created_at").ok()?;
        let job_id: Option<i64> = row.try_get("job_id").ok()?;

        let runner_scope: RunnerScope = match serde_json::from_str(&scope_json) {
            Ok(s) => s,
            Err(e) => {
                warn!("Failed to deserialize runner scope for {}: {}", runner_name, e);
                return None;
            }
        };

        let created_at = match DateTime::parse_from_rfc3339(&created_at_str) {
            Ok(dt) => dt.with_timezone(&Utc),
            Err(e) => {
                warn!("Failed to parse created_at for {}: {}", runner_name, e);
                return None;
            }
        };

        Some((
            runner_name,
            RunnerInfo {
                agent_id,
                vm_name,
                runner_scope,
                created_at,
                job_id: job_id.map(|id| id as u64),
            },
        ))
    }
}
