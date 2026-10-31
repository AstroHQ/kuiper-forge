//! Recent failures per agent, so the admin UI can show what went wrong without digging through logs.
//!
//! Only the newest `MAX_PER_AGENT` rows are kept for each agent.

use chrono::{DateTime, SecondsFormat, Utc};
use sqlx::Row;
use tracing::warn;

use crate::db::{DbPool, DbRow};
use crate::forge::JobKey;
use crate::sql;

const MAX_PER_AGENT: i64 = 100;

/// What went wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// Agent sent a `FAILED` runner event (VM clone/boot/runner config/etc.)
    RunnerFailed,
    /// Runner went away before finishing, e.g. the agent disconnected or the VM vanished
    RunnerLost,
    /// Agent refused a `CreateRunner` command
    CommandRejected,
    /// Couldn't get a usable answer to a command: timeout, send error, unexpected reply, or a legacy agent's
    /// failed result
    CommandFailed,
}

impl FailureKind {
    pub const ALL: [FailureKind; 4] = [
        FailureKind::RunnerFailed,
        FailureKind::RunnerLost,
        FailureKind::CommandRejected,
        FailureKind::CommandFailed,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            FailureKind::RunnerFailed => "runner_failed",
            FailureKind::RunnerLost => "runner_lost",
            FailureKind::CommandRejected => "command_rejected",
            FailureKind::CommandFailed => "command_failed",
        }
    }

    /// Human label for the admin UI.
    pub fn label(kind: &str) -> &'static str {
        match kind {
            "runner_failed" => "Runner failed",
            "runner_lost" => "Runner lost",
            "command_rejected" => "Rejected",
            "command_failed" => "Command failed",
            _ => "Other",
        }
    }
}

/// A recorded failure.
#[derive(Debug, Clone)]
pub struct AgentFailure {
    pub agent_id: String,
    pub occurred_at: DateTime<Utc>,
    /// `FailureKind::as_str` value, kept as a string so old rows survive new kinds
    pub kind: String,
    pub runner_name: Option<String>,
    pub job_id: Option<JobKey>,
    pub message: String,
}

/// Database-backed store for agent failures.
pub struct AgentFailureStore {
    pool: DbPool,
}

impl AgentFailureStore {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    /// Record a failure. Errors are logged, not returned, since this is only ever diagnostics.
    pub async fn record(
        &self,
        agent_id: &str,
        kind: FailureKind,
        runner_name: Option<&str>,
        job_id: Option<&JobKey>,
        message: &str,
    ) {
        if let Err(e) = self
            .try_record(agent_id, kind, runner_name, job_id, message)
            .await
        {
            warn!(agent_id = %agent_id, "Failed to record agent failure: {e}");
        }
    }

    async fn try_record(
        &self,
        agent_id: &str,
        kind: FailureKind,
        runner_name: Option<&str>,
        job_id: Option<&JobKey>,
        message: &str,
    ) -> Result<(), sqlx::Error> {
        // fixed-width timestamps so ORDER BY on the text column sorts correctly
        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Micros, true);

        sqlx::query(sql::INSERT_AGENT_FAILURE)
            .bind(uuid::Uuid::new_v4().to_string())
            .bind(agent_id)
            .bind(now)
            .bind(kind.as_str())
            .bind(runner_name)
            .bind(job_id.map(JobKey::to_string))
            .bind(message)
            .execute(&self.pool)
            .await?;

        sqlx::query(sql::PRUNE_AGENT_FAILURES)
            .bind(agent_id)
            .bind(agent_id)
            .bind(MAX_PER_AGENT)
            .execute(&self.pool)
            .await?;

        Ok(())
    }

    /// Newest failures for an agent, up to `limit`.
    pub async fn recent(
        &self,
        agent_id: &str,
        limit: i64,
    ) -> Result<Vec<AgentFailure>, sqlx::Error> {
        let rows = sqlx::query(sql::SELECT_AGENT_FAILURES)
            .bind(agent_id)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;

        Ok(rows.iter().filter_map(row_to_failure).collect())
    }

    /// Newest failures across all agents, up to `limit`. `kind` (a `FailureKind::as_str` value) and `agent_id`
    /// narrow it down when set.
    pub async fn recent_all(
        &self,
        kind: Option<&str>,
        agent_id: Option<&str>,
        limit: i64,
    ) -> Result<Vec<AgentFailure>, sqlx::Error> {
        let rows = sqlx::query(sql::SELECT_RECENT_FAILURES)
            .bind(kind)
            .bind(kind)
            .bind(agent_id)
            .bind(agent_id)
            .bind(limit)
            .fetch_all(&self.pool)
            .await?;

        Ok(rows.iter().filter_map(row_to_failure).collect())
    }

    /// Number of failures per `(agent_id, kind)` since `since`. Only the newest `MAX_PER_AGENT` per agent are
    /// kept, so a busy agent's count tops out there.
    pub async fn counts_since(
        &self,
        since: DateTime<Utc>,
    ) -> Result<Vec<(String, String, i64)>, sqlx::Error> {
        let rows = sqlx::query(sql::COUNT_FAILURES_SINCE)
            .bind(since.to_rfc3339_opts(SecondsFormat::Micros, true))
            .fetch_all(&self.pool)
            .await?;

        Ok(rows
            .iter()
            .map(|r| (r.get("agent_id"), r.get("kind"), r.get("n")))
            .collect())
    }
}

fn row_to_failure(row: &DbRow) -> Option<AgentFailure> {
    Some(AgentFailure {
        agent_id: row.get("agent_id"),
        occurred_at: DateTime::parse_from_rfc3339(row.get("occurred_at"))
            .ok()?
            .with_timezone(&Utc),
        kind: row.get("kind"),
        runner_name: row.get("runner_name"),
        job_id: row
            .get::<Option<String>, _>("job_id")
            .and_then(|k| k.parse().ok()),
        message: row.get("message"),
    })
}

#[cfg(all(test, feature = "sqlite"))]
mod tests {
    use super::*;
    use crate::config::DatabaseConfig;
    use crate::db::Database;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_record_and_prune() {
        let temp = TempDir::new().unwrap();
        let db = Database::new(&DatabaseConfig::default(), temp.path())
            .await
            .unwrap();
        let store = AgentFailureStore::new(db.pool());

        for i in 0..MAX_PER_AGENT + 5 {
            store
                .record(
                    "a1",
                    FailureKind::RunnerFailed,
                    Some("r"),
                    Some(&JobKey::new(
                        "github",
                        (30_000_000_000 + i as u64).to_string(),
                    )),
                    &format!("boom {i}"),
                )
                .await;
        }
        store
            .record("a2", FailureKind::CommandRejected, None, None, "full")
            .await;

        let a1 = store.recent("a1", 1000).await.unwrap();
        assert_eq!(a1.len() as i64, MAX_PER_AGENT);
        assert_eq!(a1[0].message, format!("boom {}", MAX_PER_AGENT + 4));

        // job keys must round-trip
        assert_eq!(
            a1[0].job_id,
            Some(JobKey::new(
                "github",
                (30_000_000_000 + MAX_PER_AGENT as u64 + 4).to_string()
            ))
        );

        let a2 = store.recent("a2", 10).await.unwrap();
        assert_eq!(a2.len(), 1);
        assert_eq!(a2[0].kind, "command_rejected");
        assert_eq!(a2[0].runner_name, None);
    }

    #[tokio::test]
    async fn test_recent_all_and_counts() {
        let temp = TempDir::new().unwrap();
        let db = Database::new(&DatabaseConfig::default(), temp.path())
            .await
            .unwrap();
        let store = AgentFailureStore::new(db.pool());
        let start = Utc::now();

        store
            .record("a1", FailureKind::RunnerFailed, Some("r1"), None, "one")
            .await;
        store
            .record("a2", FailureKind::RunnerLost, Some("r2"), None, "two")
            .await;
        store
            .record("a1", FailureKind::RunnerLost, Some("r3"), None, "three")
            .await;

        // newest first, across agents
        let all = store.recent_all(None, None, 10).await.unwrap();
        let messages: Vec<_> = all.iter().map(|f| f.message.as_str()).collect();
        assert_eq!(messages, ["three", "two", "one"]);
        assert_eq!(all[1].agent_id, "a2");

        assert_eq!(store.recent_all(None, None, 2).await.unwrap().len(), 2);

        let lost = store
            .recent_all(Some("runner_lost"), None, 10)
            .await
            .unwrap();
        let messages: Vec<_> = lost.iter().map(|f| f.message.as_str()).collect();
        assert_eq!(messages, ["three", "two"]);

        let a1_lost = store
            .recent_all(Some("runner_lost"), Some("a1"), 10)
            .await
            .unwrap();
        assert_eq!(a1_lost.len(), 1);
        assert_eq!(a1_lost[0].message, "three");

        let mut counts = store.counts_since(start).await.unwrap();
        counts.sort();
        assert_eq!(
            counts,
            [
                ("a1".to_owned(), "runner_failed".to_owned(), 1),
                ("a1".to_owned(), "runner_lost".to_owned(), 1),
                ("a2".to_owned(), "runner_lost".to_owned(), 1),
            ]
        );
        assert!(
            store
                .counts_since(Utc::now() + chrono::Duration::seconds(1))
                .await
                .unwrap()
                .is_empty()
        );
    }
}
