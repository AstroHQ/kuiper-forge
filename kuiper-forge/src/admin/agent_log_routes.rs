//! Admin UI handler for the agent logs tab.

use crate::admin::middleware::AdminState;
use crate::admin::routes::check_auth;
use crate::admin::templates::{AgentHeader, AgentLogsTemplate, BaseContext, LogLineView};
use crate::agent_logs::LogLevel;
use askama::Template;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
};
use axum_extra::extract::CookieJar;
use serde::Deserialize;
use std::sync::Arc;
use tracing::error;

const PAGE_SIZE: i64 = 500;

#[derive(Deserialize)]
pub(crate) struct LogsQuery {
    level: Option<String>,
    before: Option<String>,
    live: Option<String>,
}

/// Map the `level` query param to a filter. Defaults to everything the agent sent.
pub(crate) fn level_filter(level: Option<&str>) -> (LogLevel, &'static str) {
    match level.and_then(LogLevel::parse) {
        Some(LogLevel::Error) => (LogLevel::Error, "error"),
        Some(LogLevel::Warn) => (LogLevel::Warn, "warn"),
        Some(LogLevel::Info) => (LogLevel::Info, "info"),
        Some(LogLevel::Debug) => (LogLevel::Debug, "debug"),
        _ => (LogLevel::Trace, "trace"),
    }
}

pub(crate) async fn agent_logs(
    State(state): State<Arc<AdminState>>,
    jar: CookieJar,
    Path(agent_id): Path<String>,
    Query(query): Query<LogsQuery>,
) -> Response {
    let Some(session) = check_auth(&state, &jar).await else {
        return Redirect::to("/admin/login").into_response();
    };

    let Some(agent) = state
        .auth_manager
        .list_agents()
        .await
        .into_iter()
        .find(|a| a.agent_id == agent_id)
    else {
        return (StatusCode::NOT_FOUND, "Agent not found").into_response();
    };

    let (min_level, level) = level_filter(query.level.as_deref());
    let before = query.before.as_deref().filter(|b| !b.is_empty());
    let mut lines = match state
        .agent_logs
        .recent(&agent_id, min_level, before, PAGE_SIZE)
        .await
    {
        Ok(lines) => lines,
        Err(e) => {
            error!("Failed to load logs for agent {}: {:#}", agent_id, e);
            return (StatusCode::INTERNAL_SERVER_ERROR, "Failed to load logs").into_response();
        }
    };

    // a full page means there are probably more
    let older = (lines.len() as i64 == PAGE_SIZE)
        .then(|| lines.last().map(|l| l.cursor.clone()))
        .flatten();
    lines.reverse();

    let template = AgentLogsTemplate {
        base: BaseContext {
            username: session.username,
        },
        agent: AgentHeader {
            agent_id: agent.agent_id,
            revoked: agent.revoked,
        },
        tab: "logs",
        lines: lines
            .into_iter()
            .map(|l| LogLineView {
                ts: l.ts,
                level: l.level.as_str(),
                target: l.target,
                message: l.message,
            })
            .collect(),
        level,
        older,
        paged: before.is_some(),
        // live reload only makes sense on the newest page
        live: query.live.is_some() && before.is_none(),
    };

    Html(
        template
            .render()
            .unwrap_or_else(|e| format!("Template error: {e}")),
    )
    .into_response()
}
