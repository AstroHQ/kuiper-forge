//! Web administration UI module.
//!
//! Provides:
//! - Admin user authentication (password + optional TOTP)
//! - Session management
//! - Admin routes for dashboard, agents, etc.
//! - API tokens and the read-only `/api/v1` endpoints

mod agent_log_routes;
pub mod api;
mod api_token_routes;
pub mod api_tokens;
pub mod auth;
pub mod middleware;
pub mod routes;
pub mod templates;
mod user_routes;

pub use api::api_router;
pub use api_tokens::ApiTokenStore;
pub use auth::AdminAuthStore;
pub use middleware::AdminState;
pub use routes::admin_router;
