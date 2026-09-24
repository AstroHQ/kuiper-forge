//! API tokens for the HTTP API, managed from the admin UI.

use crate::db::{DbPool, DbRow};
use crate::sql;
use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rand::Rng;
use rand::distr::Alphanumeric;
use sha2::{Digest, Sha256};
use sqlx::Row;

const TOKEN_PREFIX: &str = "kfapi_";

/// How many chars of the token (after `TOKEN_PREFIX`) are kept in the clear for display.
const DISPLAY_CHARS: usize = 6;

/// API token record. The token itself is never stored, only its hash.
#[derive(Debug, Clone)]
pub struct ApiToken {
    pub id: String,
    pub name: String,
    /// Start of the token, e.g. `kfapi_a1b2c3`, so tokens can be told apart in the UI
    pub token_prefix: String,
    pub created_by: String,
    pub created_at: DateTime<Utc>,
    pub last_used_at: Option<DateTime<Utc>>,
}

/// Database-backed storage for API tokens.
pub struct ApiTokenStore {
    pool: DbPool,
}

impl ApiTokenStore {
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    // tokens are long random strings, so a plain sha256 is enough and keeps per-request checks cheap
    fn hash_token(token: &str) -> String {
        hex::encode(Sha256::digest(token.as_bytes()))
    }

    /// Create a token. Returns the plaintext token, which can't be recovered later.
    pub async fn create(&self, name: &str, created_by: &str) -> Result<(String, ApiToken)> {
        let secret: String = rand::rng()
            .sample_iter(&Alphanumeric)
            .take(40)
            .map(char::from)
            .collect();
        let token = format!("{TOKEN_PREFIX}{secret}");
        let record = ApiToken {
            id: uuid::Uuid::new_v4().to_string(),
            name: name.to_string(),
            token_prefix: token[..TOKEN_PREFIX.len() + DISPLAY_CHARS].to_string(),
            created_by: created_by.to_string(),
            created_at: Utc::now(),
            last_used_at: None,
        };

        sqlx::query(sql::INSERT_API_TOKEN)
            .bind(&record.id)
            .bind(&record.name)
            .bind(Self::hash_token(&token))
            .bind(&record.token_prefix)
            .bind(&record.created_by)
            .bind(record.created_at.to_rfc3339())
            .execute(&self.pool)
            .await
            .context("Failed to create API token")?;

        Ok((token, record))
    }

    /// List all tokens, newest first.
    pub async fn list(&self) -> Result<Vec<ApiToken>> {
        let rows = sqlx::query(sql::SELECT_ALL_API_TOKENS)
            .fetch_all(&self.pool)
            .await
            .context("Failed to list API tokens")?;

        Ok(rows.iter().filter_map(row_to_token).collect())
    }

    /// Delete a token by id. Returns false if it didn't exist.
    pub async fn delete(&self, id: &str) -> Result<bool> {
        let result = sqlx::query(sql::DELETE_API_TOKEN)
            .bind(id)
            .execute(&self.pool)
            .await
            .context("Failed to delete API token")?;

        Ok(result.rows_affected() > 0)
    }

    /// Look up a presented token and bump its last-used time. Returns None if it isn't valid.
    pub async fn validate(&self, token: &str) -> Result<Option<ApiToken>> {
        if !token.starts_with(TOKEN_PREFIX) {
            return Ok(None);
        }

        let row = sqlx::query(sql::SELECT_API_TOKEN_BY_HASH)
            .bind(Self::hash_token(token))
            .fetch_optional(&self.pool)
            .await
            .context("Failed to query API token")?;
        let Some(record) = row.as_ref().and_then(row_to_token) else {
            return Ok(None);
        };

        // best effort, a failed bump shouldn't fail the request
        sqlx::query(sql::UPDATE_API_TOKEN_LAST_USED)
            .bind(Utc::now().to_rfc3339())
            .bind(&record.id)
            .execute(&self.pool)
            .await
            .ok();

        Ok(Some(record))
    }
}

fn row_to_token(row: &DbRow) -> Option<ApiToken> {
    Some(ApiToken {
        id: row.get("id"),
        name: row.get("name"),
        token_prefix: row.get("token_prefix"),
        created_by: row.get("created_by"),
        created_at: DateTime::parse_from_rfc3339(row.get("created_at"))
            .ok()?
            .with_timezone(&Utc),
        last_used_at: row
            .get::<Option<String>, _>("last_used_at")
            .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
            .map(|dt| dt.with_timezone(&Utc)),
    })
}

#[cfg(all(test, feature = "sqlite"))]
mod tests {
    use super::*;
    use crate::config::DatabaseConfig;
    use crate::db::Database;
    use tempfile::TempDir;

    async fn store() -> (TempDir, ApiTokenStore) {
        let temp = TempDir::new().unwrap();
        let db = Database::new(&DatabaseConfig::default(), temp.path())
            .await
            .unwrap();
        (temp, ApiTokenStore::new(db.pool()))
    }

    #[tokio::test]
    async fn test_token_lifecycle() {
        let (_temp, store) = store().await;
        let (token, record) = store.create("grafana", "alice").await.unwrap();
        assert!(token.starts_with(&record.token_prefix));

        let found = store.validate(&token).await.unwrap().unwrap();
        assert_eq!(found.id, record.id);
        let listed = store.list().await.unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].last_used_at.is_some());

        assert!(store.validate("kfapi_nope").await.unwrap().is_none());
        assert!(store.validate("").await.unwrap().is_none());

        assert!(store.delete(&record.id).await.unwrap());
        assert!(store.validate(&token).await.unwrap().is_none());
        assert!(!store.delete(&record.id).await.unwrap());
    }
}
