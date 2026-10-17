//! Forge abstraction: the CI host (GitHub, later Forgejo) runners register with.
//!
//! A coordinator can serve several forges at once. Each is configured under a
//! unique name, and every job and runner records which forge it belongs to:
//! jobs through their [`JobKey`], runners through their stored forge name.

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;
use std::str::FromStr;
use std::sync::Arc;
use tracing::info;

use crate::config::RunnerScope;

/// Name of the forge built from a legacy top-level `[github]` section. Job ids
/// stored before multi-forge support (bare numbers) belong to it.
pub const LEGACY_GITHUB_FORGE: &str = "github";

/// Identifies a job across forges: the forge's configured name plus the forge's
/// own id for the job. Stored and displayed as `forge:id`, like `github:123456`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct JobKey {
    forge: String,
    id: String,
}

impl JobKey {
    pub fn new(forge: impl Into<String>, id: impl Into<String>) -> Self {
        Self {
            forge: forge.into(),
            id: id.into(),
        }
    }

    /// The configured name of the forge the job is on.
    pub fn forge(&self) -> &str {
        &self.forge
    }

    /// The forge's own id for the job.
    pub fn id(&self) -> &str {
        &self.id
    }
}

impl fmt::Display for JobKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.forge, self.id)
    }
}

impl FromStr for JobKey {
    type Err = anyhow::Error;

    /// Parses `forge:id`. A bare id (stored before multi-forge support) belongs to
    /// the legacy GitHub forge.
    fn from_str(s: &str) -> Result<Self> {
        match s.split_once(':') {
            Some((forge, id)) if !forge.is_empty() && !id.is_empty() => Ok(Self::new(forge, id)),
            Some(_) => Err(anyhow!("invalid job key '{s}'")),
            None if !s.is_empty() => Ok(Self::new(LEGACY_GITHUB_FORGE, s)),
            None => Err(anyhow!("empty job key")),
        }
    }
}

impl Serialize for JobKey {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for JobKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

/// A queued job discovered by listing a forge's API.
#[derive(Debug, Clone)]
pub struct QueuedJob {
    /// The forge's own id for the job
    pub id: String,
    /// Labels requested by this job
    pub labels: Vec<String>,
    /// The scope (org or repo) where this job was queued
    pub runner_scope: RunnerScope,
    /// Repository full name (owner/repo)
    pub repository: Option<String>,
}

/// Operations kuiper needs from a forge to run ephemeral runners.
///
/// Implemented by the real GitHub API client and by [`MockForge`] (dry-run/testing).
#[async_trait]
pub trait Forge: Send + Sync {
    /// The web URL of a runner scope on this forge, as runners register against it.
    fn scope_url(&self, scope: &RunnerScope) -> String;

    /// Get a registration token for the given runner scope.
    async fn get_registration_token(&self, scope: &RunnerScope) -> Result<String>;

    /// Generate a JIT (Just-In-Time) runner configuration.
    /// Returns an encoded config blob that can be passed to `run.sh --jitconfig`.
    /// The runner is pre-assigned to a specific job and auto-deregisters.
    async fn generate_jitconfig(
        &self,
        scope: &RunnerScope,
        runner_name: &str,
        labels: &[String],
        runner_group_id: u64,
    ) -> Result<String>;

    /// Remove a runner from the forge by name.
    /// This is called when a runner VM is destroyed or agent disconnects.
    async fn remove_runner(&self, scope: &RunnerScope, runner_name: &str) -> Result<()>;

    /// List all queued workflow jobs this forge's credentials can see.
    async fn list_queued_jobs(&self) -> Result<Vec<QueuedJob>>;

    /// Get the status of a specific workflow job by the forge's own id.
    /// Returns the job status ("queued", "in_progress", "completed", etc.)
    /// or None if the job is not found (404).
    async fn get_job_status(&self, repo: &str, job_id: &str) -> Result<Option<String>>;
}

/// The forges a coordinator serves, by configured name.
///
/// The first forge is the default: fixed-capacity pools register with it, and
/// runners stored before multi-forge support (no forge recorded) belong to it.
pub struct ForgeRegistry {
    forges: Vec<(String, Arc<dyn Forge>)>,
}

impl ForgeRegistry {
    /// Build a registry; `forges` must be non-empty with unique names.
    pub fn new(forges: Vec<(String, Arc<dyn Forge>)>) -> Result<Self> {
        if forges.is_empty() {
            return Err(anyhow!("at least one forge must be configured"));
        }
        for (i, (name, _)) in forges.iter().enumerate() {
            if forges[..i].iter().any(|(other, _)| other == name) {
                return Err(anyhow!("forge name '{name}' is configured more than once"));
            }
        }
        Ok(Self { forges })
    }

    /// A registry with a single forge.
    pub fn single(name: impl Into<String>, forge: Arc<dyn Forge>) -> Self {
        Self {
            forges: vec![(name.into(), forge)],
        }
    }

    /// The default forge's name.
    pub fn default_name(&self) -> &str {
        &self.forges[0].0
    }

    /// The default forge.
    pub fn default_forge(&self) -> Arc<dyn Forge> {
        self.forges[0].1.clone()
    }

    /// The forge with this name.
    pub fn get(&self, name: &str) -> Result<Arc<dyn Forge>> {
        self.forges
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, f)| f.clone())
            .ok_or_else(|| anyhow!("no forge named '{name}' is configured"))
    }

    /// The forge a job is on.
    pub fn for_job(&self, key: &JobKey) -> Result<Arc<dyn Forge>> {
        self.get(key.forge())
    }

    /// The forge a stored runner belongs to: its recorded forge, or the default
    /// for runners stored before forges were recorded.
    pub fn for_runner(&self, forge: Option<&str>) -> Result<Arc<dyn Forge>> {
        match forge {
            Some(name) => self.get(name),
            None => Ok(self.default_forge()),
        }
    }

    /// Every forge, in configured order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Arc<dyn Forge>)> {
        self.forges.iter().map(|(n, f)| (n.as_str(), f))
    }
}

/// Mock forge for dry-run/testing mode.
///
/// Returns fake tokens that won't work with any forge but allow testing
/// the full agent VM lifecycle.
pub struct MockForge;

#[async_trait]
impl Forge for MockForge {
    fn scope_url(&self, scope: &RunnerScope) -> String {
        scope.to_url()
    }

    async fn get_registration_token(&self, scope: &RunnerScope) -> Result<String> {
        let fake_token = format!("dry-run-token-{}", &uuid::Uuid::new_v4().to_string()[..8]);
        info!(
            "DRY-RUN: Generated fake registration token for {:?}: {}",
            scope, fake_token
        );
        Ok(fake_token)
    }

    async fn generate_jitconfig(
        &self,
        scope: &RunnerScope,
        runner_name: &str,
        labels: &[String],
        _runner_group_id: u64,
    ) -> Result<String> {
        let fake_jit = format!(
            "dry-run-jitconfig-{}",
            &uuid::Uuid::new_v4().to_string()[..8]
        );
        info!(
            "DRY-RUN: Generated fake JIT config for runner '{}' in {:?} with labels {:?}",
            runner_name, scope, labels
        );
        Ok(fake_jit)
    }

    async fn remove_runner(&self, scope: &RunnerScope, runner_name: &str) -> Result<()> {
        info!(
            "DRY-RUN: Would remove runner '{}' from {:?}",
            runner_name, scope
        );
        Ok(())
    }

    async fn list_queued_jobs(&self) -> Result<Vec<QueuedJob>> {
        info!("DRY-RUN: list_queued_jobs returns empty (no forge API access)");
        Ok(Vec::new())
    }

    async fn get_job_status(&self, repo: &str, job_id: &str) -> Result<Option<String>> {
        info!(
            "DRY-RUN: get_job_status for job {} in {} returns None",
            job_id, repo
        );
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_key_round_trips() {
        let key = JobKey::new("knope", "a1b2-c3");
        assert_eq!(key.to_string(), "knope:a1b2-c3");
        assert_eq!("knope:a1b2-c3".parse::<JobKey>().unwrap(), key);
    }

    #[test]
    fn job_key_id_may_contain_colons() {
        let key: JobKey = "knope:run:7".parse().unwrap();
        assert_eq!(key.forge(), "knope");
        assert_eq!(key.id(), "run:7");
    }

    #[test]
    fn bare_job_id_is_legacy_github() {
        let key: JobKey = "4815162342".parse().unwrap();
        assert_eq!(key.forge(), LEGACY_GITHUB_FORGE);
        assert_eq!(key.id(), "4815162342");
    }

    #[test]
    fn invalid_job_keys_are_rejected() {
        assert!("".parse::<JobKey>().is_err());
        assert!(":123".parse::<JobKey>().is_err());
        assert!("github:".parse::<JobKey>().is_err());
    }

    #[test]
    fn job_key_serializes_as_string() {
        let key = JobKey::new("github", "42");
        let json = serde_json::to_string(&key).unwrap();
        assert_eq!(json, "\"github:42\"");
        assert_eq!(serde_json::from_str::<JobKey>(&json).unwrap(), key);
    }

    #[test]
    fn registry_resolves_forges() {
        let registry = ForgeRegistry::new(vec![
            ("github".into(), Arc::new(MockForge) as Arc<dyn Forge>),
            ("knope".into(), Arc::new(MockForge) as Arc<dyn Forge>),
        ])
        .unwrap();
        assert_eq!(registry.default_name(), "github");
        assert!(registry.get("knope").is_ok());
        assert!(registry.get("missing").is_err());
        assert!(registry.for_job(&JobKey::new("knope", "1")).is_ok());
        assert!(registry.for_runner(None).is_ok());
        assert!(registry.for_runner(Some("missing")).is_err());
    }

    #[test]
    fn registry_rejects_duplicates_and_empty() {
        assert!(ForgeRegistry::new(Vec::new()).is_err());
        assert!(
            ForgeRegistry::new(vec![
                ("a".into(), Arc::new(MockForge) as Arc<dyn Forge>),
                ("a".into(), Arc::new(MockForge) as Arc<dyn Forge>),
            ])
            .is_err()
        );
    }
}
