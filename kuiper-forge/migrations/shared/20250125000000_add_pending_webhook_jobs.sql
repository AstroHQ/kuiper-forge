-- Pending webhook jobs table
-- Persists jobs waiting for agent capacity so they survive coordinator restarts
-- and aren't lost if the webhook channel is full
CREATE TABLE IF NOT EXISTS pending_webhook_jobs (
    job_id INTEGER PRIMARY KEY NOT NULL,
    job_labels TEXT NOT NULL,      -- JSON array of labels from the job
    agent_labels TEXT NOT NULL,    -- JSON array of labels for agent matching
    runner_scope TEXT NOT NULL,    -- JSON-serialized RunnerScope
    runner_group TEXT,             -- Optional runner group
    created_at TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_pending_jobs_created_at ON pending_webhook_jobs(created_at);
