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
use hmac::{Hmac, Mac};
use serde::Deserialize;
use sha2::Sha256;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::config::{LabelMapping, RunnerScope, WebhookConfig};
use crate::pending_jobs::PendingJobStore;

/// GitHub webhook event types we care about
const WORKFLOW_JOB_EVENT: &str = "workflow_job";

/// GitHub workflow_job webhook payload (partial - only fields we need)
#[derive(Debug, Deserialize)]
pub struct WorkflowJobEvent {
    pub action: String,
    pub workflow_job: WorkflowJob,
    #[allow(dead_code)]
    pub repository: Repository,
    #[allow(dead_code)]
    #[serde(default)]
    pub organization: Option<Organization>,
}

#[derive(Debug, Deserialize)]
pub struct WorkflowJob {
    pub id: u64,
    #[allow(dead_code)]
    pub name: String,
    pub labels: Vec<String>,
    #[allow(dead_code)]
    pub runner_id: Option<u64>,
    #[allow(dead_code)]
    pub runner_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct Repository {
    #[allow(dead_code)]
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
    /// GitHub job ID (for logging/deduplication)
    pub job_id: u64,
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
    JobCompleted { job_id: u64 },
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
    pub notifier: WebhookNotifier,
    /// Persistent store for pending jobs - webhook handler writes directly to DB
    pub pending_job_store: Arc<PendingJobStore>,
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
/// A mapping matches if ALL of its labels are present in the job's labels
/// (case-insensitive comparison).
pub fn match_labels<'a>(
    job_labels: &[String],
    mappings: &'a [LabelMapping],
) -> Option<&'a LabelMapping> {
    mappings.iter().find(|mapping| {
        mapping.labels.iter().all(|required| {
            job_labels
                .iter()
                .any(|job_label| job_label.eq_ignore_ascii_case(required))
        })
    })
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
            .route("/admin/", get(|| async { Redirect::to("/admin/dashboard") }))
            .nest("/admin", crate::admin::admin_router(admin_state));
    }

    router
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
                    event.workflow_job.id,
                    state.config.required_labels,
                    event.workflow_job.labels
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
            let (runner_scope, runner_group) = if state.config.label_mappings.is_empty() {
                // No mappings configured - use default scope for all matching jobs
                let scope = default_runner_scope();
                info!(
                    "No label mappings configured - accepting job {} with default scope {:?}",
                    event.workflow_job.id, scope
                );
                (scope, None)
            } else {
                // Match against configured mappings
                match match_labels(&event.workflow_job.labels, &state.config.label_mappings) {
                    Some(mapping) => {
                        let scope = mapping.runner_scope.clone().unwrap_or_else(default_runner_scope);
                        info!(
                            "Matched job {} to runner scope {:?} (mapping labels: {:?})",
                            event.workflow_job.id, scope, mapping.labels
                        );
                        (scope, mapping.runner_group.clone())
                    }
                    None => {
                        info!(
                            "No label mapping matches job {} labels {:?} - ignoring",
                            event.workflow_job.id, event.workflow_job.labels
                        );
                        return StatusCode::OK;
                    }
                }
            };

            // Build request and persist to DB first (this is the critical path)
            // Use job labels for agent matching - agents with a superset of these labels will match
            let request = WebhookRunnerRequest {
                job_labels: event.workflow_job.labels.clone(),
                agent_labels: event.workflow_job.labels,
                runner_scope,
                runner_group,
                job_id: event.workflow_job.id,
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
        "completed" | "cancelled" => {
            // Job finished - notify fleet manager to clean up deduplication state
            debug!(
                "Received workflow_job.{} for job {}",
                event.action, event.workflow_job.id
            );
            let _ = state
                .notifier
                .send(WebhookEvent::JobCompleted {
                    job_id: event.workflow_job.id,
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
            },
            LabelMapping {
                labels: vec!["self-hosted".into(), "Linux".into()],
                runner_scope: Some(RunnerScope::Repository {
                    owner: "test-org".into(),
                    repo: "test-repo".into(),
                }),
                runner_group: Some("linux-runners".into()),
            },
        ];

        // Exact match
        let job_labels = vec!["self-hosted".into(), "macOS".into(), "ARM64".into()];
        let matched = match_labels(&job_labels, &mappings);
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
        let matched = match_labels(&job_labels, &mappings);
        assert!(matched.is_some());

        // Case-insensitive
        let job_labels = vec!["SELF-HOSTED".into(), "macos".into(), "arm64".into()];
        let matched = match_labels(&job_labels, &mappings);
        assert!(matched.is_some());

        // No match (missing required label)
        let job_labels = vec!["self-hosted".into(), "macOS".into()];
        let matched = match_labels(&job_labels, &mappings);
        assert!(matched.is_none());

        // No match (github-hosted runner)
        let job_labels = vec!["ubuntu-latest".into()];
        let matched = match_labels(&job_labels, &mappings);
        assert!(matched.is_none());

        // Match second mapping
        let job_labels = vec!["self-hosted".into(), "Linux".into()];
        let matched = match_labels(&job_labels, &mappings);
        assert!(matched.is_some());
        assert!(matches!(
            matched.unwrap().runner_scope,
            Some(RunnerScope::Repository { .. })
        ));
    }

    #[test]
    fn test_match_labels_empty() {
        let mappings: Vec<LabelMapping> = vec![];
        let job_labels = vec!["self-hosted".into()];
        assert!(match_labels(&job_labels, &mappings).is_none());

        let mappings = vec![LabelMapping {
            labels: vec!["self-hosted".into()],
            runner_scope: None, // Test with no runner_scope - will use repo from webhook
            runner_group: None,
        }];
        let job_labels: Vec<String> = vec![];
        assert!(match_labels(&job_labels, &mappings).is_none());
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
    }
}
