-- history for the metrics page: one row per runner attempt and one per job, kept for metrics.retention_days.
-- times are unix millis so durations are a subtraction

CREATE TABLE IF NOT EXISTS runner_runs (
    runner_name TEXT PRIMARY KEY NOT NULL,
    agent_id TEXT NOT NULL,
    -- copied in, the agent row is purged after it's revoked
    hostname TEXT NOT NULL,
    agent_type TEXT NOT NULL,
    forge TEXT,
    job_id TEXT,
    repository TEXT,
    workflow_name TEXT,
    job_name TEXT,
    pool TEXT,
    labels TEXT NOT NULL,          -- JSON array
    requested_at BIGINT NOT NULL,  -- CreateRunner sent
    acked_at BIGINT,
    started_at BIGINT,             -- agent's STARTED event: runner set up, about to take the job
    job_started_at BIGINT,         -- the forge says the job is running
    finished_at BIGINT,
    outcome TEXT,                  -- completed, failed, lost, rejected, command_failed, idled, drained, destroyed
    error TEXT,
    image TEXT,
    failed_phase TEXT,
    -- JSON object of step name to millis as the agent timed them (create, boot, install, setup, job, destroy, ...).
    -- open ended so agents can add steps without a migration
    steps TEXT
);

CREATE INDEX IF NOT EXISTS idx_runner_runs_requested ON runner_runs(requested_at);
CREATE INDEX IF NOT EXISTS idx_runner_runs_agent ON runner_runs(agent_id, requested_at);

CREATE TABLE IF NOT EXISTS job_runs (
    job_id TEXT PRIMARY KEY NOT NULL,  -- "forge:id", like the other job tables
    forge TEXT NOT NULL,
    repository TEXT,
    workflow_name TEXT,
    job_name TEXT,
    labels TEXT NOT NULL,              -- JSON array
    queued_at BIGINT NOT NULL,         -- the forge's own created time when it gives one, else seen_at
    seen_at BIGINT NOT NULL,           -- first added to the pending queue
    first_runner_at BIGINT,
    started_at BIGINT,
    completed_at BIGINT,
    conclusion TEXT,                   -- the forge's verdict (success, failure, cancelled, ...)
    runner_name TEXT,                  -- whichever runner the forge says took it
    attempts INTEGER NOT NULL DEFAULT 0,
    outcome TEXT                       -- ran, abandoned, gone, cancelled
);

CREATE INDEX IF NOT EXISTS idx_job_runs_seen ON job_runs(seen_at);
