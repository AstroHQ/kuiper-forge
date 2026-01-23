-- Active runners table
-- Tracks runners created by the coordinator for crash recovery and cleanup
CREATE TABLE IF NOT EXISTS active_runners (
    runner_name TEXT PRIMARY KEY NOT NULL,
    agent_id TEXT NOT NULL,
    vm_name TEXT NOT NULL,
    runner_scope TEXT NOT NULL,  -- JSON-serialized RunnerScope
    created_at TEXT NOT NULL,
    job_id INTEGER  -- NULL for fixed capacity mode, set for webhook mode
);

CREATE INDEX IF NOT EXISTS idx_runners_agent_id ON active_runners(agent_id);
CREATE INDEX IF NOT EXISTS idx_runners_job_id ON active_runners(job_id);
