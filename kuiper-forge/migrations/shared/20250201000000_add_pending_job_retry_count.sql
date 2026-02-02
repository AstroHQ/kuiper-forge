-- Add retry_count to pending_webhook_jobs for tracking failed runner attempts
-- Jobs are retried when runners fail due to configuration errors (SSH keys, VM boot, etc.)
-- After MAX_RETRIES (default 3), the job is abandoned and removed
ALTER TABLE pending_webhook_jobs ADD COLUMN retry_count INTEGER NOT NULL DEFAULT 0;
