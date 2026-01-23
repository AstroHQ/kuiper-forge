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

// Re-export the pool and row types for the selected backend
#[cfg(feature = "sqlite")]
pub use sqlx::{sqlite::SqliteRow as DbRow, SqlitePool as DbPool};

#[cfg(feature = "postgres")]
pub use sqlx::{postgres::PgRow as DbRow, PgPool as DbPool};

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

        // Run migrations
        sqlx::migrate!("./migrations/shared")
            .run(&pool)
            .await
            .context("Failed to run migrations")?;

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

        // Run migrations
        sqlx::migrate!("./migrations/shared")
            .run(&pool)
            .await
            .context("Failed to run migrations")?;

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
}
