//! Persistent pending webhook jobs store.
//!
//! Persists webhook-triggered runner requests that are waiting for agent capacity.
//! This ensures jobs survive coordinator restarts and aren't lost if the webhook
//! processing channel is full.
//!
//! Jobs are retried when runners fail due to configuration errors (SSH keys, VM boot, etc.).
//! After MAX_RETRIES failed attempts, the job is abandoned.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use tracing::{debug, error, info, warn};

use crate::config::RunnerScope;
use crate::db::DbPool;
use crate::sql;
use crate::webhook::WebhookRunnerRequest;

/// Maximum number of retry attempts for failed runners.
/// After this many failures, the job is removed and not retried.
pub const MAX_RETRIES: i32 = 3;

/// Stored information about a pending webhook job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingJobInfo {
    /// Job labels from the webhook (the labels the job requested)
    pub job_labels: Vec<String>,
    /// Labels to use for agent matching
    pub agent_labels: Vec<String>,
    /// Runner scope for the job
    pub runner_scope: RunnerScope,
    /// Optional runner group
    pub runner_group: Option<String>,
    /// When the job was received
    pub created_at: DateTime<Utc>,
    /// Number of failed runner attempts
    #[serde(default)]
    pub retry_count: i32,
    /// Repository full name (owner/repo) for GitHub API status checks
    #[serde(default)]
    pub repository: Option<String>,
    /// Job name from the workflow file
    #[serde(default)]
    pub job_name: Option<String>,
    /// Workflow name
    #[serde(default)]
    pub workflow_name: Option<String>,
    /// Agents that already failed a runner for this job. The scheduler avoids these on retry.
    #[serde(default)]
    pub failed_agents: Vec<String>,
}

/// Persistent store for pending webhook jobs.
///
/// This allows the coordinator to recover pending jobs after a restart
/// and prevents job loss when webhooks arrive faster than they can be processed.
pub struct PendingJobStore {
    pool: DbPool,
}

impl PendingJobStore {
    /// Create a new pending job store using the provided database pool.
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    /// Load pending jobs on startup and log what we found.
    pub async fn load_and_log(&self) {
        match self.get_all_pending_jobs().await {
            Ok(jobs) if !jobs.is_empty() => {
                info!(
                    "Loaded {} pending webhook job(s) from database - will process when agents available",
                    jobs.len()
                );
                for (job_id, info) in &jobs {
                    debug!(
                        "  Job {} with labels {:?} (received {})",
                        job_id, info.job_labels, info.created_at
                    );
                }
            }
            Ok(_) => {
                debug!("No pending webhook jobs in database");
            }
            Err(e) => {
                error!("Failed to load pending jobs from database: {}", e);
            }
        }
    }

    /// Add a pending job to the store.
    ///
    /// Returns true if the job was added (or already exists), false on error.
    pub async fn add_job(&self, request: &WebhookRunnerRequest) -> bool {
        let created_at = Utc::now();
        let job_labels_json = match serde_json::to_string(&request.job_labels) {
            Ok(json) => json,
            Err(e) => {
                error!("Failed to serialize job_labels: {}", e);
                return false;
            }
        };
        let agent_labels_json = match serde_json::to_string(&request.agent_labels) {
            Ok(json) => json,
            Err(e) => {
                error!("Failed to serialize agent_labels: {}", e);
                return false;
            }
        };
        let scope_json = match serde_json::to_string(&request.runner_scope) {
            Ok(json) => json,
            Err(e) => {
                error!("Failed to serialize runner_scope: {}", e);
                return false;
            }
        };
        let created_at_str = created_at.to_rfc3339();

        let result = sqlx::query(sql::INSERT_PENDING_JOB)
            .bind(request.job_id as i64)
            .bind(&job_labels_json)
            .bind(&agent_labels_json)
            .bind(&scope_json)
            .bind(&request.runner_group)
            .bind(&created_at_str)
            .bind(&request.repository)
            .bind(&request.job_name)
            .bind(&request.workflow_name)
            .execute(&self.pool)
            .await;

        match result {
            Ok(_) => {
                debug!("Added pending job {} to database", request.job_id);
                true
            }
            Err(e) => {
                error!(
                    "Failed to add pending job {} to database: {}",
                    request.job_id, e
                );
                false
            }
        }
    }

    /// Remove a pending job from the store.
    pub async fn remove_job(&self, job_id: u64) {
        let result = sqlx::query(sql::DELETE_PENDING_JOB)
            .bind(job_id as i64)
            .execute(&self.pool)
            .await;

        match result {
            Ok(r) if r.rows_affected() > 0 => {
                debug!("Removed pending job {} from database", job_id);
            }
            Ok(_) => {
                // Job wasn't in the database - that's fine (might have been processed)
            }
            Err(e) => {
                error!(
                    "Failed to remove pending job {} from database: {}",
                    job_id, e
                );
            }
        }
    }

    /// Increment the retry count for a pending job and remember which agent failed it,
    /// so the next attempt can go somewhere else.
    ///
    /// Returns the new retry count, or None if the job doesn't exist.
    pub async fn increment_retry_count(
        &self,
        job_id: u64,
        failed_agent: Option<&str>,
    ) -> Option<i32> {
        let result = sqlx::query(sql::INCREMENT_PENDING_JOB_RETRY)
            .bind(job_id as i64)
            .fetch_optional(&self.pool)
            .await;

        match result {
            Ok(Some(row)) => {
                let count: i32 = row.try_get("retry_count").ok()?;
                debug!("Incremented retry count for job {} to {}", job_id, count);
                if let Some(agent_id) = failed_agent {
                    self.record_failed_agent(job_id, agent_id).await;
                }
                Some(count)
            }
            Ok(None) => {
                debug!("Job {} not found when incrementing retry count", job_id);
                None
            }
            Err(e) => {
                error!("Failed to increment retry count for job {}: {}", job_id, e);
                None
            }
        }
    }

    /// Append an agent to the job's failed list. Read-modify-write is fine here: retries for one job
    /// are serialized by the fleet loop, and a lost entry only costs one extra attempt on that agent.
    async fn record_failed_agent(&self, job_id: u64, agent_id: &str) {
        let Some((_, info)) = self.get_job(job_id).await else {
            return;
        };
        if info.failed_agents.iter().any(|a| a == agent_id) {
            return;
        }
        let mut failed = info.failed_agents;
        failed.push(agent_id.to_string());
        let json = match serde_json::to_string(&failed) {
            Ok(j) => j,
            Err(e) => {
                error!(
                    "Failed to serialize failed_agents for job {}: {}",
                    job_id, e
                );
                return;
            }
        };
        if let Err(e) = sqlx::query(sql::SET_PENDING_JOB_FAILED_AGENTS)
            .bind(&json)
            .bind(job_id as i64)
            .execute(&self.pool)
            .await
        {
            error!("Failed to record failed agent for job {}: {}", job_id, e);
        }
    }

    /// Get a single pending job.
    pub async fn get_job(&self, job_id: u64) -> Option<(u64, PendingJobInfo)> {
        match sqlx::query(sql::SELECT_PENDING_JOB)
            .bind(job_id as i64)
            .fetch_optional(&self.pool)
            .await
        {
            Ok(Some(row)) => self.row_to_pending_job(row),
            Ok(None) => None,
            Err(e) => {
                error!("Failed to load pending job {}: {}", job_id, e);
                None
            }
        }
    }

    /// Get the current retry count for a pending job.
    pub async fn get_retry_count(&self, job_id: u64) -> Option<i32> {
        let result = sqlx::query(sql::GET_PENDING_JOB_RETRY_COUNT)
            .bind(job_id as i64)
            .fetch_optional(&self.pool)
            .await;

        match result {
            Ok(Some(row)) => row.try_get("retry_count").ok(),
            Ok(None) => None,
            Err(e) => {
                error!("Failed to get retry count for job {}: {}", job_id, e);
                None
            }
        }
    }

    /// Check if a job exists in the pending store.
    pub async fn has_job(&self, job_id: u64) -> bool {
        let result = sqlx::query(sql::SELECT_PENDING_JOB)
            .bind(job_id as i64)
            .fetch_optional(&self.pool)
            .await;

        matches!(result, Ok(Some(_)))
    }

    /// Get all pending jobs (for startup recovery and processing).
    ///
    /// Returns jobs ordered by created_at (oldest first - FIFO).
    pub async fn get_all_pending_jobs(&self) -> Result<Vec<(u64, PendingJobInfo)>, sqlx::Error> {
        let rows = sqlx::query(sql::SELECT_ALL_PENDING_JOBS)
            .fetch_all(&self.pool)
            .await?;

        Ok(rows
            .into_iter()
            .filter_map(|row| self.row_to_pending_job(row))
            .collect())
    }

    /// Convert pending jobs to WebhookRunnerRequests for processing.
    pub async fn get_pending_requests(&self) -> Vec<WebhookRunnerRequest> {
        match self.get_all_pending_jobs().await {
            Ok(jobs) => jobs
                .into_iter()
                .map(|(job_id, info)| WebhookRunnerRequest {
                    job_labels: info.job_labels,
                    agent_labels: info.agent_labels,
                    runner_scope: info.runner_scope,
                    runner_group: info.runner_group,
                    job_id,
                    repository: info.repository,
                    job_name: info.job_name,
                    workflow_name: info.workflow_name,
                })
                .collect(),
            Err(e) => {
                error!("Failed to get pending requests: {}", e);
                Vec::new()
            }
        }
    }

    /// Convert a database row to a (job_id, PendingJobInfo) tuple.
    fn row_to_pending_job(&self, row: crate::db::DbRow) -> Option<(u64, PendingJobInfo)> {
        let job_id: i64 = row.try_get("job_id").ok()?;
        let job_labels_json: String = row.try_get("job_labels").ok()?;
        let agent_labels_json: String = row.try_get("agent_labels").ok()?;
        let scope_json: String = row.try_get("runner_scope").ok()?;
        let runner_group: Option<String> = row.try_get("runner_group").ok()?;
        let created_at_str: String = row.try_get("created_at").ok()?;
        let retry_count: i32 = row.try_get("retry_count").unwrap_or(0);
        let repository: Option<String> = row.try_get("repository").ok().flatten();
        let job_name: Option<String> = row.try_get("job_name").ok().flatten();
        let workflow_name: Option<String> = row.try_get("workflow_name").ok().flatten();
        let failed_agents: Vec<String> = row
            .try_get::<Option<String>, _>("failed_agents")
            .ok()
            .flatten()
            .and_then(|j| serde_json::from_str(&j).ok())
            .unwrap_or_default();

        let job_labels: Vec<String> = match serde_json::from_str(&job_labels_json) {
            Ok(l) => l,
            Err(e) => {
                warn!("Failed to deserialize job_labels for job {}: {}", job_id, e);
                return None;
            }
        };

        let agent_labels: Vec<String> = match serde_json::from_str(&agent_labels_json) {
            Ok(l) => l,
            Err(e) => {
                warn!(
                    "Failed to deserialize agent_labels for job {}: {}",
                    job_id, e
                );
                return None;
            }
        };

        let runner_scope: RunnerScope = match serde_json::from_str(&scope_json) {
            Ok(s) => s,
            Err(e) => {
                warn!(
                    "Failed to deserialize runner_scope for job {}: {}",
                    job_id, e
                );
                return None;
            }
        };

        let created_at = match DateTime::parse_from_rfc3339(&created_at_str) {
            Ok(dt) => dt.with_timezone(&Utc),
            Err(e) => {
                warn!("Failed to parse created_at for job {}: {}", job_id, e);
                return None;
            }
        };

        Some((
            job_id as u64,
            PendingJobInfo {
                job_labels,
                agent_labels,
                runner_scope,
                runner_group,
                created_at,
                retry_count,
                repository,
                job_name,
                workflow_name,
                failed_agents,
            },
        ))
    }
}

#[cfg(all(test, feature = "sqlite"))]
mod tests {
    use super::*;
    use crate::config::DatabaseConfig;
    use crate::db::Database;
    use tempfile::TempDir;

    async fn store() -> (TempDir, PendingJobStore) {
        let temp = TempDir::new().unwrap();
        let db = Database::new(&DatabaseConfig::default(), temp.path())
            .await
            .unwrap();
        (temp, PendingJobStore::new(db.pool()))
    }

    fn request(job_id: u64) -> WebhookRunnerRequest {
        WebhookRunnerRequest {
            job_labels: vec!["self-hosted".into(), "macos".into()],
            agent_labels: vec!["self-hosted".into(), "macos".into()],
            runner_scope: RunnerScope::Organization { name: "org".into() },
            runner_group: None,
            job_id,
            repository: Some("org/repo".into()),
            job_name: None,
            workflow_name: None,
        }
    }

    #[tokio::test]
    async fn test_retry_records_failed_agents_once() {
        let (_temp, store) = store().await;
        assert!(store.add_job(&request(1)).await);

        assert_eq!(store.increment_retry_count(1, Some("a")).await, Some(1));
        assert_eq!(store.increment_retry_count(1, None).await, Some(2));
        assert_eq!(store.increment_retry_count(1, Some("a")).await, Some(3));
        assert_eq!(store.increment_retry_count(1, Some("b")).await, Some(4));

        let (_, info) = store.get_job(1).await.unwrap();
        assert_eq!(info.retry_count, 4);
        assert_eq!(info.failed_agents, vec!["a".to_string(), "b".to_string()]);

        // fresh job has none
        assert!(store.add_job(&request(2)).await);
        let (_, info) = store.get_job(2).await.unwrap();
        assert!(info.failed_agents.is_empty());

        // unknown job is a no-op
        assert_eq!(store.increment_retry_count(99, Some("a")).await, None);
    }
}
