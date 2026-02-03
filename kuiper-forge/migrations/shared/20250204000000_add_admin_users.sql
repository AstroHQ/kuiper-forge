-- Admin users table
CREATE TABLE IF NOT EXISTS admin_users (
    username TEXT PRIMARY KEY NOT NULL,
    password_hash TEXT NOT NULL,
    totp_secret TEXT,
    created_at TEXT NOT NULL,
    last_login TEXT
);

-- Admin sessions table
CREATE TABLE IF NOT EXISTS admin_sessions (
    session_id TEXT PRIMARY KEY NOT NULL,
    username TEXT NOT NULL,
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    ip_address TEXT,
    user_agent TEXT
);

CREATE INDEX IF NOT EXISTS idx_admin_sessions_expires ON admin_sessions(expires_at);
CREATE INDEX IF NOT EXISTS idx_admin_sessions_username ON admin_sessions(username);
