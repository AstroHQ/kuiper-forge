-- agents' own log lines, uploaded in batches. pruned by age and per-agent line count on a timer
CREATE TABLE IF NOT EXISTS agent_logs (
    agent_id TEXT NOT NULL,
    ts BIGINT NOT NULL,       -- unix micros from the agent's clock
    seq BIGINT NOT NULL,      -- per agent process, breaks ties on equal ts
    level SMALLINT NOT NULL,  -- 1 error .. 5 trace, so "warn and up" is level <= 2
    target TEXT NOT NULL,
    message TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_agent_logs_agent_ts ON agent_logs(agent_id, ts, seq);
CREATE INDEX IF NOT EXISTS idx_agent_logs_ts ON agent_logs(ts);
