//! Agents' own log lines, uploaded over the `UploadLogs` rpc and shown in the admin UI.
//!
//! Older agents never upload, so they just have no rows here.

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use kuiper_agent_proto::LogRecord;
use sqlx::{QueryBuilder, Row};

use crate::db::{Db, DbPool};
use crate::sql;

/// Max lines accepted in one upload. Agents send far fewer, see `kuiper_agent_lib::log_upload`.
pub const MAX_BATCH: usize = 2_000;

const MAX_MESSAGE_BYTES: usize = 16 * 1024;
const MAX_TARGET_BYTES: usize = 256;

/// Lines older than this are pruned.
pub const RETENTION_DAYS: i64 = 7;

/// Newest lines kept per agent, so a chatty agent can't fill the database.
pub const MAX_LINES_PER_AGENT: i64 = 100_000;

/// Log level, stored as a number so filtering by minimum level is a plain comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Error = 1,
    Warn = 2,
    Info = 3,
    Debug = 4,
    Trace = 5,
}

impl LogLevel {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_uppercase().as_str() {
            "ERROR" => Some(Self::Error),
            "WARN" => Some(Self::Warn),
            "INFO" => Some(Self::Info),
            "DEBUG" => Some(Self::Debug),
            "TRACE" => Some(Self::Trace),
            _ => None,
        }
    }

    fn from_i16(n: i16) -> Self {
        match n {
            1 => Self::Error,
            2 => Self::Warn,
            3 => Self::Info,
            4 => Self::Debug,
            _ => Self::Trace,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Error => "ERROR",
            Self::Warn => "WARN",
            Self::Info => "INFO",
            Self::Debug => "DEBUG",
            Self::Trace => "TRACE",
        }
    }
}

/// A stored log line.
#[derive(Debug, Clone)]
pub struct AgentLogLine {
    pub ts: DateTime<Utc>,
    pub level: LogLevel,
    pub target: String,
    pub message: String,
    /// Opaque paging cursor: pass as `before` to get the lines older than this one
    pub cursor: String,
}

/// Database-backed store for agent log lines.
pub struct AgentLogStore {
    pool: DbPool,
}

impl AgentLogStore {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    /// Store an uploaded batch. `dropped` becomes a warning line so gaps are visible in the UI.
    pub async fn insert_batch(
        &self,
        agent_id: &str,
        records: &[LogRecord],
        dropped: u64,
    ) -> Result<()> {
        let mut rows: Vec<(i64, i64, i16, String, String)> = Vec::with_capacity(records.len() + 1);
        if dropped > 0 {
            // just before the first line of this batch, which is where the gap is
            let ts = records
                .first()
                .map(|r| r.timestamp_micros - 1)
                .unwrap_or_else(|| Utc::now().timestamp_micros());
            rows.push((
                ts,
                0,
                LogLevel::Warn as i16,
                "kuiper_forge".to_string(),
                format!("agent dropped {dropped} log line(s) because its upload buffer was full"),
            ));
        }
        for r in records {
            rows.push((
                r.timestamp_micros,
                r.seq as i64,
                LogLevel::parse(&r.level).unwrap_or(LogLevel::Info) as i16,
                truncated(&r.target, MAX_TARGET_BYTES),
                truncated(&r.message, MAX_MESSAGE_BYTES),
            ));
        }
        if rows.is_empty() {
            return Ok(());
        }

        let mut query: QueryBuilder<Db> = QueryBuilder::new(
            "INSERT INTO agent_logs (agent_id, ts, seq, level, target, message) ",
        );
        query.push_values(rows, |mut b, (ts, seq, level, target, message)| {
            b.push_bind(agent_id)
                .push_bind(ts)
                .push_bind(seq)
                .push_bind(level)
                .push_bind(target)
                .push_bind(message);
        });
        query
            .build()
            .execute(&self.pool)
            .await
            .context("Failed to insert agent logs")?;

        Ok(())
    }

    /// Newest lines at or above `min_level`, older than `before` when given. Newest first.
    pub async fn recent(
        &self,
        agent_id: &str,
        min_level: LogLevel,
        before: Option<&str>,
        limit: i64,
    ) -> Result<Vec<AgentLogLine>> {
        let (before_ts, before_seq) = before
            .and_then(parse_cursor)
            .unwrap_or((i64::MAX, i64::MAX));

        let rows = sqlx::query(sql::SELECT_AGENT_LOGS)
            .bind(agent_id)
            .bind(min_level as i16)
            .bind(before_ts)
            .bind(before_ts)
            .bind(before_seq)
            .bind(limit)
            .fetch_all(&self.pool)
            .await
            .context("Failed to query agent logs")?;

        Ok(rows
            .into_iter()
            .map(|row| {
                let ts: i64 = row.get("ts");
                let seq: i64 = row.get("seq");
                AgentLogLine {
                    ts: DateTime::from_timestamp_micros(ts).unwrap_or_default(),
                    level: LogLevel::from_i16(row.get("level")),
                    target: row.get("target"),
                    message: row.get("message"),
                    cursor: format!("{ts}_{seq}"),
                }
            })
            .collect())
    }

    /// Drop lines past the retention window, then trim each agent down to its line cap.
    pub async fn prune(&self) -> Result<u64> {
        let cutoff = (Utc::now() - chrono::Duration::days(RETENTION_DAYS)).timestamp_micros();
        let mut removed = sqlx::query(sql::DELETE_AGENT_LOGS_OLDER_THAN)
            .bind(cutoff)
            .execute(&self.pool)
            .await
            .context("Failed to prune old agent logs")?
            .rows_affected();

        let agents: Vec<String> = sqlx::query(sql::SELECT_AGENT_LOG_AGENTS)
            .fetch_all(&self.pool)
            .await
            .context("Failed to list agents with logs")?
            .into_iter()
            .map(|row| row.get("agent_id"))
            .collect();
        for agent_id in agents {
            let cutoff: Option<i64> = sqlx::query(sql::SELECT_AGENT_LOG_CUTOFF)
                .bind(&agent_id)
                .bind(MAX_LINES_PER_AGENT)
                .fetch_optional(&self.pool)
                .await
                .context("Failed to find agent log cutoff")?
                .map(|row| row.get("ts"));
            if let Some(cutoff) = cutoff {
                removed += sqlx::query(sql::DELETE_AGENT_LOGS_BEFORE)
                    .bind(&agent_id)
                    .bind(cutoff)
                    .execute(&self.pool)
                    .await
                    .context("Failed to trim agent logs")?
                    .rows_affected();
            }
        }

        Ok(removed)
    }
}

fn parse_cursor(cursor: &str) -> Option<(i64, i64)> {
    let (ts, seq) = cursor.split_once('_')?;
    Some((ts.parse().ok()?, seq.parse().ok()?))
}

fn truncated(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut cut = max;
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{} …[truncated]", &s[..cut])
}

#[cfg(all(test, feature = "sqlite"))]
mod tests {
    use super::*;
    use crate::config::DatabaseConfig;
    use crate::db::Database;
    use tempfile::TempDir;

    async fn store() -> (TempDir, AgentLogStore) {
        let temp = TempDir::new().unwrap();
        let db = Database::new(&DatabaseConfig::default(), temp.path())
            .await
            .unwrap();
        (temp, AgentLogStore::new(db.pool()))
    }

    fn record(ts: i64, seq: u64, level: &str, message: &str) -> LogRecord {
        LogRecord {
            timestamp_micros: ts,
            level: level.to_string(),
            target: "agent".to_string(),
            message: message.to_string(),
            seq,
        }
    }

    fn messages(lines: &[AgentLogLine]) -> Vec<&str> {
        lines.iter().map(|l| l.message.as_str()).collect()
    }

    #[tokio::test]
    async fn test_insert_query_and_page() {
        let (_temp, store) = store().await;
        let now = Utc::now().timestamp_micros();

        // two lines share a timestamp, seq decides their order
        let batch = vec![
            record(now, 0, "INFO", "a"),
            record(now, 1, "WARN", "b"),
            record(now + 1, 2, "ERROR", "c"),
            record(now + 2, 3, "DEBUG", "d"),
        ];
        store.insert_batch("a1", &batch, 0).await.unwrap();
        store
            .insert_batch("a2", &[record(now, 0, "INFO", "other")], 0)
            .await
            .unwrap();

        let all = store.recent("a1", LogLevel::Trace, None, 10).await.unwrap();
        assert_eq!(messages(&all), vec!["d", "c", "b", "a"]);

        let warn_up = store.recent("a1", LogLevel::Warn, None, 10).await.unwrap();
        assert_eq!(messages(&warn_up), vec!["c", "b"]);

        let page1 = store.recent("a1", LogLevel::Trace, None, 2).await.unwrap();
        let page2 = store
            .recent("a1", LogLevel::Trace, Some(&page1[1].cursor), 2)
            .await
            .unwrap();
        assert_eq!(messages(&page2), vec!["b", "a"]);
    }

    #[tokio::test]
    async fn test_dropped_marker_sits_before_batch() {
        let (_temp, store) = store().await;
        let now = Utc::now().timestamp_micros();
        store
            .insert_batch("a1", &[record(now, 7, "INFO", "after gap")], 42)
            .await
            .unwrap();

        let lines = store.recent("a1", LogLevel::Trace, None, 10).await.unwrap();
        assert_eq!(lines[0].message, "after gap");
        assert!(lines[1].message.contains("dropped 42"));
        assert_eq!(lines[1].level, LogLevel::Warn);
    }

    #[tokio::test]
    async fn test_prune_by_age() {
        let (_temp, store) = store().await;
        let old = (Utc::now() - chrono::Duration::days(RETENTION_DAYS + 1)).timestamp_micros();
        let now = Utc::now().timestamp_micros();
        store
            .insert_batch(
                "a1",
                &[record(old, 0, "INFO", "old"), record(now, 1, "INFO", "new")],
                0,
            )
            .await
            .unwrap();

        assert_eq!(store.prune().await.unwrap(), 1);
        let lines = store.recent("a1", LogLevel::Trace, None, 10).await.unwrap();
        assert_eq!(messages(&lines), vec!["new"]);
    }

    #[test]
    fn test_bad_cursor_is_ignored() {
        assert_eq!(parse_cursor("12_3"), Some((12, 3)));
        assert_eq!(parse_cursor("nope"), None);
        assert_eq!(parse_cursor("1_x"), None);
    }
}
