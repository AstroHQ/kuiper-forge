//! Admin UI handlers for managing admin users.

use crate::admin::auth::AdminSession;
use crate::admin::middleware::AdminState;
use crate::admin::routes::check_auth;
use crate::admin::templates::{BaseContext, UserSummary, UsersTemplate};
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

/// Outcome message shown at the top of the users page.
enum UsersMessage {
    Notice(String),
    Error(String),
}

/// Users list handler.
pub(crate) async fn users_page(State(state): State<Arc<AdminState>>, jar: CookieJar) -> Response {
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
pub(crate) async fn user_create(
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
pub(crate) async fn user_set_password(
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
pub(crate) async fn user_delete(
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
