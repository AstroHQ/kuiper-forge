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
