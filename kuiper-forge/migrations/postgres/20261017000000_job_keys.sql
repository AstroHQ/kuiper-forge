-- jobs are keyed by "forge:id" text (like "github:123456") so one coordinator can serve several forges,
-- and runners record their forge. existing github job ids become "github:<id>", the name a legacy [github]
-- section gets. sqlite does the same with a table rebuild in migrations/sqlite.
ALTER TABLE pending_webhook_jobs ALTER COLUMN job_id TYPE TEXT USING 'github:' || job_id::text;
ALTER TABLE active_runners ALTER COLUMN job_id TYPE TEXT USING CASE WHEN job_id IS NULL THEN NULL ELSE 'github:' || job_id::text END;
ALTER TABLE agent_failures ALTER COLUMN job_id TYPE TEXT USING CASE WHEN job_id IS NULL THEN NULL ELSE 'github:' || job_id::text END;
ALTER TABLE active_runners ADD COLUMN forge TEXT;
