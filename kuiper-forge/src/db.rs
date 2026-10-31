//! Database management for the coordinator.
//!
//! Provides a shared database connection pool that can be used by multiple
//! components (auth store, runner state, etc.).
//!
//! The database backend is selected at compile time via feature flags:
//! - `sqlite` (default): Uses SQLite
//! - `postgres`: Uses PostgreSQL

use crate::config::DatabaseConfig;
use anyhow::{Context, Result};
use std::path::Path;
use tracing::info;

#[cfg(feature = "sqlite")]
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
#[cfg(feature = "sqlite")]
use std::str::FromStr;

#[cfg(feature = "postgres")]
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};

// Re-export the backend, pool and row types for the selected backend
#[cfg(feature = "sqlite")]
pub use sqlx::{Sqlite as Db, SqlitePool as DbPool, sqlite::SqliteRow as DbRow};

#[cfg(feature = "postgres")]
pub use sqlx::{PgPool as DbPool, Postgres as Db, postgres::PgRow as DbRow};

/// Shared database for the coordinator.
///
/// This struct owns the database connection pool and handles migrations.
/// It should be created once at startup and shared across components
/// via `Arc<Database>`.
pub struct Database {
    pool: DbPool,
}

impl Database {
    /// Create a new database connection based on configuration.
    ///
    /// This will:
    /// - Connect to the database (creating SQLite file if needed)
    /// - Run all pending migrations
    /// - Return a connection pool ready for use
    #[cfg(feature = "sqlite")]
    pub async fn new(config: &DatabaseConfig, data_dir: &Path) -> Result<Self> {
        use std::fs;

        let db_path = config
            .path
            .clone()
            .unwrap_or_else(|| data_dir.join("coordinator.db"));

        // Create parent directory if needed
        if let Some(parent) = db_path.parent() {
            fs::create_dir_all(parent)?;
        }

        // Configure SQLite connection
        let options = SqliteConnectOptions::from_str(&format!("sqlite:{}", db_path.display()))?
            .create_if_missing(true);

        // Create connection pool
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .context("Failed to connect to SQLite database")?;

        // Run migrations. both sets record into the same _sqlx_migrations table, so each has to ignore the
        // other's versions
        sqlx::migrate!("./migrations/shared")
            .set_ignore_missing(true)
            .run(&pool)
            .await
            .context("Failed to run migrations")?;
        sqlx::migrate!("./migrations/sqlite")
            .set_ignore_missing(true)
            .run(&pool)
            .await
            .context("Failed to run sqlite migrations")?;

        info!(backend = "sqlite", path = %db_path.display(), "Database connected");

        Ok(Self { pool })
    }

    /// Create a new database connection based on configuration.
    #[cfg(feature = "postgres")]
    pub async fn new(config: &DatabaseConfig, _data_dir: &Path) -> Result<Self> {
        // Configure PostgreSQL connection
        let options = PgConnectOptions::new()
            .host(&config.host)
            .port(config.port)
            .username(&config.user)
            .password(&config.password)
            .database(&config.database);

        // Create connection pool
        let pool = PgPoolOptions::new()
            .max_connections(10)
            .connect_with(options)
            .await
            .context("Failed to connect to PostgreSQL database")?;

        // both sets record into the same _sqlx_migrations table, so each has to ignore the other's versions
        sqlx::migrate!("./migrations/shared")
            .set_ignore_missing(true)
            .run(&pool)
            .await
            .context("Failed to run migrations")?;
        sqlx::migrate!("./migrations/postgres")
            .set_ignore_missing(true)
            .run(&pool)
            .await
            .context("Failed to run postgres migrations")?;

        info!(
            backend = "postgres",
            host = %config.host,
            port = config.port,
            user = %config.user,
            database = %config.database,
            "Database connected"
        );

        Ok(Self { pool })
    }

    /// Get a clone of the connection pool.
    ///
    /// Use this to pass the pool to components that need database access.
    /// The pool is cheap to clone (internally Arc-based).
    pub fn pool(&self) -> DbPool {
        self.pool.clone()
    }
}

#[cfg(all(test, feature = "sqlite"))]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_sqlite_connection() {
        let temp = TempDir::new().unwrap();
        let config = DatabaseConfig::default();
        let db = Database::new(&config, temp.path()).await.unwrap();

        // Just verify we can get a pool
        let _pool = db.pool();
    }

    /// The occurred_at index survives the sqlite rebuild of agent_failures on a fresh database
    #[tokio::test]
    async fn test_agent_failures_occurred_at_index_exists() {
        let temp = TempDir::new().unwrap();
        let db = Database::new(&DatabaseConfig::default(), temp.path())
            .await
            .unwrap();
        let found: Option<(String,)> = sqlx::query_as(
            "SELECT name FROM sqlite_master WHERE type = 'index' AND name = 'idx_agent_failures_occurred_at'",
        )
        .fetch_optional(&db.pool())
        .await
        .unwrap();
        assert!(found.is_some());
    }

    /// A database created before job keys: numeric github job ids get the "github:" prefix and
    /// runners stay readable, with no forge recorded.
    #[tokio::test]
    async fn test_job_key_migration_keeps_existing_rows() {
        use crate::agent_failures::AgentFailureStore;
        use crate::forge::JobKey;
        use crate::pending_jobs::PendingJobStore;
        use crate::runner_state::RunnerStateStore;

        let temp = TempDir::new().unwrap();
        let options = SqliteConnectOptions::from_str(&format!(
            "sqlite:{}",
            temp.path().join("coordinator.db").display()
        ))
        .unwrap()
        .create_if_missing(true);
        let old = SqlitePoolOptions::new()
            .connect_with(options)
            .await
            .unwrap();

        // schema and rows as of the last release
        sqlx::migrate!("./migrations/shared")
            .run(&old)
            .await
            .unwrap();
        let scope = r#"{"type":"organization","name":"org"}"#;
        sqlx::query(
            "INSERT INTO pending_webhook_jobs (job_id, job_labels, agent_labels, runner_scope, created_at, repository) \
             VALUES (30000000001, '[\"self-hosted\"]', '[\"self-hosted\"]', ?, '2026-10-01T00:00:00+00:00', 'org/repo')",
        )
        .bind(scope)
        .execute(&old)
        .await
        .unwrap();
        for (name, job_id) in [("r-job", Some(30000000001i64)), ("r-pool", None)] {
            sqlx::query(
                "INSERT INTO active_runners (runner_name, agent_id, vm_name, runner_scope, created_at, job_id) \
                 VALUES (?, 'agent', ?, ?, '2026-10-01T00:00:00+00:00', ?)",
            )
            .bind(name)
            .bind(name)
            .bind(scope)
            .bind(job_id)
            .execute(&old)
            .await
            .unwrap();
        }
        sqlx::query(
            "INSERT INTO agent_failures (id, agent_id, occurred_at, kind, job_id, message) \
             VALUES ('f1', 'agent', '2026-10-01T00:00:00.000000Z', 'runner_failed', 30000000001, 'boom')",
        )
        .execute(&old)
        .await
        .unwrap();
        old.close().await;

        // opening it the normal way runs the new migrations, and opening it again is a no-op
        drop(
            Database::new(&DatabaseConfig::default(), temp.path())
                .await
                .unwrap(),
        );
        let pool = Database::new(&DatabaseConfig::default(), temp.path())
            .await
            .unwrap()
            .pool();

        let key = JobKey::new("github", "30000000001");
        let (stored_key, info) = PendingJobStore::new(pool.clone())
            .get_job(&key)
            .await
            .expect("pending job survives");
        assert_eq!(stored_key, key);
        assert_eq!(info.repository.as_deref(), Some("org/repo"));

        let runners = RunnerStateStore::new(pool.clone());
        let job_runner = runners
            .get_runner("r-job")
            .await
            .expect("job runner survives");
        assert_eq!(job_runner.job_id, Some(key.clone()));
        assert_eq!(job_runner.forge, None);
        let pool_runner = runners
            .get_runner("r-pool")
            .await
            .expect("pool runner survives");
        assert_eq!(pool_runner.job_id, None);
        assert_eq!(runners.runners_for_job(&key).await.len(), 1);

        let recent = AgentFailureStore::new(pool)
            .recent("agent", 10)
            .await
            .unwrap();
        assert_eq!(recent[0].job_id, Some(key));
    }
}
