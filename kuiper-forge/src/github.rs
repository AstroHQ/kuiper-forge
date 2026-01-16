//! GitHub API client for runner management.
//!
//! Handles:
//! - GitHub App authentication (JWT generation)
//! - Installation access token retrieval
//! - Runner registration token generation

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use chrono::{Duration, Utc};
use jsonwebtoken::{encode, Algorithm, EncodingKey, Header};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::info;

use crate::config::RunnerScope;

/// Trait for obtaining runner registration tokens.
///
/// This abstracts the token source so we can use either:
/// - Real GitHub API (production)
/// - Mock tokens (dry-run/testing)
#[async_trait]
pub trait RunnerTokenProvider: Send + Sync {
    /// Get a registration token for the given runner scope.
    async fn get_registration_token(&self, scope: &RunnerScope) -> Result<String>;
}

/// Mock token provider for dry-run/testing mode.
///
/// Returns fake tokens that won't work with GitHub but allow testing
/// the full agent VM lifecycle.
pub struct MockTokenProvider;

#[async_trait]
impl RunnerTokenProvider for MockTokenProvider {
    async fn get_registration_token(&self, scope: &RunnerScope) -> Result<String> {
        let fake_token = format!("dry-run-token-{}", uuid::Uuid::new_v4().to_string()[..8].to_string());
        info!(
            "DRY-RUN: Generated fake registration token for {:?}: {}",
            scope, fake_token
        );
        Ok(fake_token)
    }
}

/// GitHub API base URL
const GITHUB_API_URL: &str = "https://api.github.com";

/// Cached access token with expiration tracking
#[derive(Debug, Clone)]
struct CachedToken {
    token: String,
    expires_at: chrono::DateTime<Utc>,
}

impl CachedToken {
    /// Check if the token is still valid (with a 5-minute buffer)
    fn is_valid(&self) -> bool {
        self.expires_at > Utc::now() + Duration::minutes(5)
    }
}

/// JWT claims for GitHub App authentication
#[derive(Debug, Serialize)]
struct AppJwtClaims {
    /// Issued at time
    iat: i64,
    /// Expiration time (max 10 minutes)
    exp: i64,
    /// GitHub App ID (issuer)
    iss: String,
}

/// Response from installation access token endpoint
#[derive(Debug, Deserialize)]
struct InstallationTokenResponse {
    token: String,
    expires_at: String,
}

/// Response from runner registration token endpoint
#[derive(Debug, Deserialize)]
struct RegistrationTokenResponse {
    token: String,
    #[allow(dead_code)] // Present in API response but not used
    expires_at: String,
}

/// GitHub API client for managing runners
pub struct GitHubClient {
    /// GitHub App ID
    app_id: String,

    /// GitHub App private key (PEM)
    private_key: String,

    /// Installation ID for this GitHub App installation
    installation_id: String,

    /// HTTP client
    http_client: Client,

    /// Cached installation access token
    cached_token: Arc<RwLock<Option<CachedToken>>>,
}

impl GitHubClient {
    /// Create a new GitHub client from configuration
    pub fn new(app_id: String, private_key_path: &Path, installation_id: String) -> Result<Self> {
        let private_key = std::fs::read_to_string(private_key_path).with_context(|| {
            format!(
                "Failed to read GitHub App private key: {}",
                private_key_path.display()
            )
        })?;

        Self::from_key(app_id, private_key, installation_id)
    }

    /// Create a new GitHub client from a private key string
    pub fn from_key(app_id: String, private_key: String, installation_id: String) -> Result<Self> {
        let http_client = Client::builder()
            .user_agent("kuiper-forge")
            .build()
            .context("Failed to create HTTP client")?;

        Ok(Self {
            app_id,
            private_key,
            installation_id,
            http_client,
            cached_token: Arc::new(RwLock::new(None)),
        })
    }

    /// Generate a JWT for GitHub App authentication
    fn generate_jwt(&self) -> Result<String> {
        let now = Utc::now();
        let claims = AppJwtClaims {
            // GitHub recommends 60 seconds in the past to avoid clock drift
            iat: (now - Duration::seconds(60)).timestamp(),
            // Max expiration is 10 minutes
            exp: (now + Duration::minutes(9)).timestamp(),
            iss: self.app_id.clone(),
        };

        let key = EncodingKey::from_rsa_pem(self.private_key.as_bytes())
            .context("Failed to parse GitHub App private key")?;

        let header = Header::new(Algorithm::RS256);

        encode(&header, &claims, &key).context("Failed to generate JWT")
    }

    /// Get or refresh the installation access token
    pub async fn get_access_token(&self) -> Result<String> {
        // Check if we have a valid cached token
        {
            let cached = self.cached_token.read().await;
            if let Some(token) = cached.as_ref() {
                if token.is_valid() {
                    return Ok(token.token.clone());
                }
            }
        }

        // Need to get a new token
        let jwt = self.generate_jwt()?;

        let url = format!(
            "{}/app/installations/{}/access_tokens",
            GITHUB_API_URL, self.installation_id
        );

        let response = self
            .http_client
            .post(&url)
            .header("Authorization", format!("Bearer {}", jwt))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await
            .context("Failed to request installation access token")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "GitHub API error ({}): {}",
                status,
                body
            ));
        }

        let token_response: InstallationTokenResponse = response
            .json()
            .await
            .context("Failed to parse installation access token response")?;

        let expires_at = chrono::DateTime::parse_from_rfc3339(&token_response.expires_at)
            .context("Failed to parse token expiration")?
            .with_timezone(&Utc);

        // Cache the new token
        {
            let mut cached = self.cached_token.write().await;
            *cached = Some(CachedToken {
                token: token_response.token.clone(),
                expires_at,
            });
        }

        Ok(token_response.token)
    }

    /// Get a runner registration token for the given scope
    pub async fn get_registration_token(&self, scope: &RunnerScope) -> Result<String> {
        let access_token = self.get_access_token().await?;

        let url = format!("{}{}", GITHUB_API_URL, scope.registration_token_path());

        let response = self
            .http_client
            .post(&url)
            .header("Authorization", format!("Bearer {}", access_token))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await
            .context("Failed to request runner registration token")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "GitHub API error getting registration token ({}): {}",
                status,
                body
            ));
        }

        let token_response: RegistrationTokenResponse = response
            .json()
            .await
            .context("Failed to parse registration token response")?;

        Ok(token_response.token)
    }
}

#[async_trait]
impl RunnerTokenProvider for GitHubClient {
    async fn get_registration_token(&self, scope: &RunnerScope) -> Result<String> {
        // Delegate to the inherent method
        GitHubClient::get_registration_token(self, scope).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_registration_token_path() {
        let org_scope = RunnerScope::Organization {
            name: "my-org".to_string(),
        };
        assert_eq!(
            org_scope.registration_token_path(),
            "/orgs/my-org/actions/runners/registration-token"
        );

        let repo_scope = RunnerScope::Repository {
            owner: "owner".to_string(),
            repo: "repo".to_string(),
        };
        assert_eq!(
            repo_scope.registration_token_path(),
            "/repos/owner/repo/actions/runners/registration-token"
        );
    }

    #[test]
    fn test_cached_token_validity() {
        let valid_token = CachedToken {
            token: "test".to_string(),
            expires_at: Utc::now() + Duration::hours(1),
        };
        assert!(valid_token.is_valid());

        let expired_token = CachedToken {
            token: "test".to_string(),
            expires_at: Utc::now() - Duration::hours(1),
        };
        assert!(!expired_token.is_valid());

        // Token expiring in less than 5 minutes should be considered invalid
        let almost_expired = CachedToken {
            token: "test".to_string(),
            expires_at: Utc::now() + Duration::minutes(4),
        };
        assert!(!almost_expired.is_valid());
    }
}
