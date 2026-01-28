//! GitHub API client for runner management.
//!
//! Handles:
//! - GitHub App authentication (JWT generation)
//! - Installation access token retrieval (with auto-discovery)
//! - Runner registration token generation

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use chrono::{Duration, Utc};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::{debug, info};

use crate::config::RunnerScope;

/// Trait for obtaining runner registration tokens and managing runners.
///
/// This abstracts the token source so we can use either:
/// - Real GitHub API (production)
/// - Mock tokens (dry-run/testing)
#[async_trait]
pub trait RunnerTokenProvider: Send + Sync {
    /// Get a registration token for the given runner scope.
    async fn get_registration_token(&self, scope: &RunnerScope) -> Result<String>;

    /// Remove a runner from GitHub by name.
    /// This is called when a runner VM is destroyed or agent disconnects.
    async fn remove_runner(&self, scope: &RunnerScope, runner_name: &str) -> Result<()>;
}

/// Mock token provider for dry-run/testing mode.
///
/// Returns fake tokens that won't work with GitHub but allow testing
/// the full agent VM lifecycle.
pub struct MockTokenProvider;

#[async_trait]
impl RunnerTokenProvider for MockTokenProvider {
    async fn get_registration_token(&self, scope: &RunnerScope) -> Result<String> {
        let fake_token = format!("dry-run-token-{}", &uuid::Uuid::new_v4().to_string()[..8]);
        info!(
            "DRY-RUN: Generated fake registration token for {:?}: {}",
            scope, fake_token
        );
        Ok(fake_token)
    }

    async fn remove_runner(&self, scope: &RunnerScope, runner_name: &str) -> Result<()> {
        info!(
            "DRY-RUN: Would remove runner '{}' from {:?}",
            runner_name, scope
        );
        Ok(())
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

/// Response from runners list endpoint
#[derive(Debug, Deserialize)]
struct RunnersListResponse {
    runners: Vec<RunnerInfo>,
}

/// Individual runner info from GitHub API
#[derive(Debug, Deserialize)]
struct RunnerInfo {
    id: u64,
    name: String,
    #[allow(dead_code)]
    os: String,
    #[allow(dead_code)]
    status: String,
    #[allow(dead_code)]
    busy: bool,
}

/// Installation info from GitHub API
#[derive(Debug, Deserialize)]
struct Installation {
    id: u64,
    account: InstallationAccount,
}

/// Account info for an installation
#[derive(Debug, Deserialize)]
struct InstallationAccount {
    login: String,
}

/// GitHub API client for managing runners
pub struct GitHubClient {
    /// GitHub App ID
    app_id: u64,

    /// GitHub App private key (PEM)
    private_key: String,

    /// HTTP client
    http_client: Client,

    /// Cached installation access tokens (keyed by installation_id)
    cached_tokens: Arc<RwLock<HashMap<u64, CachedToken>>>,

    /// Cached mapping of account name -> installation_id
    installation_ids: Arc<RwLock<HashMap<String, u64>>>,
}

impl GitHubClient {
    /// Create a new GitHub client from configuration
    pub fn new(app_id: u64, private_key_path: &Path) -> Result<Self> {
        let private_key = std::fs::read_to_string(private_key_path).with_context(|| {
            format!(
                "Failed to read GitHub App private key: {}",
                private_key_path.display()
            )
        })?;

        Self::from_key(app_id, private_key)
    }

    /// Create a new GitHub client from a private key string
    pub fn from_key(app_id: u64, private_key: String) -> Result<Self> {
        let http_client = Client::builder()
            .user_agent("kuiper-forge")
            .build()
            .context("Failed to create HTTP client")?;

        Ok(Self {
            app_id,
            private_key,
            http_client,
            cached_tokens: Arc::new(RwLock::new(HashMap::new())),
            installation_ids: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    /// Validate GitHub API access and discover installations.
    ///
    /// This should be called on startup to verify:
    /// - The private key is valid and can sign JWTs
    /// - The GitHub API is accessible
    /// - The app has at least one installation
    ///
    /// Returns a list of account names where the app is installed.
    pub async fn validate(&self) -> Result<Vec<String>> {
        info!("Validating GitHub App credentials...");

        // Test JWT generation (validates private key)
        let _jwt = self.generate_jwt().context(
            "Failed to generate JWT. Check that the private key is valid and matches the App ID.",
        )?;
        debug!("JWT generation successful");

        // Discover installations (validates API access and app configuration)
        self.discover_installations().await.context(
            "Failed to access GitHub API. Check your network connection and App credentials.",
        )?;

        // Get the discovered accounts
        let cache = self.installation_ids.read().await;
        let accounts: Vec<String> = cache.keys().cloned().collect();

        if accounts.is_empty() {
            return Err(anyhow!(
                "GitHub App has no installations. \
                 Install the app on at least one organization or repository."
            ));
        }

        info!(
            "GitHub API access validated. App installed on: {}",
            accounts.join(", ")
        );

        Ok(accounts)
    }

    /// Generate a JWT for GitHub App authentication
    fn generate_jwt(&self) -> Result<String> {
        let now = Utc::now();
        let claims = AppJwtClaims {
            // GitHub recommends 60 seconds in the past to avoid clock drift
            iat: (now - Duration::seconds(60)).timestamp(),
            // Max expiration is 10 minutes
            exp: (now + Duration::minutes(9)).timestamp(),
            iss: self.app_id.to_string(),
        };

        let key = EncodingKey::from_rsa_pem(self.private_key.as_bytes())
            .context("Failed to parse GitHub App private key")?;

        let header = Header::new(Algorithm::RS256);

        encode(&header, &claims, &key).context("Failed to generate JWT")
    }

    /// Discover all installations for this GitHub App and cache them.
    async fn discover_installations(&self) -> Result<()> {
        let jwt = self.generate_jwt()?;

        let url = format!("{GITHUB_API_URL}/app/installations");

        let response = self
            .http_client
            .get(&url)
            .header("Authorization", format!("Bearer {jwt}"))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await
            .context("Failed to list GitHub App installations")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!("GitHub API error ({}): {}", status, body));
        }

        let installations: Vec<Installation> = response
            .json()
            .await
            .context("Failed to parse installations response")?;

        let mut cache = self.installation_ids.write().await;
        for installation in installations {
            let account_name = installation.account.login.to_lowercase();
            debug!(
                "Discovered installation {} for account '{}'",
                installation.id, account_name
            );
            cache.insert(account_name, installation.id);
        }

        info!("Discovered {} GitHub App installation(s)", cache.len());
        Ok(())
    }

    /// Get the installation ID for a given account (org or user).
    /// Discovers installations if not cached.
    async fn get_installation_id(&self, account: &str) -> Result<u64> {
        let account_lower = account.to_lowercase();

        // Check cache first
        {
            let cache = self.installation_ids.read().await;
            if let Some(&id) = cache.get(&account_lower) {
                return Ok(id);
            }
        }

        // Not in cache, discover installations
        self.discover_installations().await?;

        // Check again after discovery
        let cache = self.installation_ids.read().await;
        cache.get(&account_lower).copied().ok_or_else(|| {
            anyhow!(
                "GitHub App is not installed on account '{}'. \
                 Please install the app at https://github.com/apps/YOUR-APP-NAME",
                account
            )
        })
    }

    /// Get the account name from a runner scope
    fn scope_account(scope: &RunnerScope) -> &str {
        match scope {
            RunnerScope::Organization { name } => name,
            RunnerScope::Repository { owner, .. } => owner,
        }
    }

    /// Get or refresh the installation access token for a given installation.
    async fn get_access_token(&self, installation_id: u64) -> Result<String> {
        // Check if we have a valid cached token
        {
            let cached = self.cached_tokens.read().await;
            if let Some(token) = cached.get(&installation_id)
                && token.is_valid()
            {
                return Ok(token.token.clone());
            }
        }

        // Need to get a new token
        let jwt = self.generate_jwt()?;

        let url = format!("{GITHUB_API_URL}/app/installations/{installation_id}/access_tokens");

        let response = self
            .http_client
            .post(&url)
            .header("Authorization", format!("Bearer {jwt}"))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await
            .context("Failed to request installation access token")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!("GitHub API error ({}): {}", status, body));
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
            let mut cached = self.cached_tokens.write().await;
            cached.insert(
                installation_id,
                CachedToken {
                    token: token_response.token.clone(),
                    expires_at,
                },
            );
        }

        Ok(token_response.token)
    }

    /// Get a runner registration token for the given scope
    pub async fn get_registration_token(&self, scope: &RunnerScope) -> Result<String> {
        // Get installation ID for this scope (auto-discovers if needed)
        let account = Self::scope_account(scope);
        let installation_id = self.get_installation_id(account).await?;

        // Get access token for this installation
        let access_token = self.get_access_token(installation_id).await?;

        let url = format!("{}{}", GITHUB_API_URL, scope.registration_token_path());

        let response = self
            .http_client
            .post(&url)
            .header("Authorization", format!("Bearer {access_token}"))
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

    /// List all runners for the given scope
    async fn list_runners(&self, scope: &RunnerScope) -> Result<Vec<RunnerInfo>> {
        let account = Self::scope_account(scope);
        let installation_id = self.get_installation_id(account).await?;
        let access_token = self.get_access_token(installation_id).await?;

        let url = format!("{}{}", GITHUB_API_URL, scope.runners_list_path());

        let response = self
            .http_client
            .get(&url)
            .header("Authorization", format!("Bearer {access_token}"))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await
            .context("Failed to list runners")?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "GitHub API error listing runners ({}): {}",
                status,
                body
            ));
        }

        let runners_response: RunnersListResponse = response
            .json()
            .await
            .context("Failed to parse runners list response")?;

        Ok(runners_response.runners)
    }

    /// Delete a runner by its ID
    async fn delete_runner(&self, scope: &RunnerScope, runner_id: u64) -> Result<()> {
        let account = Self::scope_account(scope);
        let installation_id = self.get_installation_id(account).await?;
        let access_token = self.get_access_token(installation_id).await?;

        let url = format!("{}{}", GITHUB_API_URL, scope.runner_delete_path(runner_id));

        let response = self
            .http_client
            .delete(&url)
            .header("Authorization", format!("Bearer {access_token}"))
            .header("Accept", "application/vnd.github+json")
            .header("X-GitHub-Api-Version", "2022-11-28")
            .send()
            .await
            .context("Failed to delete runner")?;

        // 204 No Content is the success response
        if response.status().as_u16() == 204 {
            return Ok(());
        }

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(anyhow!(
                "GitHub API error deleting runner ({}): {}",
                status,
                body
            ));
        }

        Ok(())
    }

    /// Remove a runner by name (looks up the runner ID first)
    pub async fn remove_runner(&self, scope: &RunnerScope, runner_name: &str) -> Result<()> {
        info!("Removing runner '{}' from {:?}", runner_name, scope);

        // List runners to find the one with this name
        let runners = self.list_runners(scope).await?;

        let runner = runners.iter().find(|r| r.name == runner_name);

        match runner {
            Some(r) => {
                debug!(
                    "Found runner '{}' with ID {}, deleting...",
                    runner_name, r.id
                );
                self.delete_runner(scope, r.id).await?;
                info!(
                    "Successfully removed runner '{}' (ID {})",
                    runner_name, r.id
                );
                Ok(())
            }
            None => {
                // Runner not found - this could happen if:
                // - Runner was never registered (dry-run mode)
                // - Runner already removed itself
                // - Runner name doesn't match
                info!(
                    "Runner '{}' not found in {:?} (may have already been removed)",
                    runner_name, scope
                );
                Ok(())
            }
        }
    }
}

#[async_trait]
impl RunnerTokenProvider for GitHubClient {
    async fn get_registration_token(&self, scope: &RunnerScope) -> Result<String> {
        // Delegate to the inherent method
        GitHubClient::get_registration_token(self, scope).await
    }

    async fn remove_runner(&self, scope: &RunnerScope, runner_name: &str) -> Result<()> {
        // Delegate to the inherent method
        GitHubClient::remove_runner(self, scope, runner_name).await
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

    #[test]
    fn test_scope_account() {
        let org = RunnerScope::Organization {
            name: "MyOrg".to_string(),
        };
        assert_eq!(GitHubClient::scope_account(&org), "MyOrg");

        let repo = RunnerScope::Repository {
            owner: "Owner".to_string(),
            repo: "Repo".to_string(),
        };
        assert_eq!(GitHubClient::scope_account(&repo), "Owner");
    }
}
