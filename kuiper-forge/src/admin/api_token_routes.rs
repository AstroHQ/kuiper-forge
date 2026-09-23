//! Admin UI handlers for managing API tokens.

use crate::admin::auth::AdminSession;
use crate::admin::middleware::AdminState;
use crate::admin::routes::check_auth;
use crate::admin::templates::{ApiTokenSummary, ApiTokensTemplate, BaseContext};
use askama::Template;
use axum::{
    Form,
    extract::State,
    http::StatusCode,
    response::{Html, IntoResponse, Redirect, Response},
};
use axum_extra::extract::CookieJar;
use serde::Deserialize;
use std::sync::Arc;
use tracing::{error, info};

/// What to show above the token list after an action.
#[derive(Default)]
struct PageMessage {
    new_token: Option<String>,
    error: Option<String>,
}

pub(crate) async fn api_tokens_page(
    State(state): State<Arc<AdminState>>,
    jar: CookieJar,
) -> Response {
    let Some(session) = check_auth(&state, &jar).await else {
        return Redirect::to("/admin/login").into_response();
    };
    render_api_tokens(&state, &session, PageMessage::default()).await
}

async fn render_api_tokens(
    state: &AdminState,
    session: &AdminSession,
    message: PageMessage,
) -> Response {
    let tokens = match state.api_tokens.list().await {
        Ok(tokens) => tokens,
        Err(e) => {
            error!("Failed to list API tokens: {:#}", e);
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                "Failed to list API tokens",
            )
                .into_response();
        }
    };

    let template = ApiTokensTemplate {
        base: BaseContext {
            username: session.username.clone(),
        },
        tokens: tokens
            .into_iter()
            .map(|t| ApiTokenSummary {
                id: t.id,
                name: t.name,
                token_prefix: t.token_prefix,
                created_by: t.created_by,
                created_at: t.created_at,
                last_used_at: t.last_used_at,
            })
            .collect(),
        new_token: message.new_token,
        error: message.error,
    };

    Html(
        template
            .render()
            .unwrap_or_else(|e| format!("Template error: {e}")),
    )
    .into_response()
}

#[derive(Deserialize)]
pub struct ApiTokenCreateForm {
    name: String,
}

pub(crate) async fn api_token_create(
    State(state): State<Arc<AdminState>>,
    jar: CookieJar,
    Form(form): Form<ApiTokenCreateForm>,
) -> Response {
    let Some(session) = check_auth(&state, &jar).await else {
        return Redirect::to("/admin/login").into_response();
    };

    let name = form.name.trim();
    if name.is_empty() || name.len() > 100 {
        let message = PageMessage {
            error: Some("Name must be 1-100 characters".to_string()),
            ..Default::default()
        };
        return render_api_tokens(&state, &session, message).await;
    }

    let message = match state.api_tokens.create(name, &session.username).await {
        Ok((token, record)) => {
            info!(
                "Admin '{}' created API token '{}' ({})",
                session.username, record.name, record.token_prefix
            );
            PageMessage {
                new_token: Some(token),
                ..Default::default()
            }
        }
        Err(e) => {
            error!("Failed to create API token: {:#}", e);
            PageMessage {
                error: Some("Failed to create token".to_string()),
                ..Default::default()
            }
        }
    };
    render_api_tokens(&state, &session, message).await
}

#[derive(Deserialize)]
pub struct ApiTokenDeleteForm {
    id: String,
}

pub(crate) async fn api_token_delete(
    State(state): State<Arc<AdminState>>,
    jar: CookieJar,
    Form(form): Form<ApiTokenDeleteForm>,
) -> Response {
    let Some(session) = check_auth(&state, &jar).await else {
        return Redirect::to("/admin/login").into_response();
    };

    match state.api_tokens.delete(&form.id).await {
        Ok(true) => info!("Admin '{}' deleted API token {}", session.username, form.id),
        Ok(false) => {}
        Err(e) => {
            error!("Failed to delete API token {}: {:#}", form.id, e);
            let message = PageMessage {
                error: Some("Failed to delete token".to_string()),
                ..Default::default()
            };
            return render_api_tokens(&state, &session, message).await;
        }
    }

    Redirect::to("/admin/api-tokens").into_response()
}
