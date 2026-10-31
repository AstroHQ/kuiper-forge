-- The recent errors page scans failures across all agents by time. Per backend, so it runs after the
-- sqlite job_keys rebuild of agent_failures
CREATE INDEX IF NOT EXISTS idx_agent_failures_occurred_at ON agent_failures(occurred_at);
