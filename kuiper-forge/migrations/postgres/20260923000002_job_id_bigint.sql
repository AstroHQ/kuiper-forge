-- github job ids are past 2^31 now, and postgres INTEGER is 32-bit so inserts fail with "integer out of range".
-- sqlite's INTEGER is already 64-bit, which is why this lives here and not in shared/
ALTER TABLE pending_webhook_jobs ALTER COLUMN job_id TYPE BIGINT;
ALTER TABLE active_runners ALTER COLUMN job_id TYPE BIGINT;
