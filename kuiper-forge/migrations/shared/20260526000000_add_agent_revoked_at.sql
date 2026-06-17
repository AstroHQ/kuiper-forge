-- Track when an agent was revoked so revoked records can be purged after a
-- retention window instead of lingering in the database and UI forever.
ALTER TABLE registered_agents ADD COLUMN revoked_at TEXT;

-- Backfill existing revoked rows with a best-effort timestamp (their creation
-- time) so the purge can age them out. New revocations stamp the real time.
UPDATE registered_agents SET revoked_at = created_at WHERE revoked = 1 AND revoked_at IS NULL;

-- Speeds up the periodic purge of old revoked agents.
CREATE INDEX IF NOT EXISTS idx_agents_revoked_at ON registered_agents(revoked, revoked_at);
