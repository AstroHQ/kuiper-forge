//! Webhook handler for dynamic runner provisioning.
//!
//! Handles GitHub `workflow_job` webhook events to create runners on-demand.
//! This module provides:
//! - GitHub webhook payload parsing
//! - HMAC-SHA256 signature validation
//! - Label matching against configured mappings
//! - Axum HTTP handlers for the webhook endpoint

use axum::{
    Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
};
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::Sha256;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::config::{LabelMapping, RunnerScope, WebhookConfig};
use crate::forge::JobKey;
use crate::metrics::MetricsStore;
use crate::pending_jobs::PendingJobStore;

/// GitHub webhook event types we care about
const WORKFLOW_JOB_EVENT: &str = "workflow_job";

/// GitHub workflow_job webhook payload (partial - only fields we need)
#[derive(Debug, Deserialize)]
pub struct WorkflowJobEvent {
    pub action: String,
    pub workflow_job: WorkflowJob,
    pub repository: Repository,
    #[serde(default)]
    pub organization: Option<Organization>,
    /// Workflow name (from top-level of webhook event)
    #[serde(default)]
    pub workflow_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct WorkflowJob {
    pub id: u64,
    /// Job name from the workflow file
    pub name: String,
    pub labels: Vec<String>,
    #[serde(default)]
    pub runner_id: Option<u64>,
    #[serde(default)]
    pub runner_name: Option<String>,
    #[serde(default)]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub completed_at: Option<DateTime<Utc>>,
    /// success, failure, cancelled, skipped, ... once it's completed
    #[serde(default)]
    pub conclusion: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Repository {
    pub full_name: String,
    #[allow(dead_code)]
    pub owner: RepositoryOwner,
    #[allow(dead_code)]
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct RepositoryOwner {
    #[allow(dead_code)]
    pub login: String,
    #[allow(dead_code)]
    #[serde(rename = "type")]
    pub owner_type: String,
}

#[derive(Debug, Deserialize)]
pub struct Organization {
    #[allow(dead_code)]
    pub login: String,
}

/// Request to create a runner, sent from webhook handler to fleet manager
#[derive(Debug, Clone)]
pub struct WebhookRunnerRequest {
    /// Job labels from the webhook (the labels the job requested)
    pub job_labels: Vec<String>,
    /// Labels to use for agent matching (typically same as job_labels)
    pub agent_labels: Vec<String>,
    /// Runner scope determined by label matching
    pub runner_scope: RunnerScope,
    /// Optional runner group
    pub runner_group: Option<String>,
    /// Job key (forge name + the forge's job id), for deduplication
    pub job_id: JobKey,
    /// Repository full name (owner/repo) for GitHub API status checks
    pub repository: Option<String>,
    /// Job name from the workflow file
    pub job_name: Option<String>,
    /// Workflow name
    pub workflow_name: Option<String>,
    /// When the forge queued the job, if it says
    pub queued_at: Option<DateTime<Utc>>,
}

/// Event types sent from webhook handler to fleet manager.
///
/// These are lightweight notifications - the actual job data is persisted
/// to the database by the webhook handler, so losing a notification doesn't
/// lose the job.
#[derive(Debug, Clone)]
pub enum WebhookEvent {
    /// New job(s) available in pending queue - check and process
    NewJobAvailable,
    /// Job completed/cancelled - clean up state and check pending queue
    JobCompleted { job_id: JobKey },
}

/// Handle for sending webhook events to the fleet manager
#[derive(Clone)]
pub struct WebhookNotifier {
    tx: mpsc::Sender<WebhookEvent>,
}

impl WebhookNotifier {
    pub fn new(tx: mpsc::Sender<WebhookEvent>) -> Self {
        Self { tx }
    }

    pub async fn send(&self, event: WebhookEvent) -> bool {
        self.tx.try_send(event).is_ok()
    }
}

/// Shared state for webhook handlers
pub struct WebhookState {
    pub config: WebhookConfig,
    /// Configured name of the GitHub forge these webhooks come from
    pub forge: String,
    pub notifier: WebhookNotifier,
    /// Persistent store for pending jobs - webhook handler writes directly to DB
    pub pending_job_store: Arc<PendingJobStore>,
    pub metrics: Arc<MetricsStore>,
}

/// Validate HMAC-SHA256 signature from GitHub.
///
/// GitHub sends the signature in the `X-Hub-Signature-256` header as `sha256=<hex>`.
pub fn validate_signature(secret: &str, payload: &[u8], signature_header: &str) -> bool {
    // GitHub sends signature as "sha256=<hex>"
    let signature = match signature_header.strip_prefix("sha256=") {
        Some(sig) => sig,
        None => return false,
    };

    let signature_bytes = match hex::decode(signature) {
        Ok(bytes) => bytes,
        Err(_) => return false,
    };

    type HmacSha256 = Hmac<Sha256>;
    let mut mac = match HmacSha256::new_from_slice(secret.as_bytes()) {
        Ok(mac) => mac,
        Err(_) => return false,
    };

    mac.update(payload);
    mac.verify_slice(&signature_bytes).is_ok()
}

/// Check if job labels contain all required labels (case-insensitive).
///
/// Returns `true` if ALL required labels are present in job_labels.
/// Empty required_labels returns `true` (no filtering).
pub fn has_required_labels(job_labels: &[String], required_labels: &[String]) -> bool {
    required_labels.iter().all(|required| {
        job_labels
            .iter()
            .any(|job_label| job_label.eq_ignore_ascii_case(required))
    })
}

/// Match job labels against configured label mappings.
///
/// Returns the first matching `LabelMapping`, or `None` if no match.
/// A mapping matches if it applies to the job's forge (no `forge`, or the same
/// name) and ALL of its labels are present in the job's labels
/// (case-insensitive comparison).
pub fn match_labels<'a>(
    forge: &str,
    job_labels: &[String],
    mappings: &'a [LabelMapping],
) -> Option<&'a LabelMapping> {
    mappings.iter().find(|mapping| {
        mapping.forge.as_deref().is_none_or(|f| f == forge)
            && mapping.labels.iter().all(|required| {
                job_labels
                    .iter()
                    .any(|job_label| job_label.eq_ignore_ascii_case(required))
            })
    })
}

/// Decide the runner scope and group for a queued job from the label mappings.
///
/// With no mappings, every job is accepted with `default_scope`. Otherwise the first
/// matching mapping wins (its scope falling back to `default_scope`), and a job no
/// mapping matches is rejected with `None`. Mappings restricted to another forge
/// never match. Shared by the webhook handler and the polling scan so both accept
/// the same jobs.
pub fn resolve_runner_scope(
    job_id: &JobKey,
    job_labels: &[String],
    mappings: &[LabelMapping],
    default_scope: impl FnOnce() -> RunnerScope,
) -> Option<(RunnerScope, Option<String>)> {
    if mappings.is_empty() {
        let scope = default_scope();
        info!(
            "No label mappings configured - accepting job {} with default scope {:?}",
            job_id, scope
        );
        return Some((scope, None));
    }
    match match_labels(job_id.forge(), job_labels, mappings) {
        Some(mapping) => {
            let scope = mapping.runner_scope.clone().unwrap_or_else(default_scope);
            info!(
                "Matched job {} to runner scope {:?} (mapping labels: {:?})",
                job_id, scope, mapping.labels
            );
            Some((scope, mapping.runner_group.clone()))
        }
        None => {
            info!(
                "No label mapping matches job {} labels {:?} - ignoring",
                job_id, job_labels
            );
            None
        }
    }
}

/// Build the Axum router for webhook endpoints.
pub fn webhook_router(state: Arc<WebhookState>) -> Router {
    let path = state.config.path.clone();
    Router::new()
        .route(&path, post(handle_webhook))
        .with_state(state)
}

/// Build the combined HTTP router with webhook and admin UI.
pub fn http_router(
    webhook_state: Arc<WebhookState>,
    admin_state: Option<Arc<crate::admin::AdminState>>,
) -> Router {
    use axum::response::Redirect;
    use axum::routing::get;

    let mut router = webhook_router(webhook_state);

    // Add admin UI routes if enabled
    if let Some(admin_state) = admin_state {
        // Handle both /admin and /admin/ by redirecting to dashboard
        router = router
            .route("/admin", get(|| async { Redirect::to("/admin/dashboard") }))
            .route(
                "/admin/",
                get(|| async { Redirect::to("/admin/dashboard") }),
            )
            .nest("/admin", crate::admin::admin_router(admin_state.clone()))
            .nest("/api/v1", crate::admin::api_router(admin_state));
    }

    router
}

/// Build an admin-only HTTP router (no webhook).
///
/// Used when admin UI is enabled but webhook mode is not configured.
pub fn admin_only_router(admin_state: Arc<crate::admin::AdminState>) -> Router {
    use axum::response::Redirect;
    use axum::routing::get;

    Router::new()
        .route("/admin", get(|| async { Redirect::to("/admin/dashboard") }))
        .route(
            "/admin/",
            get(|| async { Redirect::to("/admin/dashboard") }),
        )
        .nest("/admin", crate::admin::admin_router(admin_state.clone()))
        .nest("/api/v1", crate::admin::api_router(admin_state))
}

/// Handle incoming GitHub webhook.
async fn handle_webhook(
    State(state): State<Arc<WebhookState>>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    // Get event type
    let event_type = headers
        .get("X-GitHub-Event")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    // We only care about workflow_job events
    if event_type != WORKFLOW_JOB_EVENT {
        debug!("Ignoring webhook event type: {}", event_type);
        return StatusCode::OK;
    }

    // Validate signature
    let signature = headers
        .get("X-Hub-Signature-256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if !validate_signature(&state.config.secret, &body, signature) {
        warn!("Webhook signature validation failed");
        return StatusCode::UNAUTHORIZED;
    }

    // Parse payload
    let event: WorkflowJobEvent = match serde_json::from_slice(&body) {
        Ok(e) => e,
        Err(e) => {
            warn!("Failed to parse webhook payload: {}", e);
            return StatusCode::BAD_REQUEST;
        }
    };

    // Handle different workflow_job actions
    match event.action.as_str() {
        "queued" => {
            info!(
                "Received workflow_job.queued for job {} with labels {:?}",
                event.workflow_job.id, event.workflow_job.labels
            );

            // Pre-filter: check required labels (default: "self-hosted")
            if !has_required_labels(&event.workflow_job.labels, &state.config.required_labels) {
                info!(
                    "Job {} missing required labels {:?} (has {:?}) - ignoring",
                    event.workflow_job.id, state.config.required_labels, event.workflow_job.labels
                );
                return StatusCode::OK;
            }

            // Helper to compute default runner scope from webhook event
            let default_runner_scope = || {
                // Prefer organization login if available, fall back to repository owner
                let org_name = event
                    .organization
                    .as_ref()
                    .map(|o| o.login.clone())
                    .unwrap_or_else(|| event.repository.owner.login.clone());
                RunnerScope::Organization { name: org_name }
            };

            // Try to match labels to find runner scope and runner group
            let job_key = JobKey::new(&state.forge, event.workflow_job.id.to_string());
            let Some((runner_scope, runner_group)) = resolve_runner_scope(
                &job_key,
                &event.workflow_job.labels,
                &state.config.label_mappings,
                default_runner_scope,
            ) else {
                return StatusCode::OK;
            };

            // Build request and persist to DB first (this is the critical path)
            // Use job labels for agent matching - agents with a superset of these labels will match
            let request = WebhookRunnerRequest {
                job_labels: event.workflow_job.labels.clone(),
                agent_labels: event.workflow_job.labels,
                runner_scope,
                runner_group,
                job_id: job_key,
                repository: Some(event.repository.full_name),
                job_name: Some(event.workflow_job.name),
                workflow_name: event.workflow_name,
                queued_at: event.workflow_job.created_at,
            };

            // Persist to database - this ensures we don't lose the job
            if !state.pending_job_store.add_job(&request).await {
                // DB write failed - return 503 so GitHub will retry
                warn!(
                    "Failed to persist job {} to database - returning 503 for retry",
                    event.workflow_job.id
                );
                return StatusCode::SERVICE_UNAVAILABLE;
            }

            info!("Persisted job {} to pending queue", event.workflow_job.id);

            // Notify fleet manager (non-blocking, can fail safely since job is in DB)
            // If notification fails, FleetManager will pick up the job on next event or poll
            let _ = state.notifier.send(WebhookEvent::NewJobAvailable).await;
        }
        "in_progress" => {
            // only for the history: the dashboard's queue wait ends here, and it says which runner took the job
            let job_key = JobKey::new(&state.forge, event.workflow_job.id.to_string());
            let at = event.workflow_job.started_at.unwrap_or_else(Utc::now);
            let runner = event.workflow_job.runner_name.as_deref();
            state.metrics.job_started(&job_key, runner, at).await;
            if let Some(runner) = runner {
                state.metrics.runner_job_started(runner, at).await;
            }
        }
        "completed" | "cancelled" => {
            // Job finished - notify fleet manager to clean up deduplication state
            debug!(
                "Received workflow_job.{} for job {}",
                event.action, event.workflow_job.id
            );
            let job_key = JobKey::new(&state.forge, event.workflow_job.id.to_string());
            state
                .metrics
                .job_completed(
                    &job_key,
                    event.workflow_job.conclusion.as_deref(),
                    event.workflow_job.completed_at.unwrap_or_else(Utc::now),
                )
                .await;
            let _ = state
                .notifier
                .send(WebhookEvent::JobCompleted {
                    job_id: JobKey::new(&state.forge, event.workflow_job.id.to_string()),
                })
                .await;
        }
        _ => {
            debug!(
                "Ignoring workflow_job action: {} for job {}",
                event.action, event.workflow_job.id
            );
        }
    }

    // Always return 200 OK quickly - runner creation is async
    StatusCode::OK
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_signature() {
        let secret = "test-secret";
        let payload = b"test payload";

        // Generate valid signature
        use hmac::Mac;
        type HmacSha256 = Hmac<Sha256>;
        let mut mac = HmacSha256::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(payload);
        let signature = hex::encode(mac.finalize().into_bytes());
        let header = format!("sha256={signature}");

        assert!(validate_signature(secret, payload, &header));
        assert!(!validate_signature("wrong-secret", payload, &header));
        assert!(!validate_signature(secret, b"wrong payload", &header));
    }

    #[test]
    fn test_validate_signature_github_example() {
        // Official test values from GitHub webhook documentation
        // https://docs.github.com/en/webhooks/using-webhooks/validating-webhook-deliveries
        let secret = "It's a Secret to Everybody";
        let payload = b"Hello, World!";
        let expected_signature =
            "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17";

        assert!(
            validate_signature(secret, payload, expected_signature),
            "GitHub's official test case should validate correctly"
        );
    }

    #[test]
    fn test_validate_signature_invalid_format() {
        let secret = "test-secret";
        let payload = b"test payload";

        // Missing sha256= prefix
        assert!(!validate_signature(secret, payload, "abc123"));

        // Invalid hex
        assert!(!validate_signature(secret, payload, "sha256=notvalidhex!"));

        // Empty
        assert!(!validate_signature(secret, payload, ""));
    }

    #[test]
    fn test_match_labels() {
        let mappings = vec![
            LabelMapping {
                labels: vec!["self-hosted".into(), "macOS".into(), "ARM64".into()],
                runner_scope: Some(RunnerScope::Organization {
                    name: "test-org".into(),
                }),
                runner_group: None,
                forge: None,
            },
            LabelMapping {
                labels: vec!["self-hosted".into(), "Linux".into()],
                runner_scope: Some(RunnerScope::Repository {
                    owner: "test-org".into(),
                    repo: "test-repo".into(),
                }),
                runner_group: Some("linux-runners".into()),
                forge: None,
            },
        ];

        // Exact match
        let job_labels = vec!["self-hosted".into(), "macOS".into(), "ARM64".into()];
        let matched = match_labels("github", &job_labels, &mappings);
        assert!(matched.is_some());
        assert_eq!(
            matched.unwrap().runner_scope,
            Some(RunnerScope::Organization {
                name: "test-org".into()
            })
        );

        // Superset match (extra labels OK)
        let job_labels = vec![
            "self-hosted".into(),
            "macOS".into(),
            "ARM64".into(),
            "gpu".into(),
        ];
        let matched = match_labels("github", &job_labels, &mappings);
        assert!(matched.is_some());

        // Case-insensitive
        let job_labels = vec!["SELF-HOSTED".into(), "macos".into(), "arm64".into()];
        let matched = match_labels("github", &job_labels, &mappings);
        assert!(matched.is_some());

        // No match (missing required label)
        let job_labels = vec!["self-hosted".into(), "macOS".into()];
        let matched = match_labels("github", &job_labels, &mappings);
        assert!(matched.is_none());

        // No match (github-hosted runner)
        let job_labels = vec!["ubuntu-latest".into()];
        let matched = match_labels("github", &job_labels, &mappings);
        assert!(matched.is_none());

        // Match second mapping
        let job_labels = vec!["self-hosted".into(), "Linux".into()];
        let matched = match_labels("github", &job_labels, &mappings);
        assert!(matched.is_some());
        assert!(matches!(
            matched.unwrap().runner_scope,
            Some(RunnerScope::Repository { .. })
        ));
    }

    #[test]
    fn test_resolve_runner_scope() {
        let org = || RunnerScope::Organization {
            name: "default-org".into(),
        };

        // No mappings: accept with the default scope
        let (scope, group) = resolve_runner_scope(
            &JobKey::new("github", "1"),
            &["self-hosted".into()],
            &[],
            org,
        )
        .unwrap();
        assert!(matches!(scope, RunnerScope::Organization { ref name } if name == "default-org"));
        assert!(group.is_none());

        let mappings = vec![
            LabelMapping {
                labels: vec!["self-hosted".into(), "macOS".into()],
                runner_scope: None,
                runner_group: Some("mac".into()),
                forge: None,
            },
            LabelMapping {
                labels: vec!["self-hosted".into(), "Linux".into()],
                runner_scope: Some(RunnerScope::Repository {
                    owner: "o".into(),
                    repo: "r".into(),
                }),
                runner_group: None,
                forge: None,
            },
        ];

        // Mapping without a scope falls back to the default, keeps its group
        let (scope, group) = resolve_runner_scope(
            &JobKey::new("github", "2"),
            &["self-hosted".into(), "macOS".into()],
            &mappings,
            org,
        )
        .unwrap();
        assert!(matches!(scope, RunnerScope::Organization { .. }));
        assert_eq!(group.as_deref(), Some("mac"));

        // Mapping scope wins
        let (scope, _) = resolve_runner_scope(
            &JobKey::new("github", "3"),
            &["self-hosted".into(), "Linux".into()],
            &mappings,
            org,
        )
        .unwrap();
        assert!(matches!(scope, RunnerScope::Repository { .. }));

        // No mapping matches: rejected
        assert!(
            resolve_runner_scope(
                &JobKey::new("github", "4"),
                &["ubuntu-latest".into()],
                &mappings,
                org
            )
            .is_none()
        );
    }

    #[test]
    fn test_match_labels_empty() {
        let mappings: Vec<LabelMapping> = vec![];
        let job_labels = vec!["self-hosted".into()];
        assert!(match_labels("github", &job_labels, &mappings).is_none());

        let mappings = vec![LabelMapping {
            labels: vec!["self-hosted".into()],
            runner_scope: None, // Test with no runner_scope - will use repo from webhook
            runner_group: None,
            forge: None,
        }];
        let job_labels: Vec<String> = vec![];
        assert!(match_labels("github", &job_labels, &mappings).is_none());
    }

    #[test]
    fn test_match_labels_respects_forge() {
        let mappings = vec![
            LabelMapping {
                labels: vec!["linux".into()],
                runner_scope: None,
                runner_group: Some("forgejo".into()),
                forge: Some("knope".into()),
            },
            LabelMapping {
                labels: vec!["linux".into()],
                runner_scope: None,
                runner_group: Some("any".into()),
                forge: None,
            },
        ];
        let job_labels = vec!["linux".into()];
        let matched = match_labels("knope", &job_labels, &mappings).unwrap();
        assert_eq!(matched.runner_group.as_deref(), Some("forgejo"));
        // a mapping for another forge is skipped
        let matched = match_labels("github", &job_labels, &mappings).unwrap();
        assert_eq!(matched.runner_group.as_deref(), Some("any"));
        // and is never matched on its own
        assert!(match_labels("github", &job_labels, &mappings[..1]).is_none());
    }

    #[test]
    fn test_has_required_labels() {
        // Default: require "self-hosted"
        let required = vec!["self-hosted".into()];

        // Has self-hosted
        let job_labels = vec!["self-hosted".into(), "macOS".into()];
        assert!(has_required_labels(&job_labels, &required));

        // Case-insensitive
        let job_labels = vec!["SELF-HOSTED".into()];
        assert!(has_required_labels(&job_labels, &required));

        // Missing self-hosted (github-hosted runner)
        let job_labels = vec!["ubuntu-latest".into()];
        assert!(!has_required_labels(&job_labels, &required));

        // Empty job labels
        let job_labels: Vec<String> = vec![];
        assert!(!has_required_labels(&job_labels, &required));

        // Empty required labels = accept all
        let required: Vec<String> = vec![];
        let job_labels = vec!["ubuntu-latest".into()];
        assert!(has_required_labels(&job_labels, &required));

        // Multiple required labels
        let required = vec!["self-hosted".into(), "macOS".into()];
        let job_labels = vec!["self-hosted".into(), "macOS".into(), "ARM64".into()];
        assert!(has_required_labels(&job_labels, &required));

        // Missing one of multiple required
        let job_labels = vec!["self-hosted".into(), "Linux".into()];
        assert!(!has_required_labels(&job_labels, &required));
    }

    #[test]
    fn test_parse_workflow_job_event() {
        let payload = r#"{
            "action": "queued",
            "workflow_job": {
                "id": 12345,
                "name": "build",
                "labels": ["self-hosted", "macOS", "ARM64"],
                "runner_id": null,
                "runner_name": null
            },
            "repository": {
                "full_name": "test-org/test-repo",
                "name": "test-repo",
                "owner": {
                    "login": "test-org",
                    "type": "Organization"
                }
            },
            "organization": {
                "login": "test-org"
            }
        }"#;

        let event: WorkflowJobEvent = serde_json::from_str(payload).unwrap();
        assert_eq!(event.action, "queued");
        assert_eq!(event.workflow_job.id, 12345);
        assert_eq!(event.workflow_job.labels.len(), 3);
        assert!(
            event
                .workflow_job
                .labels
                .contains(&"self-hosted".to_string())
        );
        assert_eq!(event.workflow_job.created_at, None);
    }

    #[test]
    fn test_parse_completed_job_times() {
        let payload = r#"{
            "action": "completed",
            "workflow_job": {
                "id": 29679449,
                "run_id": 10431227,
                "name": "test",
                "status": "completed",
                "conclusion": "success",
                "created_at": "2026-10-28T19:34:56Z",
                "started_at": "2026-10-28T19:35:41Z",
                "completed_at": "2026-10-28T19:41:02Z",
                "labels": ["self-hosted", "macos"],
                "runner_id": 412,
                "runner_name": "mac-mini-3-1a2b3c4d",
                "runner_group_id": 1
            },
            "repository": {
                "full_name": "test-org/test-repo",
                "name": "test-repo",
                "owner": { "login": "test-org", "type": "Organization" }
            }
        }"#;

        let job = serde_json::from_str::<WorkflowJobEvent>(payload)
            .unwrap()
            .workflow_job;
        assert_eq!(job.conclusion.as_deref(), Some("success"));
        assert_eq!(job.runner_name.as_deref(), Some("mac-mini-3-1a2b3c4d"));
        let wait = job.started_at.unwrap() - job.created_at.unwrap();
        assert_eq!(wait.num_seconds(), 45);
        assert!(job.completed_at.is_some());
    }
}
