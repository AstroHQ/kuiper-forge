-- Add job context columns to active_runners table
ALTER TABLE active_runners ADD COLUMN job_name TEXT;
ALTER TABLE active_runners ADD COLUMN repository TEXT;
ALTER TABLE active_runners ADD COLUMN workflow_name TEXT;
