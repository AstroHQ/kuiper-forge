//! Run history for the metrics page: when each runner and job reached each step, and how it ended.
//!
//! Writes are best effort. A failed write is logged and never holds up provisioning.

mod sql;
pub mod summary;

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use kuiper_agent_proto::RunnerTimings;
use sqlx::Row;
use tracing::warn;

use crate::db::{DbPool, DbRow};
use crate::forge::JobKey;

/// How a runner attempt ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunOutcome {
    /// The agent reported the runner finished
    Completed,
    /// The agent reported a failure (clone, boot, runner setup, ...)
    Failed,
    /// It went away before finishing: agent offline, VM missing, destroyed mid-job
    Lost,
    /// The agent refused the CreateRunner
    Rejected,
    /// No usable answer to the CreateRunner: timeout, send error, unexpected reply
    CommandFailed,
    /// We destroyed it because its job never started
    Idled,
    /// We destroyed it to drain an unavailable agent
    Drained,
    /// We destroyed a pool runner that had no job to finish
    Destroyed,
}

impl RunOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            RunOutcome::Completed => "completed",
            RunOutcome::Failed => "failed",
            RunOutcome::Lost => "lost",
            RunOutcome::Rejected => "rejected",
            RunOutcome::CommandFailed => "command_failed",
            RunOutcome::Idled => "idled",
            RunOutcome::Drained => "drained",
            RunOutcome::Destroyed => "destroyed",
        }
    }
}

/// Why a job left the pending queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobOutcome {
    /// A runner ran it, or the forge says it finished
    Ran,
    /// Too many runners failed it
    Abandoned,
    /// The forge stopped listing it as queued: someone else took it, it was deleted, or it's too old to check
    Gone,
}

impl JobOutcome {
    fn as_str(self) -> &'static str {
        match self {
            JobOutcome::Ran => "ran",
            JobOutcome::Abandoned => "abandoned",
            JobOutcome::Gone => "gone",
        }
    }
}

/// A runner about to be requested from an agent.
pub struct NewRunner<'a> {
    pub runner_name: &'a str,
    pub agent_id: &'a str,
    pub hostname: &'a str,
    pub agent_type: &'a str,
    pub forge: Option<&'a str>,
    pub job_id: Option<&'a JobKey>,
    pub repository: Option<&'a str>,
    pub workflow_name: Option<&'a str>,
    pub job_name: Option<&'a str>,
    pub pool: Option<&'a str>,
    pub labels: &'a [String],
}

/// A job just added to the pending queue.
pub struct NewJob<'a> {
    pub job_id: &'a JobKey,
    pub repository: Option<&'a str>,
    pub workflow_name: Option<&'a str>,
    pub job_name: Option<&'a str>,
    pub labels: &'a [String],
    /// The forge's own created time, if it gave one
    pub queued_at: Option<DateTime<Utc>>,
}

/// One runner attempt, as stored. Times are unix millis.
#[derive(Debug, Clone, Default)]
pub struct RunnerRun {
    pub runner_name: String,
    pub agent_id: String,
    pub hostname: String,
    pub agent_type: String,
    pub forge: Option<String>,
    pub job_id: Option<String>,
    pub repository: Option<String>,
    pub workflow_name: Option<String>,
    pub job_name: Option<String>,
    pub pool: Option<String>,
    pub labels: Vec<String>,
    pub requested_at: i64,
    pub acked_at: Option<i64>,
    pub started_at: Option<i64>,
    pub job_started_at: Option<i64>,
    pub finished_at: Option<i64>,
    /// `RunOutcome::as_str`, None while it's still running
    pub outcome: Option<String>,
    pub error: Option<String>,
    pub image: Option<String>,
    pub failed_phase: Option<String>,
    /// The agent's step timings in millis (create, boot, install, setup, job, destroy, ...). A missing step didn't
    /// run or wasn't reported, 0 means it was skipped
    pub steps: BTreeMap<String, i64>,
}

/// One job, as stored. Times are unix millis.
#[derive(Debug, Clone, Default)]
pub struct JobRun {
    pub job_id: String,
    pub forge: String,
    pub repository: Option<String>,
    pub workflow_name: Option<String>,
    pub job_name: Option<String>,
    pub labels: Vec<String>,
    pub queued_at: i64,
    pub seen_at: i64,
    pub first_runner_at: Option<i64>,
    pub started_at: Option<i64>,
    pub completed_at: Option<i64>,
    pub conclusion: Option<String>,
    pub runner_name: Option<String>,
    pub attempts: i64,
    /// ran, abandoned, gone or cancelled. None while it's pending
    pub outcome: Option<String>,
}

/// Database-backed run history.
pub struct MetricsStore {
    pool: DbPool,
}

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

fn labels_json(labels: &[String]) -> String {
    serde_json::to_string(labels).unwrap_or_else(|_| "[]".to_string())
}

fn steps_json<K: serde::Serialize + Ord>(steps: &BTreeMap<K, i64>) -> String {
    serde_json::to_string(steps).unwrap_or_else(|_| "{}".to_string())
}

fn log_err<T>(what: &str, key: &str, result: Result<T, sqlx::Error>) {
    if let Err(e) = result {
        warn!("Failed to record {what} for {key} in the run history: {e}");
    }
}

impl MetricsStore {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    pub async fn runner_requested(&self, run: NewRunner<'_>) {
        let result = sqlx::query(sql::INSERT_RUNNER_RUN)
            .bind(run.runner_name)
            .bind(run.agent_id)
            .bind(run.hostname)
            .bind(run.agent_type)
            .bind(run.forge)
            .bind(run.job_id.map(JobKey::to_string))
            .bind(run.repository)
            .bind(run.workflow_name)
            .bind(run.job_name)
            .bind(run.pool)
            .bind(labels_json(run.labels))
            .bind(now_ms())
            .execute(&self.pool)
            .await;
        log_err("runner request", run.runner_name, result);
    }

    pub async fn runner_acked(&self, runner_name: &str) {
        self.set_time(sql::SET_RUNNER_ACKED, "runner ack", runner_name, now_ms())
            .await;
    }

    pub async fn runner_started(&self, runner_name: &str) {
        self.set_time(
            sql::SET_RUNNER_STARTED,
            "runner start",
            runner_name,
            now_ms(),
        )
        .await;
    }

    /// The forge says the runner took its job.
    pub async fn runner_job_started(&self, runner_name: &str, at: DateTime<Utc>) {
        self.set_time(
            sql::SET_RUNNER_JOB_STARTED,
            "runner job start",
            runner_name,
            at.timestamp_millis(),
        )
        .await;
    }

    async fn set_time(&self, query: &'static str, what: &str, key: &str, at: i64) {
        let result = sqlx::query(query)
            .bind(at)
            .bind(key)
            .execute(&self.pool)
            .await;
        log_err(what, key, result);
    }

    /// Record how a runner ended. Only the first call counts: the agent can send more than one final event (a
    /// Failed for the killed job, then a Destroyed), and the first one says what really happened.
    pub async fn runner_finished(&self, runner_name: &str, outcome: RunOutcome, error: &str) {
        let error = (!error.is_empty()).then_some(error);
        let result = sqlx::query(sql::FINISH_RUNNER_RUN)
            .bind(now_ms())
            .bind(outcome.as_str())
            .bind(error)
            .bind(runner_name)
            .execute(&self.pool)
            .await;
        log_err("runner finish", runner_name, result);
    }

    /// Fill in the agent's step timings. Applies after the runner finished too, the event carrying them can come
    /// after the one that finished it.
    pub async fn runner_timings(&self, runner_name: &str, timings: &RunnerTimings) {
        let text = |s: &str| (!s.is_empty()).then(|| s.to_string());
        let steps: BTreeMap<&str, i64> = timings
            .steps_ms
            .iter()
            .map(|(step, ms)| (step.as_str(), (*ms).min(i64::MAX as u64) as i64))
            .collect();
        let result = sqlx::query(sql::SET_RUNNER_TIMINGS)
            .bind(text(&timings.image))
            .bind(text(&timings.failed_phase))
            .bind(steps_json(&steps))
            .bind(runner_name)
            .execute(&self.pool)
            .await;
        log_err("runner timings", runner_name, result);
    }

    pub async fn job_seen(&self, job: NewJob<'_>) {
        let seen = now_ms();
        let key = job.job_id.to_string();
        let result = sqlx::query(sql::INSERT_JOB_RUN)
            .bind(&key)
            .bind(job.job_id.forge())
            .bind(job.repository)
            .bind(job.workflow_name)
            .bind(job.job_name)
            .bind(labels_json(job.labels))
            // a forge clock a bit ahead of ours mustn't make the wait negative
            .bind(
                job.queued_at
                    .map_or(seen, |t| t.timestamp_millis().min(seen)),
            )
            .bind(seen)
            .execute(&self.pool)
            .await;
        log_err("job", &key, result);
    }

    pub async fn job_runner_created(&self, job_id: &JobKey) {
        let key = job_id.to_string();
        let result = sqlx::query(sql::JOB_RUNNER_CREATED)
            .bind(now_ms())
            .bind(&key)
            .execute(&self.pool)
            .await;
        log_err("job runner", &key, result);
    }

    /// The forge says `runner_name` took the job.
    pub async fn job_started(&self, job_id: &JobKey, runner_name: Option<&str>, at: DateTime<Utc>) {
        let key = job_id.to_string();
        let result = sqlx::query(sql::SET_JOB_STARTED)
            .bind(at.timestamp_millis())
            .bind(runner_name)
            .bind(&key)
            .execute(&self.pool)
            .await;
        log_err("job start", &key, result);
    }

    pub async fn job_completed(
        &self,
        job_id: &JobKey,
        conclusion: Option<&str>,
        at: DateTime<Utc>,
    ) {
        let key = job_id.to_string();
        let outcome = if conclusion == Some("cancelled") {
            "cancelled"
        } else {
            JobOutcome::Ran.as_str()
        };
        let result = sqlx::query(sql::SET_JOB_COMPLETED)
            .bind(at.timestamp_millis())
            .bind(conclusion)
            .bind(outcome)
            .bind(&key)
            .execute(&self.pool)
            .await;
        log_err("job completion", &key, result);
    }

    pub async fn job_ended(&self, job_id: &JobKey, outcome: JobOutcome) {
        let key = job_id.to_string();
        let result = sqlx::query(sql::SET_JOB_OUTCOME)
            .bind(outcome.as_str())
            .bind(outcome.as_str())
            .bind(&key)
            .execute(&self.pool)
            .await;
        log_err("job outcome", &key, result);
    }

    /// Delete history older than `retention`. Returns the number of rows removed.
    pub async fn prune(&self, retention: chrono::Duration) -> Result<u64, sqlx::Error> {
        let cutoff = (Utc::now() - retention).timestamp_millis();
        let runs = sqlx::query(sql::DELETE_RUNNER_RUNS_BEFORE)
            .bind(cutoff)
            .execute(&self.pool)
            .await?
            .rows_affected();
        let jobs = sqlx::query(sql::DELETE_JOB_RUNS_BEFORE)
            .bind(cutoff)
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(runs + jobs)
    }

    /// Insert a whole runner row as given, for seeding mock history. Normal recording goes through the methods above.
    pub async fn insert_run(&self, run: &RunnerRun) -> Result<(), sqlx::Error> {
        sqlx::query(sql::INSERT_RUNNER_RUN_ROW)
            .bind(&run.runner_name)
            .bind(&run.agent_id)
            .bind(&run.hostname)
            .bind(&run.agent_type)
            .bind(&run.forge)
            .bind(&run.job_id)
            .bind(&run.repository)
            .bind(&run.workflow_name)
            .bind(&run.job_name)
            .bind(&run.pool)
            .bind(labels_json(&run.labels))
            .bind(run.requested_at)
            .bind(run.acked_at)
            .bind(run.started_at)
            .bind(run.job_started_at)
            .bind(run.finished_at)
            .bind(&run.outcome)
            .bind(&run.error)
            .bind(&run.image)
            .bind(&run.failed_phase)
            .bind((!run.steps.is_empty()).then(|| steps_json(&run.steps)))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Insert a whole job row as given, for seeding mock history.
    pub async fn insert_job(&self, job: &JobRun) -> Result<(), sqlx::Error> {
        sqlx::query(sql::INSERT_JOB_RUN_ROW)
            .bind(&job.job_id)
            .bind(&job.forge)
            .bind(&job.repository)
            .bind(&job.workflow_name)
            .bind(&job.job_name)
            .bind(labels_json(&job.labels))
            .bind(job.queued_at)
            .bind(job.seen_at)
            .bind(job.first_runner_at)
            .bind(job.started_at)
            .bind(job.completed_at)
            .bind(&job.conclusion)
            .bind(&job.runner_name)
            .bind(job.attempts as i32)
            .bind(&job.outcome)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Delete every runner and job row recorded under `forge`. Returns the number of rows removed.
    pub async fn delete_forge(&self, forge: &str) -> Result<u64, sqlx::Error> {
        let runs = sqlx::query(sql::DELETE_RUNNER_RUNS_FOR_FORGE)
            .bind(forge)
            .execute(&self.pool)
            .await?
            .rows_affected();
        let jobs = sqlx::query(sql::DELETE_JOB_RUNS_FOR_FORGE)
            .bind(forge)
            .execute(&self.pool)
            .await?
            .rows_affected();
        Ok(runs + jobs)
    }

    /// Runner attempts requested at or after `since` (unix millis), oldest first.
    pub async fn runs_since(&self, since: i64) -> Result<Vec<RunnerRun>, sqlx::Error> {
        let rows = sqlx::query(sql::SELECT_RUNNER_RUNS_SINCE)
            .bind(since)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(row_to_run).collect())
    }

    /// Jobs first seen at or after `since` (unix millis), oldest first.
    pub async fn jobs_since(&self, since: i64) -> Result<Vec<JobRun>, sqlx::Error> {
        let rows = sqlx::query(sql::SELECT_JOB_RUNS_SINCE)
            .bind(since)
            .fetch_all(&self.pool)
            .await?;
        Ok(rows.iter().map(row_to_job).collect())
    }
}

fn parse_labels(json: &str) -> Vec<String> {
    serde_json::from_str(json).unwrap_or_default()
}

fn row_to_run(row: &DbRow) -> RunnerRun {
    RunnerRun {
        runner_name: row.get("runner_name"),
        agent_id: row.get("agent_id"),
        hostname: row.get("hostname"),
        agent_type: row.get("agent_type"),
        forge: row.get("forge"),
        job_id: row.get("job_id"),
        repository: row.get("repository"),
        workflow_name: row.get("workflow_name"),
        job_name: row.get("job_name"),
        pool: row.get("pool"),
        labels: parse_labels(row.get("labels")),
        requested_at: row.get("requested_at"),
        acked_at: row.get("acked_at"),
        started_at: row.get("started_at"),
        job_started_at: row.get("job_started_at"),
        finished_at: row.get("finished_at"),
        outcome: row.get("outcome"),
        error: row.get("error"),
        image: row.get("image"),
        failed_phase: row.get("failed_phase"),
        steps: row
            .get::<Option<String>, _>("steps")
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default(),
    }
}

fn row_to_job(row: &DbRow) -> JobRun {
    JobRun {
        job_id: row.get("job_id"),
        forge: row.get("forge"),
        repository: row.get("repository"),
        workflow_name: row.get("workflow_name"),
        job_name: row.get("job_name"),
        labels: parse_labels(row.get("labels")),
        queued_at: row.get("queued_at"),
        seen_at: row.get("seen_at"),
        first_runner_at: row.get("first_runner_at"),
        started_at: row.get("started_at"),
        completed_at: row.get("completed_at"),
        conclusion: row.get("conclusion"),
        runner_name: row.get("runner_name"),
        attempts: i64::from(row.get::<i32, _>("attempts")),
        outcome: row.get("outcome"),
    }
}

#[cfg(all(test, feature = "sqlite"))]
mod tests {
    use super::*;
    use crate::config::DatabaseConfig;
    use crate::db::Database;
    use tempfile::TempDir;

    async fn store() -> (TempDir, MetricsStore) {
        let temp = TempDir::new().unwrap();
        let db = Database::new(&DatabaseConfig::default(), temp.path())
            .await
            .unwrap();
        (temp, MetricsStore::new(db.pool()))
    }

    fn new_runner<'a>(
        name: &'a str,
        job: Option<&'a JobKey>,
        labels: &'a [String],
    ) -> NewRunner<'a> {
        NewRunner {
            runner_name: name,
            agent_id: "a1",
            hostname: "mac-1",
            agent_type: "tart",
            forge: Some("github"),
            job_id: job,
            repository: Some("o/r"),
            workflow_name: Some("ci"),
            job_name: Some("build"),
            pool: None,
            labels,
        }
    }

    #[tokio::test]
    async fn test_runner_lifecycle() {
        let (_temp, store) = store().await;
        let job = JobKey::new("github", "42");
        let labels = vec!["macos".to_string()];
        store
            .runner_requested(new_runner("r1", Some(&job), &labels))
            .await;
        store.runner_acked("r1").await;
        store.runner_started("r1").await;
        store
            .runner_timings(
                "r1",
                &RunnerTimings {
                    steps_ms: [
                        ("create", 1_000),
                        ("boot", 20_000),
                        ("install", 0),
                        ("setup", 3_000),
                    ]
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v))
                    .collect(),
                    image: "macos-15".into(),
                    ..Default::default()
                },
            )
            .await;
        store
            .runner_finished("r1", RunOutcome::Failed, "boom")
            .await;

        // later events: a second finish is ignored, timings still fill in
        store.runner_finished("r1", RunOutcome::Lost, "").await;
        store
            .runner_timings(
                "r1",
                &RunnerTimings {
                    steps_ms: [("setup".to_string(), 3_100), ("destroy".to_string(), 500)].into(),
                    failed_phase: "job".into(),
                    ..Default::default()
                },
            )
            .await;

        let runs = store.runs_since(0).await.unwrap();
        assert_eq!(runs.len(), 1);
        let r = &runs[0];
        assert_eq!(r.job_id.as_deref(), Some("github:42"));
        assert_eq!(r.labels, labels);
        assert!(r.acked_at.is_some() && r.started_at.is_some() && r.finished_at.is_some());
        assert_eq!(r.outcome.as_deref(), Some("failed"));
        assert_eq!(r.error.as_deref(), Some("boom"));
        assert_eq!(r.image.as_deref(), Some("macos-15"));
        assert_eq!(r.failed_phase.as_deref(), Some("job"));
        // merged: the later event's steps added or replaced, the rest kept
        let steps: Vec<(&str, i64)> = r.steps.iter().map(|(k, v)| (k.as_str(), *v)).collect();
        assert_eq!(
            steps,
            [
                ("boot", 20_000),
                ("create", 1_000),
                ("destroy", 500),
                ("install", 0),
                ("setup", 3_100)
            ]
        );
    }

    #[tokio::test]
    async fn test_job_outcomes() {
        let (_temp, store) = store().await;
        let job = JobKey::new("github", "7");
        let labels = vec!["linux".to_string()];
        let queued = Utc::now() - chrono::Duration::minutes(2);
        store
            .job_seen(NewJob {
                job_id: &job,
                repository: Some("o/r"),
                workflow_name: None,
                job_name: None,
                labels: &labels,
                queued_at: Some(queued),
            })
            .await;
        store.job_runner_created(&job).await;
        store.job_runner_created(&job).await;

        // dropped as gone first, then it turns out our runner ran it
        store.job_ended(&job, JobOutcome::Gone).await;
        store.job_ended(&job, JobOutcome::Ran).await;
        store.job_ended(&job, JobOutcome::Abandoned).await;

        let jobs = store.jobs_since(0).await.unwrap();
        assert_eq!(jobs.len(), 1);
        let j = &jobs[0];
        assert_eq!(j.queued_at, queued.timestamp_millis());
        assert_eq!(j.attempts, 2);
        assert!(j.first_runner_at.is_some());
        assert_eq!(j.outcome.as_deref(), Some("ran"));
    }

    #[tokio::test]
    async fn test_cancelled_job() {
        let (_temp, store) = store().await;
        let job = JobKey::new("github", "8");
        store
            .job_seen(NewJob {
                job_id: &job,
                repository: None,
                workflow_name: None,
                job_name: None,
                labels: &[],
                queued_at: None,
            })
            .await;
        store
            .job_completed(&job, Some("cancelled"), Utc::now())
            .await;
        store.job_ended(&job, JobOutcome::Ran).await;

        let j = &store.jobs_since(0).await.unwrap()[0];
        assert_eq!(j.queued_at, j.seen_at);
        assert_eq!(j.conclusion.as_deref(), Some("cancelled"));
        assert_eq!(j.outcome.as_deref(), Some("cancelled"));
    }

    #[tokio::test]
    async fn test_prune() {
        let (_temp, store) = store().await;
        store.runner_requested(new_runner("r1", None, &[])).await;
        assert_eq!(store.prune(chrono::Duration::days(1)).await.unwrap(), 0);
        assert_eq!(
            store
                .prune(chrono::Duration::milliseconds(-1000))
                .await
                .unwrap(),
            1
        );
        assert!(store.runs_since(0).await.unwrap().is_empty());
    }
}
