//! Admin route handlers.
//!
//! Provides HTTP handlers for the admin UI: login, logout, dashboard, agents.

use crate::admin::auth::AdminSession;
use crate::admin::middleware::{AdminState, SESSION_COOKIE};
use crate::admin::templates::{
    AgentDetailTemplate, AgentSummary, BaseContext, DashboardTemplate, LoginTemplate,
    RunnerSummary, TokenSummary, UserSummary, UsersTemplate,
};
use crate::agent_registry::AgentInfo;
use askama::Template;
use axum::{
    Form, Router,
    extract::{Path, State},
    http::{StatusCode, header},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use axum_extra::extract::CookieJar;
use chrono::Duration;
use serde::Deserialize;
use std::collections::HashMap;
use std::sync::Arc;
use tracing::{error, info};

/// Build the admin router.
pub fn admin_router(state: Arc<AdminState>) -> Router {
    Router::new()
        .route("/login", get(login_page))
        .route("/login", post(login_submit))
        .route("/dashboard", get(dashboard))
        .route("/logout", post(logout))
        .route("/tokens/create", post(token_create))
        .route("/tokens/{token}/delete", post(token_delete))
        .route("/agents/{agent_id}", get(agent_detail))
        .route("/agents/{agent_id}/revoke", post(agent_revoke))
        .route("/users", get(users_page))
        .route("/users/create", post(user_create))
        .route("/users/password", post(user_set_password))
        .route("/users/delete", post(user_delete))
        .with_state(state)
}

/// Check session and return user if authenticated.
async fn check_auth(state: &AdminState, jar: &CookieJar) -> Option<AdminSession> {
    let session_id = jar.get(SESSION_COOKIE)?.value().to_string();
    state.auth_store.validate_session(&session_id).await.ok()?
}

/// Login page handler.
async fn login_page(State(state): State<Arc<AdminState>>, jar: CookieJar) -> Response {
    // If already logged in, redirect to dashboard
    if check_auth(&state, &jar).await.is_some() {
        return Redirect::to("/admin/dashboard").into_response();
    }

    let template = LoginTemplate { error: None };
    Html(
        template
            .render()
            .unwrap_or_else(|e| format!("Template error: {e}")),
    )
    .into_response()
}

/// Login form data.
#[derive(Deserialize)]
pub struct LoginForm {
    username: String,
    password: String,
}

/// Login form submission handler.
async fn login_submit(
    State(state): State<Arc<AdminState>>,
    _jar: CookieJar,
    Form(form): Form<LoginForm>,
) -> Response {
    // Attempt authentication
    let session_id = match state
        .auth_store
        .authenticate(
            &form.username,
            &form.password,
            state.session_timeout_secs,
            None,
            None,
        )
        .await
    {
        Ok(Some(session_id)) => session_id,
        Ok(None) => {
            let template = LoginTemplate {
                error: Some("Invalid username or password".to_string()),
            };
            return Html(
                template
                    .render()
                    .unwrap_or_else(|e| format!("Template error: {e}")),
            )
            .into_response();
        }
        Err(e) => {
            error!("Login error: {}", e);
            let template = LoginTemplate {
                error: Some("An error occurred. Please try again.".to_string()),
            };
            return Html(
                template
                    .render()
                    .unwrap_or_else(|e| format!("Template error: {e}")),
            )
            .into_response();
        }
    };

    // Set session cookie
    let cookie = format!("{SESSION_COOKIE}={session_id}; Path=/admin; HttpOnly; SameSite=Strict");

    Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header(header::LOCATION, "/admin/dashboard")
        .header(header::SET_COOKIE, cookie)
        .body(axum::body::Body::empty())
        .unwrap()
}

/// Logout handler.
async fn logout(State(state): State<Arc<AdminState>>, jar: CookieJar) -> Response {
    // Delete session from database
    if let Some(session) = check_auth(&state, &jar).await
        && let Err(e) = state.auth_store.delete_session(&session.session_id).await
    {
        error!("Failed to delete session: {}", e);
    }

    // Clear cookie by setting it to expire in the past
    let cookie = format!("{SESSION_COOKIE}=; Path=/admin; HttpOnly; SameSite=Strict; Max-Age=0");

    Response::builder()
        .status(StatusCode::SEE_OTHER)
        .header(header::LOCATION, "/admin/login")
        .header(header::SET_COOKIE, cookie)
        .body(axum::body::Body::empty())
        .unwrap()
}

/// Dashboard handler.
async fn dashboard(State(state): State<Arc<AdminState>>, jar: CookieJar) -> Response {
    render_dashboard(&state, &jar, None).await
}

/// Render dashboard with optional new token to display.
async fn render_dashboard(
    state: &AdminState,
    jar: &CookieJar,
    new_token: Option<String>,
) -> Response {
    let session = match check_auth(state, jar).await {
        Some(s) => s,
        None => return Redirect::to("/admin/login").into_response(),
    };

    let base = BaseContext {
        username: session.username.clone(),
    };

    let connected_agents = state.agent_registry.count().await;
    let active_runners = state
        .runner_state
        .get_all_runners()
        .await
        .map(|r| r.len())
        .unwrap_or(0);
    let pending_jobs = state
        .pending_jobs
        .get_all_pending_jobs()
        .await
        .map(|j| j.len())
        .unwrap_or(0);

    // Get agents
    let registered = state.auth_manager.list_agents().await;
    let connected: HashMap<String, AgentInfo> = state
        .agent_registry
        .list_all()
        .await
        .into_iter()
        .map(|a| (a.agent_id.clone(), a))
        .collect();
    let runners = state
        .runner_state
        .get_all_runners()
        .await
        .unwrap_or_default();

    let agents: Vec<AgentSummary> = registered
        .into_iter()
        .map(|a| {
            let active_vms = runners
                .iter()
                .filter(|(_, r)| r.agent_id == a.agent_id)
                .count();
            let label_sets = connected
                .get(&a.agent_id)
                .map(|c| c.label_sets.clone())
                .unwrap_or_default();
            AgentSummary {
                agent_id: a.agent_id.clone(),
                hostname: a.hostname,
                agent_type: a.agent_type,
                label_sets,
                max_vms: a.max_vms,
                is_online: connected.contains_key(&a.agent_id),
                active_vms,
                created_at: a.created_at,
                revoked: a.revoked,
            }
        })
        .collect();

    // Get pending tokens
    let tokens: Vec<TokenSummary> = state
        .auth_manager
        .list_tokens()
        .await
        .into_iter()
        .map(|t| TokenSummary {
            token: t.token,
            expires_at: t.expires_at,
            created_by: t.created_by,
            created_at: t.created_at,
        })
        .collect();

    let template = DashboardTemplate {
        base,
        connected_agents,
        active_runners,
        pending_jobs,
        agents,
        tokens,
        new_token,
    };

    Html(
        template
            .render()
            .unwrap_or_else(|e| format!("Template error: {e}")),
    )
    .into_response()
}

/// Create a new registration token.
async fn token_create(State(state): State<Arc<AdminState>>, jar: CookieJar) -> Response {
    let session = match check_auth(&state, &jar).await {
        Some(s) => s,
        None => return Redirect::to("/admin/login").into_response(),
    };

    // Create token valid for 1 hour
    match state
        .auth_manager
        .create_registration_token(Duration::hours(1), &session.username)
        .await
    {
        Ok(token) => {
            // Build registration bundle like the CLI does
            let bundle = build_registration_bundle(
                &token.token,
                &state.server_trust,
                &state.coordinator_url,
            );
            render_dashboard(&state, &jar, Some(bundle)).await
        }
        Err(e) => {
            error!("Failed to create registration token: {}", e);
            Redirect::to("/admin/dashboard").into_response()
        }
    }
}

/// Build a registration bundle from token and server trust info.
fn build_registration_bundle(
    token: &str,
    server_trust: &crate::tls::ServerTrust,
    coordinator_url: &str,
) -> String {
    use base64::Engine;

    let mut bundle_map = serde_json::Map::new();
    bundle_map.insert(
        "t".to_string(),
        serde_json::Value::String(token.to_string()),
    );

    if let Some(ref ca_pem) = server_trust.server_ca_pem {
        bundle_map.insert("ca".to_string(), serde_json::Value::String(ca_pem.clone()));
    }

    let trust_mode = match server_trust.server_trust_mode {
        crate::config::ServerTrustMode::Ca => "ca",
        crate::config::ServerTrustMode::Chain => "chain",
    };
    bundle_map.insert(
        "m".to_string(),
        serde_json::Value::String(trust_mode.to_string()),
    );
    bundle_map.insert(
        "u".to_string(),
        serde_json::Value::String(coordinator_url.to_string()),
    );

    let bundle_json = serde_json::Value::Object(bundle_map);
    let encoded =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bundle_json.to_string().as_bytes());
    format!("kfr1_{encoded}")
}

/// Delete a registration token.
async fn token_delete(
    State(state): State<Arc<AdminState>>,
    jar: CookieJar,
    Path(token): Path<String>,
) -> Response {
    if check_auth(&state, &jar).await.is_none() {
        return Redirect::to("/admin/login").into_response();
    }

    if let Err(e) = state.auth_manager.delete_token(&token).await {
        error!("Failed to delete token: {}", e);
    }

    Redirect::to("/admin/dashboard").into_response()
}

/// Agent detail handler.
async fn agent_detail(
    State(state): State<Arc<AdminState>>,
    jar: CookieJar,
    Path(agent_id): Path<String>,
) -> Response {
    let session = match check_auth(&state, &jar).await {
        Some(s) => s,
        None => return Redirect::to("/admin/login").into_response(),
    };

    let base = BaseContext {
        username: session.username.clone(),
    };

    let registered = state.auth_manager.list_agents().await;
    let agent = match registered.into_iter().find(|a| a.agent_id == agent_id) {
        Some(a) => a,
        None => {
            return (StatusCode::NOT_FOUND, "Agent not found").into_response();
        }
    };

    let connected: HashMap<String, AgentInfo> = state
        .agent_registry
        .list_all()
        .await
        .into_iter()
        .map(|a| (a.agent_id.clone(), a))
        .collect();

    let all_runners = state
        .runner_state
        .get_all_runners()
        .await
        .unwrap_or_default();
    let runners: Vec<RunnerSummary> = all_runners
        .into_iter()
        .filter(|(_, r)| r.agent_id == agent_id)
        .map(|(name, r)| RunnerSummary {
            runner_name: name,
            vm_name: r.vm_name,
            created_at: r.created_at,
            job_id: r.job_id,
            job_name: r.job_name,
            repository: r.repository,
            workflow_name: r.workflow_name,
        })
        .collect();

    let label_sets = connected
        .get(&agent.agent_id)
        .map(|c| c.label_sets.clone())
        .unwrap_or_default();
    let agent_summary = AgentSummary {
        agent_id: agent.agent_id.clone(),
        hostname: agent.hostname,
        agent_type: agent.agent_type,
        label_sets,
        max_vms: agent.max_vms,
        is_online: connected.contains_key(&agent.agent_id),
        active_vms: runners.len(),
        created_at: agent.created_at,
        revoked: agent.revoked,
    };

    let template = AgentDetailTemplate {
        base,
        agent: agent_summary,
        runners,
    };

    Html(
        template
            .render()
            .unwrap_or_else(|e| format!("Template error: {e}")),
    )
    .into_response()
}

/// Agent revoke handler.
async fn agent_revoke(
    State(state): State<Arc<AdminState>>,
    jar: CookieJar,
    Path(agent_id): Path<String>,
) -> Response {
    if check_auth(&state, &jar).await.is_none() {
        return Redirect::to("/admin/login").into_response();
    }

    match state.auth_manager.revoke_agent(&agent_id).await {
        Ok(true) => Redirect::to("/admin/dashboard").into_response(),
        Ok(false) => (StatusCode::NOT_FOUND, "Agent not found").into_response(),
        Err(e) => {
            error!("Failed to revoke agent {}: {}", agent_id, e);
            (StatusCode::INTERNAL_SERVER_ERROR, "Failed to revoke agent").into_response()
        }
    }
}

/// Outcome message shown at the top of the users page.
enum UsersMessage {
    Notice(String),
    Error(String),
}

/// Users list handler.
async fn users_page(State(state): State<Arc<AdminState>>, jar: CookieJar) -> Response {
    let Some(session) = check_auth(&state, &jar).await else {
        return Redirect::to("/admin/login").into_response();
    };
    render_users(&state, &session, None).await
}

async fn render_users(
    state: &AdminState,
    session: &AdminSession,
    message: Option<UsersMessage>,
) -> Response {
    let users = match state.auth_store.list_users().await {
        Ok(users) => users,
        Err(e) => {
            error!("Failed to list admin users: {}", e);
            return (StatusCode::INTERNAL_SERVER_ERROR, "Failed to list users").into_response();
        }
    };

    let users = users
        .into_iter()
        .map(|u| UserSummary {
            is_self: u.username == session.username,
            username: u.username,
            created_at: u.created_at,
            last_login: u.last_login,
        })
        .collect();

    let (notice, error) = match message {
        Some(UsersMessage::Notice(n)) => (Some(n), None),
        Some(UsersMessage::Error(e)) => (None, Some(e)),
        None => (None, None),
    };

    let template = UsersTemplate {
        base: BaseContext {
            username: session.username.clone(),
        },
        users,
        notice,
        error,
    };

    Html(
        template
            .render()
            .unwrap_or_else(|e| format!("Template error: {e}")),
    )
    .into_response()
}

/// Returns an error message if the password pair isn't usable.
fn check_new_password(password: &str, confirm: &str) -> Option<&'static str> {
    if password.is_empty() {
        Some("Password cannot be empty")
    } else if password != confirm {
        Some("Passwords do not match")
    } else {
        None
    }
}

/// Create user form data.
#[derive(Deserialize)]
pub struct UserCreateForm {
    username: String,
    password: String,
    confirm: String,
}

/// Create a new admin user.
async fn user_create(
    State(state): State<Arc<AdminState>>,
    jar: CookieJar,
    Form(form): Form<UserCreateForm>,
) -> Response {
    let Some(session) = check_auth(&state, &jar).await else {
        return Redirect::to("/admin/login").into_response();
    };

    let username = form.username.trim();

    // stricter than the cli on purpose, keeps names safe to drop into html/urls/logs
    let valid_name = !username.is_empty()
        && username.len() <= 64
        && username
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@'));
    let problem = if !valid_name {
        Some("Username must be 1-64 characters: letters, digits, . _ - @")
    } else {
        check_new_password(&form.password, &form.confirm)
    };
    if let Some(problem) = problem {
        return render_users(
            &state,
            &session,
            Some(UsersMessage::Error(problem.to_string())),
        )
        .await;
    }

    let message = match state.auth_store.get_user(username).await {
        Ok(Some(_)) => UsersMessage::Error(format!("User '{username}' already exists")),
        Ok(None) => match state.auth_store.create_user(username, &form.password).await {
            Ok(()) => {
                info!(
                    "Admin '{}' created admin user '{}'",
                    session.username, username
                );
                UsersMessage::Notice(format!("User '{username}' created"))
            }
            Err(e) => {
                error!("Failed to create admin user: {:#}", e);
                UsersMessage::Error("Failed to create user".to_string())
            }
        },
        Err(e) => {
            error!("Failed to look up admin user: {:#}", e);
            UsersMessage::Error("Failed to create user".to_string())
        }
    };
    render_users(&state, &session, Some(message)).await
}

/// Set password form data.
#[derive(Deserialize)]
pub struct UserPasswordForm {
    username: String,
    password: String,
    confirm: String,
}

/// Set an admin user's password and log out their other sessions.
async fn user_set_password(
    State(state): State<Arc<AdminState>>,
    jar: CookieJar,
    Form(form): Form<UserPasswordForm>,
) -> Response {
    let Some(session) = check_auth(&state, &jar).await else {
        return Redirect::to("/admin/login").into_response();
    };

    if let Some(problem) = check_new_password(&form.password, &form.confirm) {
        return render_users(
            &state,
            &session,
            Some(UsersMessage::Error(problem.to_string())),
        )
        .await;
    }

    let username = &form.username;
    if let Err(e) = state
        .auth_store
        .update_password(username, &form.password)
        .await
    {
        error!("Failed to update password for '{}': {:#}", username, e);
        return render_users(
            &state,
            &session,
            Some(UsersMessage::Error("Failed to update password".to_string())),
        )
        .await;
    }

    // keep the caller logged in when they change their own password
    let keep = (*username == session.username).then_some(session.session_id.as_str());
    if let Err(e) = state.auth_store.delete_user_sessions(username, keep).await {
        error!("Failed to clear sessions for '{}': {:#}", username, e);
    }

    info!(
        "Admin '{}' set password for admin user '{}'",
        session.username, username
    );
    render_users(
        &state,
        &session,
        Some(UsersMessage::Notice(format!(
            "Password updated for '{username}'"
        ))),
    )
    .await
}

/// Delete user form data.
#[derive(Deserialize)]
pub struct UserDeleteForm {
    username: String,
}

/// Delete an admin user.
async fn user_delete(
    State(state): State<Arc<AdminState>>,
    jar: CookieJar,
    Form(form): Form<UserDeleteForm>,
) -> Response {
    let Some(session) = check_auth(&state, &jar).await else {
        return Redirect::to("/admin/login").into_response();
    };

    // also guarantees at least one admin always remains
    if form.username == session.username {
        return render_users(
            &state,
            &session,
            Some(UsersMessage::Error(
                "You cannot delete your own account".to_string(),
            )),
        )
        .await;
    }

    let message = match state.auth_store.delete_user(&form.username).await {
        Ok(()) => {
            info!(
                "Admin '{}' deleted admin user '{}'",
                session.username, form.username
            );
            UsersMessage::Notice(format!("User '{}' deleted", form.username))
        }
        Err(e) => {
            error!("Failed to delete admin user '{}': {:#}", form.username, e);
            UsersMessage::Error("Failed to delete user".to_string())
        }
    };
    render_users(&state, &session, Some(message)).await
}
