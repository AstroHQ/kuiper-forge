-- jobs are keyed by "forge:id" text (like "github:123456") so one coordinator can serve several forges,
-- and runners record their forge. sqlite can't change a column's type, so the job tables are rebuilt.
-- existing github job ids become "github:<id>", the name a legacy [github] section gets.

CREATE TABLE pending_webhook_jobs_new (
    job_id TEXT PRIMARY KEY NOT NULL,
    job_labels TEXT NOT NULL,
    agent_labels TEXT NOT NULL,
    runner_scope TEXT NOT NULL,
    runner_group TEXT,
    created_at TEXT NOT NULL,
    retry_count INTEGER NOT NULL DEFAULT 0,
    repository TEXT,
    job_name TEXT,
    workflow_name TEXT,
    failed_agents TEXT
);
INSERT INTO pending_webhook_jobs_new
    SELECT 'github:' || CAST(job_id AS TEXT), job_labels, agent_labels, runner_scope, runner_group, created_at,
           retry_count, repository, job_name, workflow_name, failed_agents
    FROM pending_webhook_jobs;
DROP TABLE pending_webhook_jobs;
ALTER TABLE pending_webhook_jobs_new RENAME TO pending_webhook_jobs;
CREATE INDEX IF NOT EXISTS idx_pending_jobs_created_at ON pending_webhook_jobs(created_at);

CREATE TABLE active_runners_new (
    runner_name TEXT PRIMARY KEY NOT NULL,
    agent_id TEXT NOT NULL,
    vm_name TEXT NOT NULL,
    runner_scope TEXT NOT NULL,
    created_at TEXT NOT NULL,
    job_id TEXT,
    job_name TEXT,
    repository TEXT,
    workflow_name TEXT,
    pool TEXT,
    forge TEXT
);
INSERT INTO active_runners_new
    SELECT runner_name, agent_id, vm_name, runner_scope, created_at,
           CASE WHEN job_id IS NULL THEN NULL ELSE 'github:' || CAST(job_id AS TEXT) END,
           job_name, repository, workflow_name, pool, NULL
    FROM active_runners;
DROP TABLE active_runners;
ALTER TABLE active_runners_new RENAME TO active_runners;
CREATE INDEX IF NOT EXISTS idx_runners_agent_id ON active_runners(agent_id);
CREATE INDEX IF NOT EXISTS idx_runners_job_id ON active_runners(job_id);

CREATE TABLE agent_failures_new (
    id TEXT PRIMARY KEY NOT NULL,
    agent_id TEXT NOT NULL,
    occurred_at TEXT NOT NULL,
    kind TEXT NOT NULL,
    runner_name TEXT,
    job_id TEXT,
    message TEXT NOT NULL
);
INSERT INTO agent_failures_new
    SELECT id, agent_id, occurred_at, kind, runner_name,
           CASE WHEN job_id IS NULL THEN NULL ELSE 'github:' || CAST(job_id AS TEXT) END,
           message
    FROM agent_failures;
DROP TABLE agent_failures;
ALTER TABLE agent_failures_new RENAME TO agent_failures;
CREATE INDEX IF NOT EXISTS idx_agent_failures_agent ON agent_failures(agent_id, occurred_at);
