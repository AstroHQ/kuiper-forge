-- Add workflow context columns to pending_webhook_jobs table
ALTER TABLE pending_webhook_jobs ADD COLUMN job_name TEXT;
ALTER TABLE pending_webhook_jobs ADD COLUMN workflow_name TEXT;
