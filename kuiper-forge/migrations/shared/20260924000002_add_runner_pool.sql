-- fixed-capacity pool the runner was created for (its label set, sorted and comma-joined). NULL for webhook runners and
-- legacy pools
ALTER TABLE active_runners ADD COLUMN pool TEXT;
