-- Registration tokens table
CREATE TABLE IF NOT EXISTS registration_tokens (
    token TEXT PRIMARY KEY NOT NULL,
    expires_at TEXT NOT NULL,
    created_by TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_tokens_expires_at ON registration_tokens(expires_at);

-- Registered agents table
CREATE TABLE IF NOT EXISTS registered_agents (
    agent_id TEXT PRIMARY KEY NOT NULL,
    hostname TEXT NOT NULL,
    agent_type TEXT NOT NULL,
    labels TEXT NOT NULL,
    max_vms INTEGER NOT NULL,
    serial_number TEXT NOT NULL,
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    revoked INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_agents_revoked_expires ON registered_agents(revoked, expires_at);
