-- Add repository column to pending webhook jobs
-- Stores the full repository name (owner/repo) for targeted GitHub job status checks
ALTER TABLE pending_webhook_jobs ADD COLUMN repository TEXT;
