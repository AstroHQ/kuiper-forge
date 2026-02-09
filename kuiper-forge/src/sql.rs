//! SQL query constants with database-specific placeholders.
//!
//! This module provides SQL queries that work with the selected database backend.
//! SQLite uses `?` placeholders, PostgreSQL uses `$1, $2, ...` numbered placeholders.

#[cfg(feature = "sqlite")]
pub const STORE_TOKEN: &str = r#"
    INSERT INTO registration_tokens (token, expires_at, created_by, created_at)
    VALUES (?, ?, ?, ?)
    ON CONFLICT(token) DO UPDATE SET
        expires_at = excluded.expires_at,
        created_by = excluded.created_by,
        created_at = excluded.created_at
"#;

#[cfg(feature = "postgres")]
pub const STORE_TOKEN: &str = r#"
    INSERT INTO registration_tokens (token, expires_at, created_by, created_at)
    VALUES ($1, $2, $3, $4)
    ON CONFLICT(token) DO UPDATE SET
        expires_at = excluded.expires_at,
        created_by = excluded.created_by,
        created_at = excluded.created_at
"#;

#[cfg(feature = "sqlite")]
pub const SELECT_TOKEN: &str =
    "SELECT token, expires_at, created_by, created_at FROM registration_tokens WHERE token = ?";

#[cfg(feature = "postgres")]
pub const SELECT_TOKEN: &str =
    "SELECT token, expires_at, created_by, created_at FROM registration_tokens WHERE token = $1";

#[cfg(feature = "sqlite")]
pub const DELETE_TOKEN: &str = "DELETE FROM registration_tokens WHERE token = ?";

#[cfg(feature = "postgres")]
pub const DELETE_TOKEN: &str = "DELETE FROM registration_tokens WHERE token = $1";

#[cfg(feature = "sqlite")]
pub const CONSUME_TOKEN: &str = "DELETE FROM registration_tokens WHERE token = ? RETURNING token, expires_at, created_by, created_at";

#[cfg(feature = "postgres")]
pub const CONSUME_TOKEN: &str = "DELETE FROM registration_tokens WHERE token = $1 RETURNING token, expires_at, created_by, created_at";

#[cfg(feature = "sqlite")]
pub const DELETE_EXPIRED_TOKENS: &str = "DELETE FROM registration_tokens WHERE expires_at < ?";

#[cfg(feature = "postgres")]
pub const DELETE_EXPIRED_TOKENS: &str = "DELETE FROM registration_tokens WHERE expires_at < $1";

#[cfg(feature = "sqlite")]
pub const STORE_AGENT: &str = r#"
    INSERT INTO registered_agents
        (agent_id, hostname, agent_type, labels, max_vms, serial_number, created_at, expires_at, revoked)
    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
    ON CONFLICT(agent_id) DO UPDATE SET
        hostname = excluded.hostname,
        agent_type = excluded.agent_type,
        labels = excluded.labels,
        max_vms = excluded.max_vms,
        serial_number = excluded.serial_number,
        created_at = excluded.created_at,
        expires_at = excluded.expires_at,
        revoked = excluded.revoked
"#;

#[cfg(feature = "postgres")]
pub const STORE_AGENT: &str = r#"
    INSERT INTO registered_agents
        (agent_id, hostname, agent_type, labels, max_vms, serial_number, created_at, expires_at, revoked)
    VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
    ON CONFLICT(agent_id) DO UPDATE SET
        hostname = excluded.hostname,
        agent_type = excluded.agent_type,
        labels = excluded.labels,
        max_vms = excluded.max_vms,
        serial_number = excluded.serial_number,
        created_at = excluded.created_at,
        expires_at = excluded.expires_at,
        revoked = excluded.revoked
"#;

#[cfg(feature = "sqlite")]
pub const SELECT_AGENT: &str = "SELECT * FROM registered_agents WHERE agent_id = ?";

#[cfg(feature = "postgres")]
pub const SELECT_AGENT: &str = "SELECT * FROM registered_agents WHERE agent_id = $1";

#[cfg(feature = "sqlite")]
pub const REVOKE_AGENT: &str = "UPDATE registered_agents SET revoked = 1 WHERE agent_id = ?";

#[cfg(feature = "postgres")]
pub const REVOKE_AGENT: &str = "UPDATE registered_agents SET revoked = 1 WHERE agent_id = $1";

#[cfg(feature = "sqlite")]
pub const CHECK_AGENT_VALID: &str =
    "SELECT 1 FROM registered_agents WHERE agent_id = ? AND revoked = 0 AND expires_at > ?";

#[cfg(feature = "postgres")]
pub const CHECK_AGENT_VALID: &str =
    "SELECT 1 FROM registered_agents WHERE agent_id = $1 AND revoked = 0 AND expires_at > $2";

// Active runners queries

#[cfg(feature = "sqlite")]
pub const INSERT_RUNNER: &str = r#"
    INSERT INTO active_runners (runner_name, agent_id, vm_name, runner_scope, created_at, job_id, job_name, repository, workflow_name)
    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
"#;

#[cfg(feature = "postgres")]
pub const INSERT_RUNNER: &str = r#"
    INSERT INTO active_runners (runner_name, agent_id, vm_name, runner_scope, created_at, job_id, job_name, repository, workflow_name)
    VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
"#;

#[cfg(feature = "sqlite")]
pub const DELETE_RUNNER: &str = "DELETE FROM active_runners WHERE runner_name = ?";

#[cfg(feature = "postgres")]
pub const DELETE_RUNNER: &str = "DELETE FROM active_runners WHERE runner_name = $1";

#[cfg(feature = "sqlite")]
pub const SELECT_RUNNER: &str = "SELECT runner_name, agent_id, vm_name, runner_scope, created_at, job_id, job_name, repository, workflow_name FROM active_runners WHERE runner_name = ?";

#[cfg(feature = "postgres")]
pub const SELECT_RUNNER: &str = "SELECT runner_name, agent_id, vm_name, runner_scope, created_at, job_id, job_name, repository, workflow_name FROM active_runners WHERE runner_name = $1";

#[cfg(feature = "sqlite")]
pub const SELECT_RUNNERS_BY_AGENT: &str = "SELECT runner_name, agent_id, vm_name, runner_scope, created_at, job_id, job_name, repository, workflow_name FROM active_runners WHERE agent_id = ?";

#[cfg(feature = "postgres")]
pub const SELECT_RUNNERS_BY_AGENT: &str = "SELECT runner_name, agent_id, vm_name, runner_scope, created_at, job_id, job_name, repository, workflow_name FROM active_runners WHERE agent_id = $1";

pub const SELECT_ALL_RUNNERS: &str =
    "SELECT runner_name, agent_id, vm_name, runner_scope, created_at, job_id, job_name, repository, workflow_name FROM active_runners";

#[cfg(feature = "sqlite")]
pub const SELECT_RUNNER_BY_JOB_ID: &str = "SELECT 1 FROM active_runners WHERE job_id = ?";

#[cfg(feature = "postgres")]
pub const SELECT_RUNNER_BY_JOB_ID: &str = "SELECT 1 FROM active_runners WHERE job_id = $1";

// Count runners for a specific agent
#[cfg(feature = "sqlite")]
pub const COUNT_RUNNERS_BY_AGENT: &str = "SELECT COUNT(*) as count FROM active_runners WHERE agent_id = ?";

#[cfg(feature = "postgres")]
pub const COUNT_RUNNERS_BY_AGENT: &str = "SELECT COUNT(*) as count FROM active_runners WHERE agent_id = $1";

// Bulk delete for agent disconnect cleanup
#[cfg(feature = "sqlite")]
pub const DELETE_RUNNERS_BY_AGENT: &str = "DELETE FROM active_runners WHERE agent_id = ?";

#[cfg(feature = "postgres")]
pub const DELETE_RUNNERS_BY_AGENT: &str = "DELETE FROM active_runners WHERE agent_id = $1";

// Pending webhook jobs queries

#[cfg(feature = "sqlite")]
pub const INSERT_PENDING_JOB: &str = r#"
    INSERT INTO pending_webhook_jobs (job_id, job_labels, agent_labels, runner_scope, runner_group, created_at, repository, job_name, workflow_name)
    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
    ON CONFLICT(job_id) DO NOTHING
"#;

#[cfg(feature = "postgres")]
pub const INSERT_PENDING_JOB: &str = r#"
    INSERT INTO pending_webhook_jobs (job_id, job_labels, agent_labels, runner_scope, runner_group, created_at, repository, job_name, workflow_name)
    VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
    ON CONFLICT(job_id) DO NOTHING
"#;

#[cfg(feature = "sqlite")]
pub const DELETE_PENDING_JOB: &str = "DELETE FROM pending_webhook_jobs WHERE job_id = ?";

#[cfg(feature = "postgres")]
pub const DELETE_PENDING_JOB: &str = "DELETE FROM pending_webhook_jobs WHERE job_id = $1";

#[cfg(feature = "sqlite")]
pub const SELECT_PENDING_JOB: &str = "SELECT job_id, job_labels, agent_labels, runner_scope, runner_group, created_at, retry_count, repository, job_name, workflow_name FROM pending_webhook_jobs WHERE job_id = ?";

#[cfg(feature = "postgres")]
pub const SELECT_PENDING_JOB: &str = "SELECT job_id, job_labels, agent_labels, runner_scope, runner_group, created_at, retry_count, repository, job_name, workflow_name FROM pending_webhook_jobs WHERE job_id = $1";

pub const SELECT_ALL_PENDING_JOBS: &str = "SELECT job_id, job_labels, agent_labels, runner_scope, runner_group, created_at, retry_count, repository, job_name, workflow_name FROM pending_webhook_jobs ORDER BY created_at ASC";

#[cfg(feature = "sqlite")]
pub const INCREMENT_PENDING_JOB_RETRY: &str =
    "UPDATE pending_webhook_jobs SET retry_count = retry_count + 1 WHERE job_id = ? RETURNING retry_count";

#[cfg(feature = "postgres")]
pub const INCREMENT_PENDING_JOB_RETRY: &str =
    "UPDATE pending_webhook_jobs SET retry_count = retry_count + 1 WHERE job_id = $1 RETURNING retry_count";

#[cfg(feature = "sqlite")]
pub const GET_PENDING_JOB_RETRY_COUNT: &str =
    "SELECT retry_count FROM pending_webhook_jobs WHERE job_id = ?";

#[cfg(feature = "postgres")]
pub const GET_PENDING_JOB_RETRY_COUNT: &str =
    "SELECT retry_count FROM pending_webhook_jobs WHERE job_id = $1";

// Admin users queries

#[cfg(feature = "sqlite")]
pub const INSERT_ADMIN_USER: &str = r#"
    INSERT INTO admin_users (username, password_hash, totp_secret, created_at)
    VALUES (?, ?, ?, ?)
"#;

#[cfg(feature = "postgres")]
pub const INSERT_ADMIN_USER: &str = r#"
    INSERT INTO admin_users (username, password_hash, totp_secret, created_at)
    VALUES ($1, $2, $3, $4)
"#;

#[cfg(feature = "sqlite")]
pub const SELECT_ADMIN_USER: &str =
    "SELECT username, password_hash, totp_secret, created_at, last_login FROM admin_users WHERE username = ?";

#[cfg(feature = "postgres")]
pub const SELECT_ADMIN_USER: &str =
    "SELECT username, password_hash, totp_secret, created_at, last_login FROM admin_users WHERE username = $1";

pub const SELECT_ALL_ADMIN_USERS: &str =
    "SELECT username, password_hash, totp_secret, created_at, last_login FROM admin_users ORDER BY username";

#[cfg(feature = "sqlite")]
pub const UPDATE_ADMIN_USER_LAST_LOGIN: &str =
    "UPDATE admin_users SET last_login = ? WHERE username = ?";

#[cfg(feature = "postgres")]
pub const UPDATE_ADMIN_USER_LAST_LOGIN: &str =
    "UPDATE admin_users SET last_login = $1 WHERE username = $2";

#[cfg(feature = "sqlite")]
pub const UPDATE_ADMIN_USER_PASSWORD: &str =
    "UPDATE admin_users SET password_hash = ? WHERE username = ?";

#[cfg(feature = "postgres")]
pub const UPDATE_ADMIN_USER_PASSWORD: &str =
    "UPDATE admin_users SET password_hash = $1 WHERE username = $2";

#[cfg(feature = "sqlite")]
pub const DELETE_ADMIN_USER: &str = "DELETE FROM admin_users WHERE username = ?";

#[cfg(feature = "postgres")]
pub const DELETE_ADMIN_USER: &str = "DELETE FROM admin_users WHERE username = $1";

// Admin sessions queries

#[cfg(feature = "sqlite")]
pub const INSERT_ADMIN_SESSION: &str = r#"
    INSERT INTO admin_sessions (session_id, username, created_at, expires_at, ip_address, user_agent)
    VALUES (?, ?, ?, ?, ?, ?)
"#;

#[cfg(feature = "postgres")]
pub const INSERT_ADMIN_SESSION: &str = r#"
    INSERT INTO admin_sessions (session_id, username, created_at, expires_at, ip_address, user_agent)
    VALUES ($1, $2, $3, $4, $5, $6)
"#;

#[cfg(feature = "sqlite")]
pub const SELECT_ADMIN_SESSION: &str =
    "SELECT session_id, username, created_at, expires_at, ip_address, user_agent FROM admin_sessions WHERE session_id = ?";

#[cfg(feature = "postgres")]
pub const SELECT_ADMIN_SESSION: &str =
    "SELECT session_id, username, created_at, expires_at, ip_address, user_agent FROM admin_sessions WHERE session_id = $1";

#[cfg(feature = "sqlite")]
pub const DELETE_ADMIN_SESSION: &str = "DELETE FROM admin_sessions WHERE session_id = ?";

#[cfg(feature = "postgres")]
pub const DELETE_ADMIN_SESSION: &str = "DELETE FROM admin_sessions WHERE session_id = $1";

#[cfg(feature = "sqlite")]
pub const DELETE_EXPIRED_ADMIN_SESSIONS: &str = "DELETE FROM admin_sessions WHERE expires_at < ?";

#[cfg(feature = "postgres")]
pub const DELETE_EXPIRED_ADMIN_SESSIONS: &str = "DELETE FROM admin_sessions WHERE expires_at < $1";

#[cfg(feature = "sqlite")]
pub const DELETE_ADMIN_SESSIONS_BY_USER: &str = "DELETE FROM admin_sessions WHERE username = ?";

#[cfg(feature = "postgres")]
pub const DELETE_ADMIN_SESSIONS_BY_USER: &str = "DELETE FROM admin_sessions WHERE username = $1";
