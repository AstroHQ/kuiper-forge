//! Web administration UI module.
//!
//! Provides:
//! - Admin user authentication (password + optional TOTP)
//! - Session management
//! - Admin routes for dashboard, agents, etc.

pub mod auth;
pub mod middleware;
pub mod routes;
pub mod templates;

pub use auth::AdminAuthStore;
pub use middleware::AdminState;
pub use routes::admin_router;
