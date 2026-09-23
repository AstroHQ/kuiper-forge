-- failures reported by (or about) agents, for the admin ui. capped per agent on insert
CREATE TABLE IF NOT EXISTS agent_failures (
    id TEXT PRIMARY KEY NOT NULL,
    agent_id TEXT NOT NULL,
    occurred_at TEXT NOT NULL,
    kind TEXT NOT NULL,
    runner_name TEXT,
    job_id BIGINT,
    message TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_agent_failures_agent ON agent_failures(agent_id, occurred_at);
