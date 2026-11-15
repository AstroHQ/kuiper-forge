//! Forgejo webhook mode, driven by real Forgejo 17 deliveries (`tests/fixtures/forgejo-17`).
#![cfg(feature = "sqlite")]

use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use kuiper_forge::config::{
    Config, DatabaseConfig, ForgeKindConfig, LabelMapping, ProvisioningMode, RunnerScope,
    WebhookConfig,
};
use kuiper_forge::db::Database;
use kuiper_forge::forge::JobKey;
use kuiper_forge::metrics::MetricsStore;
use kuiper_forge::pending_jobs::PendingJobStore;
use kuiper_forge::webhook::{
    WebhookEvent, WebhookNotifier, WebhookRunnerRequest, WebhookSource, WebhookState,
    validate_forgejo_signature, webhook_router,
};
use serde_json::Value;
use tempfile::TempDir;
use tokio::sync::mpsc;
use tower::ServiceExt;

/// The secret the fixtures were signed with
const SECRET: &str = "test-webhook-secret";
const FORGE: &str = "fj";
/// Handles of the fixtures' two jobs: `build`, and `fails` (which `needs: build`)
const BUILD: &str = "b93ebf9d-946f-404a-a7f7-67bc8fc15e92";
const FAILS: &str = "a7104b1c-328a-4de9-b4b5-e152c2807e70";

struct Delivery {
    headers: Vec<(String, String)>,
    body: String,
}

impl Delivery {
    fn header(&self, name: &str) -> &str {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
            .unwrap()
    }

    fn without(mut self, name: &str) -> Self {
        self.headers.retain(|(k, _)| !k.eq_ignore_ascii_case(name));
        self
    }

    fn with(self, name: &str, value: &str) -> Self {
        let mut d = self.without(name);
        d.headers.push((name.to_string(), value.to_string()));
        d
    }
}

fn fixture(name: &str) -> Delivery {
    let path = format!(
        "{}/tests/fixtures/forgejo-17/{name}.json",
        env!("CARGO_MANIFEST_DIR")
    );
    let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let headers = v["headers"]
        .as_object()
        .unwrap()
        .iter()
        .filter(|(k, _)| {
            !k.eq_ignore_ascii_case("content-length") && !k.eq_ignore_ascii_case("host")
        })
        .map(|(k, v)| (k.clone(), v.as_str().unwrap().to_string()))
        .collect();
    Delivery {
        headers,
        body: v["body"].as_str().unwrap().to_string(),
    }
}

struct Harness {
    _temp: TempDir,
    store: Arc<PendingJobStore>,
    metrics: Arc<MetricsStore>,
    events: mpsc::Receiver<WebhookEvent>,
    router: Router,
}

async fn harness_with(source: WebhookSource, config: WebhookConfig) -> Harness {
    let temp = TempDir::new().unwrap();
    let db = Database::new(&DatabaseConfig::default(), temp.path())
        .await
        .unwrap();
    let store = Arc::new(PendingJobStore::new(db.pool()));
    let metrics = Arc::new(MetricsStore::new(db.pool()));
    let (tx, events) = mpsc::channel(16);
    let state = Arc::new(WebhookState {
        config,
        forge: FORGE.to_string(),
        source,
        notifier: WebhookNotifier::new(tx),
        pending_job_store: store.clone(),
        metrics: metrics.clone(),
    });
    Harness {
        _temp: temp,
        store,
        metrics,
        events,
        router: webhook_router(state),
    }
}

fn webhook_config() -> WebhookConfig {
    WebhookConfig {
        path: "/webhook".into(),
        secret: SECRET.into(),
        required_labels: vec!["self-hosted".into()],
        label_mappings: Vec::new(),
        queued_scan_lookback_hours: 168,
        forge: Some(FORGE.into()),
    }
}

fn lodge() -> RunnerScope {
    RunnerScope::Organization {
        name: "lodge".into(),
    }
}

async fn forgejo_harness(scopes: Vec<RunnerScope>) -> Harness {
    harness_with(WebhookSource::Forgejo { scopes }, webhook_config()).await
}

impl Harness {
    async fn deliver(&self, d: &Delivery) -> StatusCode {
        let mut req = Request::post("/webhook");
        for (k, v) in &d.headers {
            req = req.header(k, v);
        }
        let req = req.body(Body::from(d.body.clone())).unwrap();
        self.router.clone().oneshot(req).await.unwrap().status()
    }

    async fn pending(&self) -> Vec<String> {
        self.store
            .get_all_pending_jobs()
            .await
            .unwrap()
            .into_iter()
            .map(|(k, _)| k.to_string())
            .collect()
    }

    fn next_event(&mut self) -> Option<WebhookEvent> {
        self.events.try_recv().ok()
    }
}

fn key(handle: &str) -> String {
    JobKey::new(FORGE, handle).to_string()
}

#[test]
fn forgejo_signature_checks_the_real_delivery() {
    let d = fixture("waiting");
    let sig = d.header("X-Forgejo-Signature");
    assert!(validate_forgejo_signature(SECRET, d.body.as_bytes(), sig));
    assert!(!validate_forgejo_signature("wrong", d.body.as_bytes(), sig));
    let tampered = d.body.replace("\"build\"", "\"bui1d\"");
    assert!(!validate_forgejo_signature(
        SECRET,
        tampered.as_bytes(),
        sig
    ));
    // GitHub's prefixed form isn't what Forgejo puts in X-Forgejo-Signature
    assert!(!validate_forgejo_signature(
        SECRET,
        d.body.as_bytes(),
        &format!("sha256={sig}")
    ));
    assert!(!validate_forgejo_signature(
        SECRET,
        d.body.as_bytes(),
        "not-hex"
    ));
    assert!(!validate_forgejo_signature(SECRET, d.body.as_bytes(), ""));
}

#[tokio::test]
async fn waiting_delivery_queues_one_job_keyed_by_handle() {
    let mut h = forgejo_harness(vec![lodge()]).await;
    assert_eq!(h.deliver(&fixture("waiting")).await, StatusCode::OK);
    assert_eq!(h.pending().await, vec![key(BUILD)]);
    assert!(matches!(
        h.next_event(),
        Some(WebhookEvent::NewJobAvailable)
    ));

    let (_, info) = h.store.get_all_pending_jobs().await.unwrap().remove(0);
    assert_eq!(info.runner_scope, lodge());
    assert_eq!(info.job_labels, vec!["self-hosted".to_string()]);
    assert_eq!(info.agent_labels, vec!["self-hosted".to_string()]);
    assert_eq!(info.repository.as_deref(), Some("lodge/app"));
    assert_eq!(info.job_name.as_deref(), Some("build"));

    // a redelivery (Forgejo retries, or the web UI's "Test delivery") adds nothing
    assert_eq!(h.deliver(&fixture("waiting")).await, StatusCode::OK);
    assert_eq!(h.pending().await, vec![key(BUILD)]);
}

#[tokio::test]
async fn webhook_and_poller_share_one_job() {
    let h = forgejo_harness(vec![lodge()]).await;
    // the recovery scan found the job first: the Forgejo poller keys it forge + handle
    let polled = WebhookRunnerRequest {
        job_labels: vec!["self-hosted".into()],
        agent_labels: vec!["self-hosted".into()],
        runner_scope: lodge(),
        runner_group: None,
        job_id: JobKey::new(FORGE, BUILD),
        repository: Some("lodge/app".into()),
        job_name: None,
        workflow_name: None,
        queued_at: None,
    };
    assert!(h.store.add_job(&polled).await);
    assert_eq!(h.deliver(&fixture("waiting")).await, StatusCode::OK);
    assert_eq!(h.pending().await, vec![key(BUILD)]);
}

#[tokio::test]
async fn bad_or_missing_signature_is_rejected() {
    let h = forgejo_harness(vec![lodge()]).await;
    let d = fixture("waiting");
    let wrong = "0".repeat(64);
    assert_eq!(
        h.deliver(&fixture("waiting").with("X-Forgejo-Signature", &wrong))
            .await,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        h.deliver(&fixture("waiting").without("X-Forgejo-Signature"))
            .await,
        StatusCode::UNAUTHORIZED
    );
    // GitHub's header alone doesn't count for a Forgejo forge
    let hub = d.header("X-Hub-Signature-256").to_string();
    assert_eq!(
        h.deliver(
            &fixture("waiting")
                .without("X-Forgejo-Signature")
                .with("X-Hub-Signature-256", &hub)
        )
        .await,
        StatusCode::UNAUTHORIZED
    );
    let mut tampered = fixture("waiting");
    tampered.body = tampered.body.replace("\"build\"", "\"bui1d\"");
    assert_eq!(h.deliver(&tampered).await, StatusCode::UNAUTHORIZED);
    assert!(h.pending().await.is_empty());
}

#[tokio::test]
async fn blocked_job_waits_for_its_waiting_delivery() {
    let mut h = forgejo_harness(vec![lodge()]).await;
    // `fails` arrives as a new_job_attempt that's blocked on `needs: build`...
    assert_eq!(h.deliver(&fixture("blocked")).await, StatusCode::OK);
    assert!(h.pending().await.is_empty());
    assert!(h.next_event().is_none());
    // ...and becomes waiting with job_status_changed once `build` is done
    assert_eq!(
        h.deliver(&fixture("waiting-after-needs")).await,
        StatusCode::OK
    );
    assert_eq!(h.pending().await, vec![key(FAILS)]);
}

#[tokio::test]
async fn running_and_completed_fill_the_job_stats() {
    let mut h = forgejo_harness(vec![lodge()]).await;
    assert_eq!(h.deliver(&fixture("waiting")).await, StatusCode::OK);
    let _ = h.next_event();
    assert_eq!(h.deliver(&fixture("running")).await, StatusCode::OK);
    assert_eq!(h.deliver(&fixture("success")).await, StatusCode::OK);
    match h.next_event() {
        Some(WebhookEvent::JobCompleted { job_id }) => assert_eq!(job_id.to_string(), key(BUILD)),
        other => panic!("expected JobCompleted, got {other:?}"),
    }
    let jobs = h.metrics.jobs_since(0).await.unwrap();
    let job = jobs
        .iter()
        .find(|j| j.job_id == key(BUILD))
        .expect("job in history");
    assert_eq!(job.forge, FORGE);
    assert!(job.started_at.is_some(), "running sets the start");
    assert!(job.completed_at.is_some(), "success sets the finish");
    assert_eq!(job.conclusion.as_deref(), Some("success"));
}

#[tokio::test]
async fn failure_and_cancelled_complete_the_job() {
    let mut h = forgejo_harness(vec![lodge()]).await;
    assert_eq!(
        h.deliver(&fixture("waiting-after-needs")).await,
        StatusCode::OK
    );
    let _ = h.next_event();
    assert_eq!(h.deliver(&fixture("failure")).await, StatusCode::OK);
    assert!(
        matches!(h.next_event(), Some(WebhookEvent::JobCompleted { job_id }) if job_id.to_string() == key(FAILS))
    );
    let jobs = h.metrics.jobs_since(0).await.unwrap();
    let job = jobs.iter().find(|j| j.job_id == key(FAILS)).unwrap();
    assert_eq!(job.conclusion.as_deref(), Some("failure"));

    assert_eq!(h.deliver(&fixture("cancelled")).await, StatusCode::OK);
    assert!(matches!(
        h.next_event(),
        Some(WebhookEvent::JobCompleted { .. })
    ));
}

#[tokio::test]
async fn job_outside_the_forges_scopes_is_ignored() {
    let h = forgejo_harness(vec![RunnerScope::Repository {
        owner: "kbot".into(),
        repo: "solo".into(),
    }])
    .await;
    assert_eq!(h.deliver(&fixture("waiting")).await, StatusCode::OK);
    assert!(h.pending().await.is_empty());
}

#[tokio::test]
async fn job_scope_follows_the_configured_scopes() {
    let repo = RunnerScope::Repository {
        owner: "lodge".into(),
        repo: "app".into(),
    };
    let h = forgejo_harness(vec![repo.clone()]).await;
    assert_eq!(h.deliver(&fixture("waiting")).await, StatusCode::OK);
    let (_, info) = h.store.get_all_pending_jobs().await.unwrap().remove(0);
    assert_eq!(info.runner_scope, repo);

    let h = forgejo_harness(vec![RunnerScope::Instance]).await;
    assert_eq!(h.deliver(&fixture("waiting")).await, StatusCode::OK);
    let (_, info) = h.store.get_all_pending_jobs().await.unwrap().remove(0);
    assert_eq!(info.runner_scope, RunnerScope::Instance);
}

#[tokio::test]
async fn required_labels_and_label_mappings_apply() {
    let mut config = webhook_config();
    config.required_labels = vec!["linux".into()];
    let h = harness_with(
        WebhookSource::Forgejo {
            scopes: vec![lodge()],
        },
        config,
    )
    .await;
    assert_eq!(h.deliver(&fixture("waiting")).await, StatusCode::OK);
    assert!(h.pending().await.is_empty());

    // a mapping for another forge never matches, so the job is ignored
    let mut config = webhook_config();
    config.label_mappings = vec![LabelMapping {
        labels: vec!["self-hosted".into()],
        runner_scope: None,
        runner_group: None,
        forge: Some("github".into()),
    }];
    let h = harness_with(
        WebhookSource::Forgejo {
            scopes: vec![lodge()],
        },
        config,
    )
    .await;
    assert_eq!(h.deliver(&fixture("waiting")).await, StatusCode::OK);
    assert!(h.pending().await.is_empty());

    // a mapping for this forge picks the runner group
    let mut config = webhook_config();
    config.label_mappings = vec![LabelMapping {
        labels: vec!["self-hosted".into()],
        runner_scope: None,
        runner_group: Some("7".into()),
        forge: Some(FORGE.into()),
    }];
    let h = harness_with(
        WebhookSource::Forgejo {
            scopes: vec![lodge()],
        },
        config,
    )
    .await;
    assert_eq!(h.deliver(&fixture("waiting")).await, StatusCode::OK);
    let (_, info) = h.store.get_all_pending_jobs().await.unwrap().remove(0);
    assert_eq!(info.runner_group.as_deref(), Some("7"));
    assert_eq!(info.runner_scope, lodge());
}

#[tokio::test]
async fn github_forge_ignores_forgejo_deliveries() {
    // GitHub behaviour is unchanged: it reads X-GitHub-Event, which Forgejo sets to
    // workflow_job_waiting rather than workflow_job
    let h = harness_with(WebhookSource::Github, webhook_config()).await;
    assert_eq!(h.deliver(&fixture("waiting")).await, StatusCode::OK);
    assert!(h.pending().await.is_empty());
}

const CONFIG: &str = r#"
[tls]
ca_cert = "/etc/ci-runner/ca.crt"
ca_key = "/etc/ci-runner/ca.key"
server_cert = "/etc/ci-runner/server.crt"
server_key = "/etc/ci-runner/server.key"

[[forges]]
name = "fj"
type = "forgejo"
url = "https://git.example.com"
token = "abc"
scopes = ["lodge"]

[provisioning]
mode = "webhook"

[provisioning.webhook]
secret = "s3cret"
forge = "fj"
"#;

fn load(text: &str) -> Config {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("config.toml");
    std::fs::write(&path, text).unwrap();
    Config::load(&path).unwrap()
}

#[test]
fn forgejo_webhook_config_is_accepted() {
    let config = load(CONFIG);
    assert_eq!(config.provisioning.mode, ProvisioningMode::Webhook);
    config.resolved_forges().expect("config validates");
    let webhook = config.provisioning.webhook.clone().unwrap();
    let forge = config
        .webhook_forge(&webhook)
        .expect("a Forgejo webhook forge is allowed");
    assert_eq!(forge.name, "fj");
    assert!(matches!(forge.kind, ForgeKindConfig::Forgejo(_)));
    match WebhookSource::for_forge(&forge).unwrap() {
        WebhookSource::Forgejo { scopes } => assert_eq!(scopes, vec![lodge()]),
        WebhookSource::Github => panic!("expected a Forgejo source"),
    }
    // the recovery scan still runs as a backup in this setup
    assert!(config.provisioning.on_demand_filters().is_some());

    // without `forge`, a lone Forgejo forge is the webhook forge
    let config = load(&CONFIG.replace("forge = \"fj\"\n", ""));
    let webhook = config.provisioning.webhook.clone().unwrap();
    assert_eq!(config.webhook_forge(&webhook).unwrap().name, "fj");

    // a forge that isn't configured is still an error
    let config = load(&CONFIG.replace("forge = \"fj\"", "forge = \"nope\""));
    let webhook = config.provisioning.webhook.clone().unwrap();
    assert!(config.webhook_forge(&webhook).is_err());
}
