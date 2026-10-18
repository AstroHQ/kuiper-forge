//! Forgejo API client for ephemeral one-job runners.
//!
//! Forgejo (15+) lists the jobs waiting for a runner at org and repo scope and
//! registers ephemeral runners over its REST API. Each runner is started with
//! `forgejo-runner one-job --handle`, so it runs exactly the job attempt it was
//! made for. Forgejo before 17 has no job webhooks, so jobs are found by polling.

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use reqwest::{Client, Method, RequestBuilder, Response, StatusCode};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Instant;
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::config::{ForgejoConfig, RunnerScope};
use crate::forge::{Forge, ForgejoLaunch, QueuedJob, RunnerKind, RunnerLaunch};

/// Runners per page when listing them (Forgejo's default maximum)
const RUNNER_PAGE_SIZE: usize = 50;

/// Most runner pages read, a guard against a runaway loop
const MAX_RUNNER_PAGES: u32 = 100;

/// A job from `GET …/actions/runners/jobs` (Forgejo 15+). The endpoint returns
/// waiting and running jobs, unpaginated.
#[derive(Debug, Clone, Deserialize)]
struct ApiJob {
    id: i64,
    #[serde(default)]
    handle: String,
    #[serde(default)]
    repo_id: i64,
    #[serde(default)]
    runs_on: Vec<String>,
    #[serde(default)]
    status: String,
    #[serde(default)]
    name: String,
}

#[derive(Debug, Serialize)]
struct RegisterRunnerRequest<'a> {
    name: &'a str,
    description: &'a str,
    ephemeral: bool,
}

#[derive(Debug, Deserialize)]
struct RegisterRunnerResponse {
    id: i64,
    uuid: String,
    token: String,
}

#[derive(Debug, Deserialize)]
struct ApiRunner {
    id: i64,
    name: String,
}

#[derive(Debug, Deserialize)]
struct ApiRepository {
    full_name: String,
}

/// Forgejo's job status in GitHub's terms, which the fleet manager checks.
fn github_status(status: &str) -> String {
    match status {
        "waiting" => "queued".to_string(),
        "running" => "in_progress".to_string(),
        other => other.to_string(),
    }
}

/// Client for one Forgejo instance, polling the configured org and repo scopes.
pub struct ForgejoClient {
    /// Instance URL without a trailing slash, like `https://git.example.com`
    base_url: String,
    token: String,
    scopes: Vec<RunnerScope>,
    http_client: Client,
    /// Repository full names by id, since the jobs list only has `repo_id`
    repo_names: RwLock<HashMap<i64, String>>,
}

impl ForgejoClient {
    pub fn new(config: &ForgejoConfig) -> Result<Self> {
        let http_client = Client::builder()
            .user_agent("kuiper-forge")
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .context("Failed to create HTTP client")?;
        Ok(Self {
            base_url: config.url.trim_end_matches('/').to_string(),
            token: config.token_content()?,
            scopes: config.runner_scopes()?,
            http_client,
            repo_names: RwLock::new(HashMap::new()),
        })
    }

    /// The API path prefix for a runner scope: `orgs/{org}` or `repos/{owner}/{repo}`.
    fn scope_path(scope: &RunnerScope) -> String {
        match scope {
            RunnerScope::Organization { name } => format!("orgs/{name}"),
            RunnerScope::Repository { owner, repo } => format!("repos/{owner}/{repo}"),
        }
    }

    fn request(&self, method: Method, path: &str) -> RequestBuilder {
        self.http_client
            .request(method, format!("{}/api/v1/{path}", self.base_url))
            .header("Authorization", format!("token {}", self.token))
            .header("Accept", "application/json")
    }

    /// Send a request, logging failures with the status and how long it took.
    async fn send(&self, what: &str, path: &str, request: RequestBuilder) -> Result<Response> {
        let started = Instant::now();
        let response = request.send().await.map_err(|e| {
            warn!(
                path = %path,
                elapsed_ms = started.elapsed().as_millis() as u64,
                "Forgejo API request failed ({what}): {e}"
            );
            anyhow!("Forgejo API request failed ({what}): {e}")
        })?;
        let status = response.status();
        debug!(
            path = %path,
            status = status.as_u16(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "Forgejo API {what}"
        );
        Ok(response)
    }

    /// Turn a non-success response into an error with its status and body.
    async fn error_for(what: &str, path: &str, response: Response) -> anyhow::Error {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        let hint = match status {
            StatusCode::UNAUTHORIZED => " (is the token valid?)",
            StatusCode::FORBIDDEN => {
                " (the token's user must own the scope: org owner with write:organization, \
                 or repo owner with write:repository)"
            }
            _ => "",
        };
        warn!(
            path = %path,
            status = status.as_u16(),
            "Forgejo API error ({what}): {}",
            body.trim()
        );
        anyhow!("Forgejo API error {what} ({status}){hint}: {}", body.trim())
    }

    /// Check the token can list jobs and manage runners in every configured scope.
    ///
    /// Forgejo picks the scope level by HTTP method (GET needs `read:`, POST and
    /// DELETE need `write:`), so listing alone passes with a read-only token. The
    /// write check deletes runner id 0, which can't exist: a token that may manage
    /// runners gets 404, one that may not gets 403. Only the scopes are checked, so
    /// the token needs no `read:user` scope.
    pub async fn validate(&self) -> Result<()> {
        for scope in &self.scopes {
            let jobs = self
                .list_scope_jobs(scope)
                .await
                .with_context(|| format!("Token can't list jobs in {scope:?}"))?;
            debug!("Forgejo scope {:?} lists {} job(s)", scope, jobs.len());

            let path = format!("{}/actions/runners/0", Self::scope_path(scope));
            let response = self
                .send(
                    "checking runner access",
                    &path,
                    self.request(Method::DELETE, &path),
                )
                .await?;
            match response.status() {
                StatusCode::NOT_FOUND | StatusCode::BAD_REQUEST => {}
                status if status.is_success() => {}
                _ => {
                    return Err(Self::error_for("checking runner access", &path, response).await)
                        .with_context(|| format!("Token can't manage runners in {scope:?}"));
                }
            }
        }
        info!(
            "Forgejo {} access validated for {} scope(s)",
            self.base_url,
            self.scopes.len()
        );
        Ok(())
    }

    /// Every waiting and running job in a scope.
    async fn list_scope_jobs(&self, scope: &RunnerScope) -> Result<Vec<ApiJob>> {
        let path = format!("{}/actions/runners/jobs", Self::scope_path(scope));
        let response = self
            .send("listing jobs", &path, self.request(Method::GET, &path))
            .await?;
        if !response.status().is_success() {
            return Err(Self::error_for("listing jobs", &path, response).await);
        }
        // Forgejo answers `null` rather than `[]` when nothing is waiting or running
        let jobs: Option<Vec<ApiJob>> = response
            .json()
            .await
            .with_context(|| format!("Failed to parse jobs list from {path}"))?;
        Ok(jobs.unwrap_or_default())
    }

    /// A repository's full name by id, cached.
    async fn repo_name(&self, repo_id: i64) -> Result<String> {
        if let Some(name) = self.repo_names.read().await.get(&repo_id) {
            return Ok(name.clone());
        }
        let path = format!("repositories/{repo_id}");
        let response = self
            .send(
                "getting repository",
                &path,
                self.request(Method::GET, &path),
            )
            .await?;
        if !response.status().is_success() {
            return Err(Self::error_for("getting repository", &path, response).await);
        }
        let repo: ApiRepository = response
            .json()
            .await
            .with_context(|| format!("Failed to parse repository {repo_id}"))?;
        self.repo_names
            .write()
            .await
            .insert(repo_id, repo.full_name.clone());
        Ok(repo.full_name)
    }

    /// The scope to ask about a repository's jobs: a configured org that owns it,
    /// so an org-owner token works, else the repository itself.
    fn scope_for_repo(&self, repo: &str) -> Result<RunnerScope> {
        let (owner, name) = repo
            .split_once('/')
            .ok_or_else(|| anyhow!("Invalid repository '{repo}': expected 'owner/repo'"))?;
        let org = self.scopes.iter().find(|scope| {
            matches!(scope, RunnerScope::Organization { name } if name.eq_ignore_ascii_case(owner))
        });
        Ok(org.cloned().unwrap_or_else(|| RunnerScope::Repository {
            owner: owner.to_string(),
            repo: name.to_string(),
        }))
    }

    /// Every runner directly registered in a scope.
    async fn list_runners(&self, scope: &RunnerScope) -> Result<Vec<ApiRunner>> {
        let path = format!("{}/actions/runners", Self::scope_path(scope));
        let mut all = Vec::new();
        for page in 1..=MAX_RUNNER_PAGES {
            let page_path = format!("{path}?page={page}&limit={RUNNER_PAGE_SIZE}");
            let request = self.request(Method::GET, &page_path);
            let response = self.send("listing runners", &page_path, request).await?;
            if !response.status().is_success() {
                return Err(Self::error_for("listing runners", &path, response).await);
            }
            let runners: Vec<ApiRunner> = response
                .json::<Option<Vec<ApiRunner>>>()
                .await
                .with_context(|| format!("Failed to parse runners list from {path}"))?
                .unwrap_or_default();
            let count = runners.len();
            all.extend(runners);
            if count < RUNNER_PAGE_SIZE {
                return Ok(all);
            }
        }
        warn!(
            "Stopped after {} pages of runners in {:?} - listing may be incomplete",
            MAX_RUNNER_PAGES, scope
        );
        Ok(all)
    }
}

#[async_trait]
impl Forge for ForgejoClient {
    fn scope_url(&self, scope: &RunnerScope) -> String {
        match scope {
            RunnerScope::Organization { name } => format!("{}/{name}", self.base_url),
            RunnerScope::Repository { owner, repo } => {
                format!("{}/{owner}/{repo}", self.base_url)
            }
        }
    }

    fn runner_kind(&self) -> RunnerKind {
        RunnerKind::Forgejo
    }

    async fn get_registration_token(&self, _scope: &RunnerScope) -> Result<String> {
        Err(anyhow!(
            "Forgejo forges only run one-job runners (polling mode), not fixed-capacity pools"
        ))
    }

    async fn create_job_runner(
        &self,
        scope: &RunnerScope,
        runner_name: &str,
        _labels: &[String],
        _runner_group_id: u64,
        job_id: &str,
    ) -> Result<RunnerLaunch> {
        let path = format!("{}/actions/runners", Self::scope_path(scope));
        let description = format!("kuiper-forge one-job runner for job {job_id}");
        let request = self
            .request(Method::POST, &path)
            .json(&RegisterRunnerRequest {
                name: runner_name,
                description: &description,
                ephemeral: true,
            });
        let response = self.send("registering runner", &path, request).await?;
        if !response.status().is_success() {
            return Err(Self::error_for("registering runner", &path, response).await);
        }
        let registered: RegisterRunnerResponse = response
            .json()
            .await
            .context("Failed to parse Forgejo runner registration response")?;
        info!(
            "Registered Forgejo runner '{}' (id {}, uuid {}) in {:?} for job {}",
            runner_name, registered.id, registered.uuid, scope, job_id
        );
        Ok(RunnerLaunch::Forgejo(ForgejoLaunch {
            instance_url: self.base_url.clone(),
            uuid: registered.uuid,
            token: registered.token,
            handle: job_id.to_string(),
        }))
    }

    async fn remove_runner(&self, scope: &RunnerScope, runner_name: &str) -> Result<()> {
        let runners = self.list_runners(scope).await?;
        let matching: Vec<_> = runners.iter().filter(|r| r.name == runner_name).collect();
        if matching.is_empty() {
            // Ephemeral runners are removed by Forgejo once their job is done
            info!(
                "Forgejo runner '{}' not found in {:?} (may have already been removed)",
                runner_name, scope
            );
            return Ok(());
        }
        for runner in matching {
            let path = format!("{}/actions/runners/{}", Self::scope_path(scope), runner.id);
            let response = self
                .send(
                    "deleting runner",
                    &path,
                    self.request(Method::DELETE, &path),
                )
                .await?;
            if response.status() == StatusCode::NOT_FOUND {
                debug!("Forgejo runner {} already gone", runner.id);
            } else if !response.status().is_success() {
                return Err(Self::error_for("deleting runner", &path, response).await);
            } else {
                info!(
                    "Removed Forgejo runner '{}' (id {}) from {:?}",
                    runner_name, runner.id, scope
                );
            }
        }
        Ok(())
    }

    async fn list_queued_jobs(&self) -> Result<Vec<QueuedJob>> {
        let started = Instant::now();
        let mut queued = Vec::new();
        let mut failed_scopes = 0;
        for scope in &self.scopes {
            let jobs = match self.list_scope_jobs(scope).await {
                Ok(jobs) => jobs,
                Err(e) => {
                    // Pending jobs this scan doesn't list are checked one by one before
                    // they're dropped, so a failed scope can't lose them
                    warn!(
                        "Queued-job scan: skipping Forgejo scope {:?}: {:#}",
                        scope, e
                    );
                    failed_scopes += 1;
                    continue;
                }
            };
            for job in jobs {
                if job.status != "waiting" {
                    continue;
                }
                if job.handle.is_empty() {
                    warn!(
                        "Forgejo job {} ('{}') has no handle - needs Forgejo 15 or newer, skipping",
                        job.id, job.name
                    );
                    continue;
                }
                let repository = match scope {
                    RunnerScope::Repository { owner, repo } => Some(format!("{owner}/{repo}")),
                    RunnerScope::Organization { .. } => match self.repo_name(job.repo_id).await {
                        Ok(name) => Some(name),
                        Err(e) => {
                            warn!(
                                "Couldn't look up repository {} for Forgejo job {}: {:#}",
                                job.repo_id, job.handle, e
                            );
                            None
                        }
                    },
                };
                debug!(
                    "Forgejo job {} ('{}', handle {}) waiting in {:?} with labels {:?}",
                    job.id, job.name, job.handle, repository, job.runs_on
                );
                queued.push(QueuedJob {
                    id: job.handle,
                    labels: job.runs_on,
                    runner_scope: scope.clone(),
                    repository,
                });
            }
        }
        if failed_scopes == self.scopes.len() {
            return Err(anyhow!(
                "Couldn't list jobs in any of {} Forgejo scope(s)",
                self.scopes.len()
            ));
        }
        debug!(
            "Forgejo scan of {} scope(s) found {} waiting job(s) in {}ms",
            self.scopes.len(),
            queued.len(),
            started.elapsed().as_millis()
        );
        Ok(queued)
    }

    async fn get_job_status(&self, repo: &str, job_id: &str) -> Result<Option<String>> {
        let scope = self.scope_for_repo(repo)?;
        let jobs = self.list_scope_jobs(&scope).await?;
        // Finished and cancelled jobs aren't listed; their handle is gone
        Ok(jobs
            .into_iter()
            .find(|job| job.handle == job_id)
            .map(|job| github_status(&job.status)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::extract::{Path, Query, State};
    use axum::http::HeaderMap;
    use axum::routing::{delete, get};
    use axum::{Json, Router};
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Fake {
        deleted: Mutex<Vec<i64>>,
        registered: Mutex<Vec<Value>>,
        repo_lookups: Mutex<u32>,
        runners: Vec<Value>,
    }

    fn authed(headers: &HeaderMap) -> bool {
        headers.get("authorization").and_then(|v| v.to_str().ok()) == Some("token secret")
    }

    async fn serve(fake: Arc<Fake>) -> String {
        async fn org_jobs(headers: HeaderMap) -> (StatusCode, Json<Value>) {
            if !authed(&headers) {
                return (StatusCode::UNAUTHORIZED, Json(json!({})));
            }
            (
                StatusCode::OK,
                Json(json!([
                    {"id": 1, "handle": "h-wait", "repo_id": 7, "runs_on": ["linux"], "status": "waiting", "name": "build"},
                    {"id": 2, "handle": "h-run", "repo_id": 7, "runs_on": ["linux"], "status": "running", "name": "test"},
                    {"id": 3, "handle": "", "repo_id": 7, "runs_on": ["linux"], "status": "waiting", "name": "old"}
                ])),
            )
        }
        async fn repo_jobs() -> (StatusCode, Json<Value>) {
            (StatusCode::FORBIDDEN, Json(json!({"message": "not owner"})))
        }
        async fn repository(State(fake): State<Arc<Fake>>, Path(id): Path<i64>) -> Json<Value> {
            *fake.repo_lookups.lock().unwrap() += 1;
            Json(json!({"id": id, "full_name": "lodge/app"}))
        }
        async fn runners(
            State(fake): State<Arc<Fake>>,
            Query(q): Query<HashMap<String, String>>,
        ) -> Json<Value> {
            let page: usize = q["page"].parse().unwrap();
            let limit: usize = q["limit"].parse().unwrap();
            let items: Vec<Value> = fake
                .runners
                .iter()
                .skip((page - 1) * limit)
                .take(limit)
                .cloned()
                .collect();
            Json(Value::Array(items))
        }
        async fn register(
            State(fake): State<Arc<Fake>>,
            Json(body): Json<Value>,
        ) -> (StatusCode, Json<Value>) {
            fake.registered.lock().unwrap().push(body);
            (
                StatusCode::CREATED,
                Json(json!({"id": 99, "uuid": "u-1", "token": "t-1"})),
            )
        }
        async fn remove(State(fake): State<Arc<Fake>>, Path(id): Path<i64>) -> StatusCode {
            if id == 0 {
                return StatusCode::NOT_FOUND;
            }
            fake.deleted.lock().unwrap().push(id);
            StatusCode::NO_CONTENT
        }
        async fn read_only_remove() -> (StatusCode, Json<Value>) {
            (
                StatusCode::FORBIDDEN,
                Json(
                    json!({"message": "token does not have at least one of required scope(s): [write:organization]"}),
                ),
            )
        }
        async fn null_jobs() -> Json<Value> {
            Json(Value::Null)
        }
        let app = Router::new()
            .route("/api/v1/orgs/lodge/actions/runners/jobs", get(org_jobs))
            .route("/api/v1/orgs/empty/actions/runners/jobs", get(null_jobs))
            .route("/api/v1/orgs/empty/actions/runners", get(null_jobs))
            .route(
                "/api/v1/repos/bad/repo/actions/runners/jobs",
                get(repo_jobs),
            )
            .route("/api/v1/repositories/{id}", get(repository))
            .route(
                "/api/v1/orgs/lodge/actions/runners",
                get(runners).post(register),
            )
            .route("/api/v1/orgs/lodge/actions/runners/{id}", delete(remove))
            .route("/api/v1/orgs/ro/actions/runners/jobs", get(null_jobs))
            .route(
                "/api/v1/orgs/ro/actions/runners/{id}",
                delete(read_only_remove),
            )
            .with_state(fake);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}/")
    }

    fn client(url: &str, scopes: &[&str]) -> ForgejoClient {
        ForgejoClient::new(&ForgejoConfig {
            url: url.to_string(),
            token: Some("secret".into()),
            token_path: None,
            token_env: None,
            scopes: scopes.iter().map(|s| s.to_string()).collect(),
        })
        .unwrap()
    }

    #[tokio::test]
    async fn lists_only_waiting_jobs_by_handle() {
        let fake = Arc::new(Fake::default());
        let url = serve(fake.clone()).await;
        let forge = client(&url, &["lodge"]);
        let jobs = forge.list_queued_jobs().await.unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].id, "h-wait");
        assert_eq!(jobs[0].labels, vec!["linux"]);
        assert_eq!(jobs[0].repository.as_deref(), Some("lodge/app"));
        assert_eq!(
            jobs[0].runner_scope,
            RunnerScope::Organization {
                name: "lodge".into()
            }
        );
        // the repo name is cached
        forge.list_queued_jobs().await.unwrap();
        assert_eq!(*fake.repo_lookups.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn validate_needs_runner_write_access() {
        let fake = Arc::new(Fake::default());
        let url = serve(fake.clone()).await;
        client(&url, &["lodge"]).validate().await.unwrap();
        assert!(fake.deleted.lock().unwrap().is_empty());
        // a read-only token lists jobs fine but can't manage runners
        let err = client(&url, &["ro"]).validate().await.unwrap_err();
        assert!(format!("{err:#}").contains("must own the scope"), "{err:#}");
    }

    #[tokio::test]
    async fn null_lists_are_empty() {
        let url = serve(Arc::new(Fake::default())).await;
        let forge = client(&url, &["empty"]);
        assert!(forge.list_queued_jobs().await.unwrap().is_empty());
        // a finished job is "not found", not an error
        assert_eq!(
            forge.get_job_status("empty/app", "h-1").await.unwrap(),
            None
        );
        let scope = RunnerScope::Organization {
            name: "empty".into(),
        };
        forge.remove_runner(&scope, "runner-1").await.unwrap();
    }

    #[tokio::test]
    async fn one_failing_scope_is_skipped_all_failing_is_an_error() {
        let url = serve(Arc::new(Fake::default())).await;
        let jobs = client(&url, &["lodge", "bad/repo"])
            .list_queued_jobs()
            .await
            .unwrap();
        assert_eq!(jobs.len(), 1);
        assert!(
            client(&url, &["bad/repo"])
                .list_queued_jobs()
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn job_status_maps_to_github_terms() {
        let url = serve(Arc::new(Fake::default())).await;
        let forge = client(&url, &["lodge"]);
        // org scope owns the repo, so the org endpoint is asked
        assert_eq!(
            forge.get_job_status("lodge/app", "h-wait").await.unwrap(),
            Some("queued".to_string())
        );
        assert_eq!(
            forge.get_job_status("Lodge/app", "h-run").await.unwrap(),
            Some("in_progress".to_string())
        );
        assert_eq!(
            forge.get_job_status("lodge/app", "gone").await.unwrap(),
            None
        );
        // a repo outside the org scopes is asked at repo scope; errors aren't "not found"
        let err = forge.get_job_status("bad/repo", "x").await.unwrap_err();
        assert!(format!("{err:#}").contains("must own the scope"), "{err:#}");
    }

    #[tokio::test]
    async fn registers_ephemeral_runner_for_the_handle() {
        let fake = Arc::new(Fake::default());
        let url = serve(fake.clone()).await;
        let forge = client(&url, &["lodge"]);
        let scope = RunnerScope::Organization {
            name: "lodge".into(),
        };
        let launch = forge
            .create_job_runner(&scope, "runner-1", &["linux".into()], 1, "h-wait")
            .await
            .unwrap();
        assert_eq!(
            launch,
            RunnerLaunch::Forgejo(ForgejoLaunch {
                instance_url: url.trim_end_matches('/').to_string(),
                uuid: "u-1".into(),
                token: "t-1".into(),
                handle: "h-wait".into(),
            })
        );
        let registered = fake.registered.lock().unwrap().clone();
        assert_eq!(registered[0]["name"], "runner-1");
        assert_eq!(registered[0]["ephemeral"], true);
        assert_eq!(forge.scope_url(&scope), format!("{url}lodge"));
        assert!(forge.get_registration_token(&scope).await.is_err());
    }

    #[tokio::test]
    async fn removes_runner_by_name_across_pages() {
        let mut runners: Vec<Value> = (0..RUNNER_PAGE_SIZE as i64)
            .map(|i| json!({"id": i, "name": format!("other-{i}")}))
            .collect();
        runners.push(json!({"id": 500, "name": "runner-1"}));
        let fake = Arc::new(Fake {
            runners,
            ..Fake::default()
        });
        let url = serve(fake.clone()).await;
        let forge = client(&url, &["lodge"]);
        let scope = RunnerScope::Organization {
            name: "lodge".into(),
        };
        forge.remove_runner(&scope, "runner-1").await.unwrap();
        assert_eq!(*fake.deleted.lock().unwrap(), vec![500]);
        // a runner that's already gone is fine
        forge.remove_runner(&scope, "missing").await.unwrap();
        assert_eq!(fake.deleted.lock().unwrap().len(), 1);
    }
}
