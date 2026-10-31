//! Admin UI handler for the recent errors page, which lists failures across all agents.

use crate::admin::middleware::AdminState;
use crate::admin::routes::{check_auth, format_age};
use crate::admin::templates::{
    AgentFailureView, BaseContext, ErrorCountRow, ErrorsTemplate, FailureSummary,
};
use crate::agent_failures::FailureKind;
use askama::Template;
use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
};
use axum_extra::extract::CookieJar;
use chrono::{Duration, Utc};
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::error;

const LIMIT: i64 = 200;

#[derive(Deserialize)]
pub(crate) struct ErrorsQuery {
    kind: Option<String>,
    agent: Option<String>,
}

pub(crate) async fn errors_page(
    State(state): State<Arc<AdminState>>,
    jar: CookieJar,
    Query(query): Query<ErrorsQuery>,
) -> Response {
    let Some(session) = check_auth(&state, &jar).await else {
        return Redirect::to("/admin/login").into_response();
    };

    // unknown kinds fall back to everything rather than an empty page
    let kind = query
        .kind
        .as_deref()
        .and_then(|k| FailureKind::ALL.into_iter().find(|f| f.as_str() == k))
        .map(FailureKind::as_str);
    let agent = query.agent.as_deref().filter(|a| !a.is_empty());

    let failures = match state.agent_failures.recent_all(kind, agent, LIMIT).await {
        Ok(f) => f,
        Err(e) => {
            error!(?kind, ?agent, "Failed to load recent failures: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "Failed to load errors").into_response();
        }
    };

    let now = Utc::now();
    let counts = state
        .agent_failures
        .counts_since(now - Duration::hours(24))
        .await
        .unwrap_or_else(|e| {
            error!("Failed to count recent failures: {e}");
            Vec::new()
        });

    let mut agents: Vec<(String, String)> = state
        .auth_manager
        .list_agents()
        .await
        .into_iter()
        .map(|a| (a.agent_id, a.hostname))
        .collect();
    agents.sort_by(|a, b| a.1.cmp(&b.1));
    let hostnames: HashMap<&str, &str> = agents
        .iter()
        .map(|(id, host)| (id.as_str(), host.as_str()))
        .collect();
    let hostname = |id: &str| hostnames.get(id).copied().unwrap_or(id).to_owned();

    let last_day = count_rows(&counts, &hostname);
    let last_day_totals = FailureKind::ALL
        .iter()
        .enumerate()
        .map(|(i, _)| last_day.iter().map(|r| r.counts[i]).sum())
        .collect();

    let failures = failures
        .into_iter()
        .map(|f| AgentFailureView {
            hostname: hostname(&f.agent_id),
            agent_id: f.agent_id,
            failure: FailureSummary {
                ago: format_age(now - f.occurred_at),
                occurred_at: f.occurred_at,
                kind: FailureKind::label(&f.kind),
                runner_name: f.runner_name,
                job_id: f.job_id,
                message: f.message,
            },
        })
        .collect();

    let template = ErrorsTemplate {
        base: BaseContext {
            username: session.username,
        },
        kind: kind.unwrap_or_default().to_owned(),
        agent: agent.unwrap_or_default().to_owned(),
        kinds: FailureKind::ALL
            .iter()
            .map(|k| (k.as_str(), FailureKind::label(k.as_str())))
            .collect(),
        last_day,
        last_day_totals,
        failures,
        limit: LIMIT as usize,
        agents,
    };

    Html(
        template
            .render()
            .unwrap_or_else(|e| format!("Template error: {e}")),
    )
    .into_response()
}

/// One row per agent from `(agent_id, kind, count)`, columns in `FailureKind::ALL` order, most failures first.
/// Kinds this build doesn't know are only counted in the total.
fn count_rows(
    counts: &[(String, String, i64)],
    hostname: &impl Fn(&str) -> String,
) -> Vec<ErrorCountRow> {
    let mut rows: HashMap<&str, ErrorCountRow> = HashMap::new();
    for (agent_id, kind, n) in counts {
        let row = rows
            .entry(agent_id.as_str())
            .or_insert_with(|| ErrorCountRow {
                agent_id: agent_id.clone(),
                hostname: hostname(agent_id),
                total: 0,
                counts: vec![0; FailureKind::ALL.len()],
            });
        row.total += n;
        if let Some(i) = FailureKind::ALL.iter().position(|k| k.as_str() == kind) {
            row.counts[i] += n;
        }
    }
    let mut rows: Vec<_> = rows.into_values().collect();
    rows.sort_by(|a, b| b.total.cmp(&a.total).then(a.hostname.cmp(&b.hostname)));
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_count_rows() {
        let counts = vec![
            ("a1".to_owned(), "runner_lost".to_owned(), 2),
            ("a2".to_owned(), "runner_failed".to_owned(), 1),
            ("a2".to_owned(), "runner_lost".to_owned(), 3),
            ("a2".to_owned(), "from_the_future".to_owned(), 1),
        ];
        let rows = count_rows(&counts, &|id| format!("host-{id}"));
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].agent_id, "a2");
        assert_eq!(rows[0].hostname, "host-a2");
        assert_eq!(rows[0].total, 5);
        assert_eq!(rows[0].counts, [1, 3, 0, 0]);
        assert_eq!(rows[1].counts, [0, 2, 0, 0]);
    }

    // nye: the "All agents" total must match the sum of the agent rows, unknown kinds included
    #[test]
    fn test_all_agents_total_includes_unknown_kinds() {
        let counts = vec![
            ("a1".to_owned(), "runner_lost".to_owned(), 2),
            ("a2".to_owned(), "runner_lost".to_owned(), 3),
            ("a2".to_owned(), "from_the_future".to_owned(), 1),
        ];
        let last_day = count_rows(&counts, &|id| format!("host-{id}"));
        let last_day_totals = FailureKind::ALL
            .iter()
            .enumerate()
            .map(|(i, _)| last_day.iter().map(|r| r.counts[i]).sum())
            .collect();
        let html = ErrorsTemplate {
            base: BaseContext {
                username: "u".into(),
            },
            kind: String::new(),
            agent: String::new(),
            kinds: FailureKind::ALL
                .iter()
                .map(|k| (k.as_str(), FailureKind::label(k.as_str())))
                .collect(),
            agents: vec![],
            last_day,
            last_day_totals,
            failures: vec![],
            limit: 200,
        }
        .render()
        .unwrap();
        let all = &html[html.find("All agents").unwrap()..];
        let total: String = all
            .split("<td>")
            .nth(1)
            .unwrap()
            .chars()
            .take_while(|c| c.is_ascii_digit())
            .collect();
        assert_eq!(total, "6", "rows total 2 + 4 = 6");
    }
}
