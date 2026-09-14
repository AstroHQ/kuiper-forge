-- agents that already failed this job, as a json array. lets the scheduler try someone else on retry
-- instead of bouncing the job back to the same host that just broke it.
ALTER TABLE pending_webhook_jobs ADD COLUMN failed_agents TEXT;
