-- where the agent got the runner from: hit, filled, vm or fell_back. NULL when the image already had it, and for
-- older agents
ALTER TABLE runner_runs ADD COLUMN install_source TEXT;
