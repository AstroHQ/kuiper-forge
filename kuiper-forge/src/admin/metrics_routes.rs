//! Admin UI handler for the metrics page, and the agent page's performance card.

use crate::admin::middleware::AdminState;
use crate::admin::routes::check_auth;
use crate::admin::templates::{
    AgentPerformance, BaseContext, InstallView, JobSummaryView, MetricsTemplate, RunGroupView,
    StatView, StepColumn, StepView,
};
use crate::metrics::summary::{
    self, InstallGroup, JobSummary, Range, RunGroup, Stat, StepStat, fmt_ms,
};
use askama::Template;
use axum::{
    extract::{Query, State},
    response::{Html, IntoResponse, Redirect, Response},
};
use axum_extra::extract::CookieJar;
use chrono::Utc;
use serde::Deserialize;
use std::sync::Arc;
use tracing::error;

#[derive(Deserialize)]
pub(crate) struct MetricsQuery {
    range: Option<String>,
}

pub(crate) async fn metrics_page(
    State(state): State<Arc<AdminState>>,
    jar: CookieJar,
    Query(query): Query<MetricsQuery>,
) -> Response {
    let Some(session) = check_auth(&state, &jar).await else {
        return Redirect::to("/admin/login").into_response();
    };

    let range = query
        .range
        .as_deref()
        .and_then(Range::parse)
        .unwrap_or(Range::Week);
    let now = Utc::now().timestamp_millis();
    let since = now - range.duration_ms();
    let runs = state.metrics.runs_since(since).await.unwrap_or_else(|e| {
        error!("Failed to load runner history: {}", e);
        Vec::new()
    });
    let jobs = state.metrics.jobs_since(since).await.unwrap_or_else(|e| {
        error!("Failed to load job history: {}", e);
        Vec::new()
    });

    let agents = summary::by_agent(&runs);
    let images = summary::by_image(&runs);
    let job_summary = summary::jobs(&jobs, &runs);
    let steps = summary::step_names(&runs);
    let charts = summary::charts(&runs, &jobs, &agents, &steps, range, now);
    let all = summary::total(&runs);

    let template = MetricsTemplate {
        base: BaseContext {
            username: session.username,
        },
        range: range.as_str(),
        ranges: Range::ALL.iter().map(|r| r.as_str()).collect(),
        runs: runs.len(),
        running: all.running,
        failures: all.failures,
        success: pct(all.success_pct),
        provision_p95: fmt_ms(all.provision.p95),
        wait_p50: fmt_ms(job_summary.wait.p50),
        agents: agents
            .iter()
            .map(|g| group_view(g, &steps, Some(format!("/admin/agents/{}", g.key))))
            .collect(),
        images: images.iter().map(|g| group_view(g, &steps, None)).collect(),
        installs: summary::by_install(&runs)
            .iter()
            .map(install_view)
            .collect(),
        steps: steps
            .iter()
            .map(|name| StepColumn {
                title: step_title(name),
                name: name.clone(),
            })
            .collect(),
        jobs: job_view(&job_summary),
        chart_json: script_json(&charts),
    };

    Html(
        template
            .render()
            .unwrap_or_else(|e| format!("Template error: {e}")),
    )
    .into_response()
}

/// The agent page's card: its runners over the last week.
pub(crate) async fn agent_performance(
    state: &AdminState,
    agent_id: &str,
) -> Option<AgentPerformance> {
    let since = Utc::now().timestamp_millis() - Range::Week.duration_ms();
    let runs = match state.metrics.runs_since(since).await {
        Ok(runs) => runs,
        Err(e) => {
            error!(
                "Failed to load runner history for agent {}: {}",
                agent_id, e
            );
            return None;
        }
    };
    let g = summary::by_agent(runs.iter().filter(|r| r.agent_id == agent_id))
        .into_iter()
        .next()?;
    Some(AgentPerformance {
        runs: g.runs,
        failures: g.failures,
        success: pct(g.success_pct),
        success_status: success_status(g.success_pct),
        failed_phases: failed_phases(&g),
        provision: stat_view(g.provision),
        boot: step_view(g.step("boot")),
        install: step_view(g.step("install")),
        job: step_view(g.step("job")),
    })
}

/// JSON for a `<script type="application/json">`. Hostnames and repo names end up in it, so a `</script>` in one
/// mustn't end the tag.
fn script_json(value: &impl serde::Serialize) -> String {
    serde_json::to_string(value)
        .unwrap_or_else(|_| "{}".to_string())
        .replace('<', "\\u003c")
}

fn pct(p: Option<f64>) -> String {
    p.map_or_else(|| "-".to_string(), |p| format!("{p:.1}%"))
}

fn success_status(p: Option<f64>) -> &'static str {
    match p {
        None => "idle",
        Some(p) if p >= 95.0 => "ok",
        Some(p) if p >= 80.0 => "warn",
        Some(_) => "bad",
    }
}

fn failed_phases(g: &RunGroup) -> String {
    g.failed_phases
        .iter()
        .map(|(phase, n)| format!("{phase} {n}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn stat_view(s: Stat) -> StatView {
    StatView {
        p50: fmt_ms(s.p50),
        p95: fmt_ms(s.p95),
        count: s.count,
    }
}

/// What a step covers, for its column's tooltip.
fn step_title(step: &str) -> &'static str {
    match step {
        "create" => "clone the VM",
        "boot" => "start the VM until SSH answers",
        "install" => "get the runner onto the VM, skipped when the image already has it",
        "setup" => "the rest of getting the runner ready: SSH, clock sync, runner config",
        "job" => "the runner's run",
        "destroy" => "stop and delete the VM",
        _ => "reported by newer agents",
    }
}

/// How an install source reads on the page, and what it means for its tooltip.
fn install_source_view(source: &str) -> (String, &'static str) {
    let (name, title) = match source {
        "hit" => ("Cache hit", "uploaded from the agent's copy"),
        "filled" => (
            "Filled cache",
            "uploaded from the agent's copy, after waiting for it to download",
        ),
        "fell_back" => (
            "Fell back",
            "the cache didn't work out so the VM downloaded it, the agent's log says why",
        ),
        "vm" => ("VM download", "the agent's cache is turned off"),
        summary::SOURCE_NOT_RECORDED => (
            "Not recorded",
            "from before agents reported this, or an older agent",
        ),
        summary::SOURCE_IMAGE => ("In image", "the image already has the runner"),
        other => return (other.to_string(), "reported by newer agents"),
    };
    (name.to_string(), title)
}

fn install_view(g: &InstallGroup) -> InstallView {
    let (source, source_title) = install_source_view(&g.source);
    InstallView {
        image: g.image.clone(),
        source,
        source_title,
        runs: g.runs,
        install: stat_view(g.install),
    }
}

fn step_view(s: StepStat) -> StepView {
    StepView {
        stat: stat_view(s.ran),
        skipped: s
            .skipped_pct()
            .map(|p| format!("{p:.0}% skipped"))
            .unwrap_or_default(),
    }
}

fn group_view(g: &RunGroup, steps: &[String], link: Option<String>) -> RunGroupView {
    RunGroupView {
        link,
        label: if g.label.is_empty() {
            g.key.clone()
        } else {
            g.label.clone()
        },
        detail: g.detail.clone(),
        runs: g.runs,
        running: g.running,
        completed: g.completed,
        failures: g.failures,
        rejected: g.rejected,
        stopped: g.stopped,
        success: pct(g.success_pct),
        success_status: success_status(g.success_pct),
        failed_phases: failed_phases(g),
        ack: stat_view(g.ack),
        provision: stat_view(g.provision),
        steps: steps.iter().map(|name| step_view(g.step(name))).collect(),
    }
}

fn job_view(s: &JobSummary) -> JobSummaryView {
    JobSummaryView {
        jobs: s.jobs,
        pending: s.pending,
        ran: s.ran,
        abandoned: s.abandoned,
        gone: s.gone,
        cancelled: s.cancelled,
        mean_attempts: s
            .mean_attempts
            .map_or_else(|| "-".to_string(), |a| format!("{a:.2}")),
        wait: stat_view(s.wait),
        to_coordinator: stat_view(s.to_coordinator),
        dispatch: stat_view(s.dispatch),
        provision: stat_view(s.provision),
        pickup: stat_view(s.pickup),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(installs: Vec<InstallView>) -> String {
        MetricsTemplate {
            base: BaseContext {
                username: "me".into(),
            },
            range: "7d",
            ranges: vec!["7d"],
            runs: 1,
            running: 0,
            failures: 0,
            success: "-".into(),
            provision_p95: "-".into(),
            wait_p50: "-".into(),
            steps: Vec::new(),
            agents: Vec::new(),
            images: Vec::new(),
            installs,
            jobs: job_view(&JobSummary::default()),
            chart_json: "{}".into(),
        }
        .render()
        .unwrap()
    }

    #[test]
    fn test_runner_installs_card() {
        let group = |source: &str, p50| InstallGroup {
            image: "ubuntu-24.04".into(),
            source: source.into(),
            runs: 12,
            install: Stat {
                count: 12,
                p50,
                p95: p50,
            },
        };
        let html = page(vec![
            install_view(&group("hit", Some(3_000))),
            install_view(&group(summary::SOURCE_NOT_RECORDED, Some(48_000))),
        ]);
        assert!(html.contains("Runner installs"));
        assert!(html.contains("ubuntu-24.04"));
        assert!(html.contains("Cache hit"));
        assert!(html.contains("Not recorded"));
        assert!(html.contains(&fmt_ms(Some(48_000))));

        assert!(!page(Vec::new()).contains("Runner installs"));
    }

    #[test]
    fn test_install_source_view() {
        assert_eq!(install_source_view("fell_back").0, "Fell back");
        // a newer agent's source still shows, under its own name
        assert_eq!(install_source_view("zz-new").0, "zz-new");
    }

    #[test]
    fn test_script_json_cant_close_the_tag() {
        let json = script_json(&vec!["</script><script>alert(1)</script>"]);
        assert!(!json.contains('<'));
        let back: Vec<String> = serde_json::from_str(&json).unwrap();
        assert_eq!(back[0], "</script><script>alert(1)</script>");
    }
}
