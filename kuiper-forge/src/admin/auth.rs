//! Admin authentication: users and sessions.
//!
//! Handles password hashing (argon2), session creation/validation, and
//! admin user management.

use crate::db::DbPool;
use crate::sql;
use anyhow::{Context, Result, anyhow};
use argon2::{
    Argon2,
    password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString, rand_core::OsRng},
};
use chrono::{DateTime, Duration, Utc};
use rand::Rng;
use rand::distr::Alphanumeric;
use sqlx::Row;

/// Admin user record
#[derive(Debug, Clone)]
pub struct AdminUser {
    pub username: String,
    pub password_hash: String,
    pub totp_secret: Option<String>,
    pub created_at: DateTime<Utc>,
    pub last_login: Option<DateTime<Utc>>,
}

/// Admin session record
#[derive(Debug, Clone)]
pub struct AdminSession {
    pub session_id: String,
    pub username: String,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub ip_address: Option<String>,
    pub user_agent: Option<String>,
}

/// Database-backed storage for admin users and sessions.
pub struct AdminAuthStore {
    pool: DbPool,
}

impl AdminAuthStore {
    /// Create a new AdminAuthStore using the given database pool.
    pub fn new(pool: DbPool) -> Self {
        Self { pool }
    }

    /// Hash a password using Argon2id.
    pub fn hash_password(password: &str) -> Result<String> {
        let salt = SaltString::generate(&mut OsRng);
        let argon2 = Argon2::default();
        let hash = argon2
            .hash_password(password.as_bytes(), &salt)
            .map_err(|e| anyhow!("Failed to hash password: {e}"))?;
        Ok(hash.to_string())
    }

    /// Verify a password against a stored hash.
    pub fn verify_password(password: &str, hash: &str) -> bool {
        let parsed_hash = match PasswordHash::new(hash) {
            Ok(h) => h,
            Err(_) => return false,
        };
        Argon2::default()
            .verify_password(password.as_bytes(), &parsed_hash)
            .is_ok()
    }

    /// Generate a cryptographically secure session ID.
    fn generate_session_id() -> String {
        rand::rng()
            .sample_iter(&Alphanumeric)
            .take(64)
            .map(char::from)
            .collect()
    }

    /// Create a new admin user.
    pub async fn create_user(&self, username: &str, password: &str) -> Result<()> {
        let password_hash = Self::hash_password(password)?;
        let now = Utc::now();

        sqlx::query(sql::INSERT_ADMIN_USER)
            .bind(username)
            .bind(&password_hash)
            .bind(None::<String>) // totp_secret
            .bind(now.to_rfc3339())
            .execute(&self.pool)
            .await
            .context("Failed to create admin user")?;

        Ok(())
    }

    /// Get an admin user by username.
    pub async fn get_user(&self, username: &str) -> Result<Option<AdminUser>> {
        let row = sqlx::query(sql::SELECT_ADMIN_USER)
            .bind(username)
            .fetch_optional(&self.pool)
            .await
            .context("Failed to query admin user")?;

        let user = match row {
            Some(row) => Some(AdminUser {
                username: row.get("username"),
                password_hash: row.get("password_hash"),
                totp_secret: row.get("totp_secret"),
                created_at: DateTime::parse_from_rfc3339(row.get("created_at"))
                    .context("Invalid created_at timestamp")?
                    .with_timezone(&Utc),
                last_login: row
                    .get::<Option<String>, _>("last_login")
                    .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                    .map(|dt| dt.with_timezone(&Utc)),
            }),
            None => None,
        };

        Ok(user)
    }

    /// List all admin users.
    pub async fn list_users(&self) -> Result<Vec<AdminUser>> {
        let rows = sqlx::query(sql::SELECT_ALL_ADMIN_USERS)
            .fetch_all(&self.pool)
            .await
            .context("Failed to list admin users")?;

        let users = rows
            .into_iter()
            .filter_map(|row| {
                Some(AdminUser {
                    username: row.get("username"),
                    password_hash: row.get("password_hash"),
                    totp_secret: row.get("totp_secret"),
                    created_at: DateTime::parse_from_rfc3339(row.get("created_at"))
                        .ok()?
                        .with_timezone(&Utc),
                    last_login: row
                        .get::<Option<String>, _>("last_login")
                        .and_then(|s| DateTime::parse_from_rfc3339(&s).ok())
                        .map(|dt| dt.with_timezone(&Utc)),
                })
            })
            .collect();

        Ok(users)
    }

    /// Update a user's password.
    pub async fn update_password(&self, username: &str, new_password: &str) -> Result<()> {
        let password_hash = Self::hash_password(new_password)?;

        let result = sqlx::query(sql::UPDATE_ADMIN_USER_PASSWORD)
            .bind(&password_hash)
            .bind(username)
            .execute(&self.pool)
            .await
            .context("Failed to update password")?;

        if result.rows_affected() == 0 {
            return Err(anyhow!("User not found: {username}"));
        }

        Ok(())
    }

    /// Delete an admin user.
    pub async fn delete_user(&self, username: &str) -> Result<()> {
        // First delete all sessions for this user
        sqlx::query(sql::DELETE_ADMIN_SESSIONS_BY_USER)
            .bind(username)
            .execute(&self.pool)
            .await
            .context("Failed to delete user sessions")?;

        // Then delete the user
        let result = sqlx::query(sql::DELETE_ADMIN_USER)
            .bind(username)
            .execute(&self.pool)
            .await
            .context("Failed to delete admin user")?;

        if result.rows_affected() == 0 {
            return Err(anyhow!("User not found: {username}"));
        }

        Ok(())
    }

    /// Authenticate a user and create a session.
    ///
    /// Returns the session ID if authentication succeeds, None otherwise.
    pub async fn authenticate(
        &self,
        username: &str,
        password: &str,
        session_timeout_secs: u64,
        ip_address: Option<String>,
        user_agent: Option<String>,
    ) -> Result<Option<String>> {
        // Get user
        let user = match self.get_user(username).await? {
            Some(u) => u,
            None => return Ok(None),
        };

        // Verify password
        if !Self::verify_password(password, &user.password_hash) {
            return Ok(None);
        }

        // Create session
        let session_id = Self::generate_session_id();
        let now = Utc::now();
        let expires_at = now + Duration::seconds(session_timeout_secs as i64);

        sqlx::query(sql::INSERT_ADMIN_SESSION)
            .bind(&session_id)
            .bind(username)
            .bind(now.to_rfc3339())
            .bind(expires_at.to_rfc3339())
            .bind(&ip_address)
            .bind(&user_agent)
            .execute(&self.pool)
            .await
            .context("Failed to create session")?;

        // Update last login
        sqlx::query(sql::UPDATE_ADMIN_USER_LAST_LOGIN)
            .bind(now.to_rfc3339())
            .bind(username)
            .execute(&self.pool)
            .await
            .ok(); // Don't fail if this doesn't work

        Ok(Some(session_id))
    }

    /// Validate a session and return the associated user.
    pub async fn validate_session(&self, session_id: &str) -> Result<Option<AdminSession>> {
        let row = sqlx::query(sql::SELECT_ADMIN_SESSION)
            .bind(session_id)
            .fetch_optional(&self.pool)
            .await
            .context("Failed to query session")?;

        let session = match row {
            Some(row) => {
                let expires_at = DateTime::parse_from_rfc3339(row.get("expires_at"))
                    .context("Invalid expires_at timestamp")?
                    .with_timezone(&Utc);

                // Check if session is expired
                if expires_at < Utc::now() {
                    // Delete expired session
                    self.delete_session(session_id).await.ok();
                    return Ok(None);
                }

                Some(AdminSession {
                    session_id: row.get("session_id"),
                    username: row.get("username"),
                    created_at: DateTime::parse_from_rfc3339(row.get("created_at"))
                        .context("Invalid created_at timestamp")?
                        .with_timezone(&Utc),
                    expires_at,
                    ip_address: row.get("ip_address"),
                    user_agent: row.get("user_agent"),
                })
            }
            None => None,
        };

        Ok(session)
    }

    /// Delete a session (logout).
    pub async fn delete_session(&self, session_id: &str) -> Result<()> {
        sqlx::query(sql::DELETE_ADMIN_SESSION)
            .bind(session_id)
            .execute(&self.pool)
            .await
            .context("Failed to delete session")?;

        Ok(())
    }

    /// Delete all expired sessions (background cleanup task).
    pub async fn cleanup_expired_sessions(&self) -> Result<u64> {
        let result = sqlx::query(sql::DELETE_EXPIRED_ADMIN_SESSIONS)
            .bind(Utc::now().to_rfc3339())
            .execute(&self.pool)
            .await
            .context("Failed to delete expired sessions")?;

        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_password_hashing() {
        let password = "test_password_123";
        let hash = AdminAuthStore::hash_password(password).unwrap();

        // Hash should be different from password
        assert_ne!(hash, password);

        // Should verify correctly
        assert!(AdminAuthStore::verify_password(password, &hash));

        // Wrong password should fail
        assert!(!AdminAuthStore::verify_password("wrong_password", &hash));
    }
}
