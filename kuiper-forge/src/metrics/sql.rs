//! Queries for the run history tables, in the same sqlite/postgres pairs as `crate::sql`.

// runner_runs

#[cfg(feature = "sqlite")]
pub const INSERT_RUNNER_RUN: &str = r#"
    INSERT INTO runner_runs (runner_name, agent_id, hostname, agent_type, forge, job_id, repository, workflow_name,
                             job_name, pool, labels, requested_at)
    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
    ON CONFLICT(runner_name) DO NOTHING
"#;

#[cfg(feature = "postgres")]
pub const INSERT_RUNNER_RUN: &str = r#"
    INSERT INTO runner_runs (runner_name, agent_id, hostname, agent_type, forge, job_id, repository, workflow_name,
                             job_name, pool, labels, requested_at)
    VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)
    ON CONFLICT(runner_name) DO NOTHING
"#;

#[cfg(feature = "sqlite")]
pub const SET_RUNNER_ACKED: &str =
    "UPDATE runner_runs SET acked_at = ? WHERE runner_name = ? AND acked_at IS NULL";

#[cfg(feature = "postgres")]
pub const SET_RUNNER_ACKED: &str =
    "UPDATE runner_runs SET acked_at = $1 WHERE runner_name = $2 AND acked_at IS NULL";

#[cfg(feature = "sqlite")]
pub const SET_RUNNER_STARTED: &str =
    "UPDATE runner_runs SET started_at = ? WHERE runner_name = ? AND started_at IS NULL";

#[cfg(feature = "postgres")]
pub const SET_RUNNER_STARTED: &str =
    "UPDATE runner_runs SET started_at = $1 WHERE runner_name = $2 AND started_at IS NULL";

#[cfg(feature = "sqlite")]
pub const SET_RUNNER_JOB_STARTED: &str =
    "UPDATE runner_runs SET job_started_at = ? WHERE runner_name = ? AND job_started_at IS NULL";

#[cfg(feature = "postgres")]
pub const SET_RUNNER_JOB_STARTED: &str =
    "UPDATE runner_runs SET job_started_at = $1 WHERE runner_name = $2 AND job_started_at IS NULL";

#[cfg(feature = "sqlite")]
pub const FINISH_RUNNER_RUN: &str = r#"
    UPDATE runner_runs SET finished_at = ?, outcome = ?, error = ?
    WHERE runner_name = ? AND finished_at IS NULL
"#;

#[cfg(feature = "postgres")]
pub const FINISH_RUNNER_RUN: &str = r#"
    UPDATE runner_runs SET finished_at = $1, outcome = $2, error = $3
    WHERE runner_name = $4 AND finished_at IS NULL
"#;

// steps merge into what's there: STARTED brings the steps so far, the final event all of them
#[cfg(feature = "sqlite")]
pub const SET_RUNNER_TIMINGS: &str = r#"
    UPDATE runner_runs SET
        image = COALESCE(?, image),
        failed_phase = COALESCE(?, failed_phase),
        steps = json_patch(COALESCE(steps, '{}'), ?)
    WHERE runner_name = ?
"#;

#[cfg(feature = "postgres")]
pub const SET_RUNNER_TIMINGS: &str = r#"
    UPDATE runner_runs SET
        image = COALESCE($1, image),
        failed_phase = COALESCE($2, failed_phase),
        steps = (COALESCE(steps, '{}')::jsonb || $3::jsonb)::text
    WHERE runner_name = $4
"#;

// sqlx only takes static strings, so the column list is spelled out in each
#[cfg(feature = "sqlite")]
pub const SELECT_RUNNER_RUNS_SINCE: &str = r#"
    SELECT runner_name, agent_id, hostname, agent_type, forge, job_id, repository, workflow_name, job_name, pool,
           labels, requested_at, acked_at, started_at, job_started_at, finished_at, outcome, error, image,
           failed_phase, steps
    FROM runner_runs WHERE requested_at >= ? ORDER BY requested_at
"#;

#[cfg(feature = "postgres")]
pub const SELECT_RUNNER_RUNS_SINCE: &str = r#"
    SELECT runner_name, agent_id, hostname, agent_type, forge, job_id, repository, workflow_name, job_name, pool,
           labels, requested_at, acked_at, started_at, job_started_at, finished_at, outcome, error, image,
           failed_phase, steps
    FROM runner_runs WHERE requested_at >= $1 ORDER BY requested_at
"#;

#[cfg(feature = "sqlite")]
pub const DELETE_RUNNER_RUNS_BEFORE: &str = "DELETE FROM runner_runs WHERE requested_at < ?";

#[cfg(feature = "postgres")]
pub const DELETE_RUNNER_RUNS_BEFORE: &str = "DELETE FROM runner_runs WHERE requested_at < $1";

// job_runs

#[cfg(feature = "sqlite")]
pub const INSERT_JOB_RUN: &str = r#"
    INSERT INTO job_runs (job_id, forge, repository, workflow_name, job_name, labels, queued_at, seen_at)
    VALUES (?, ?, ?, ?, ?, ?, ?, ?)
    ON CONFLICT(job_id) DO NOTHING
"#;

#[cfg(feature = "postgres")]
pub const INSERT_JOB_RUN: &str = r#"
    INSERT INTO job_runs (job_id, forge, repository, workflow_name, job_name, labels, queued_at, seen_at)
    VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
    ON CONFLICT(job_id) DO NOTHING
"#;

#[cfg(feature = "sqlite")]
pub const JOB_RUNNER_CREATED: &str = r#"
    UPDATE job_runs SET attempts = attempts + 1, first_runner_at = COALESCE(first_runner_at, ?) WHERE job_id = ?
"#;

#[cfg(feature = "postgres")]
pub const JOB_RUNNER_CREATED: &str = r#"
    UPDATE job_runs SET attempts = attempts + 1, first_runner_at = COALESCE(first_runner_at, $1) WHERE job_id = $2
"#;

#[cfg(feature = "sqlite")]
pub const SET_JOB_STARTED: &str = r#"
    UPDATE job_runs SET started_at = COALESCE(started_at, ?), runner_name = COALESCE(runner_name, ?), outcome = 'ran'
    WHERE job_id = ?
"#;

#[cfg(feature = "postgres")]
pub const SET_JOB_STARTED: &str = r#"
    UPDATE job_runs SET started_at = COALESCE(started_at, $1), runner_name = COALESCE(runner_name, $2), outcome = 'ran'
    WHERE job_id = $3
"#;

#[cfg(feature = "sqlite")]
pub const SET_JOB_COMPLETED: &str = r#"
    UPDATE job_runs SET completed_at = COALESCE(completed_at, ?), conclusion = COALESCE(?, conclusion),
        outcome = COALESCE(outcome, ?)
    WHERE job_id = ?
"#;

#[cfg(feature = "postgres")]
pub const SET_JOB_COMPLETED: &str = r#"
    UPDATE job_runs SET completed_at = COALESCE(completed_at, $1), conclusion = COALESCE($2, conclusion),
        outcome = COALESCE(outcome, $3)
    WHERE job_id = $4
"#;

// a job can be dropped as gone (say the forge listed it running) and later finish on our runner, so `ran` wins over
// anything but a cancel the webhook already recorded, and the rest only fill in a blank
#[cfg(feature = "sqlite")]
pub const SET_JOB_OUTCOME: &str = r#"
    UPDATE job_runs SET outcome = CASE
        WHEN ? = 'ran' AND COALESCE(outcome, '') <> 'cancelled' THEN 'ran'
        ELSE COALESCE(outcome, ?)
    END
    WHERE job_id = ?
"#;

#[cfg(feature = "postgres")]
pub const SET_JOB_OUTCOME: &str = r#"
    UPDATE job_runs SET outcome = CASE
        WHEN $1::TEXT = 'ran' AND COALESCE(outcome, '') <> 'cancelled' THEN 'ran'
        ELSE COALESCE(outcome, $2)
    END
    WHERE job_id = $3
"#;

#[cfg(feature = "sqlite")]
pub const SELECT_JOB_RUNS_SINCE: &str = r#"
    SELECT job_id, forge, repository, workflow_name, job_name, labels, queued_at, seen_at, first_runner_at,
           started_at, completed_at, conclusion, runner_name, attempts, outcome
    FROM job_runs WHERE seen_at >= ? ORDER BY seen_at
"#;

#[cfg(feature = "postgres")]
pub const SELECT_JOB_RUNS_SINCE: &str = r#"
    SELECT job_id, forge, repository, workflow_name, job_name, labels, queued_at, seen_at, first_runner_at,
           started_at, completed_at, conclusion, runner_name, attempts, outcome
    FROM job_runs WHERE seen_at >= $1 ORDER BY seen_at
"#;

#[cfg(feature = "sqlite")]
pub const DELETE_JOB_RUNS_BEFORE: &str = "DELETE FROM job_runs WHERE seen_at < ?";

#[cfg(feature = "postgres")]
pub const DELETE_JOB_RUNS_BEFORE: &str = "DELETE FROM job_runs WHERE seen_at < $1";

// whole rows, for seeding mock history

#[cfg(feature = "sqlite")]
pub const INSERT_RUNNER_RUN_ROW: &str = r#"
    INSERT INTO runner_runs (runner_name, agent_id, hostname, agent_type, forge, job_id, repository, workflow_name,
                             job_name, pool, labels, requested_at, acked_at, started_at, job_started_at, finished_at,
                             outcome, error, image, failed_phase, steps)
    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
"#;

#[cfg(feature = "postgres")]
pub const INSERT_RUNNER_RUN_ROW: &str = r#"
    INSERT INTO runner_runs (runner_name, agent_id, hostname, agent_type, forge, job_id, repository, workflow_name,
                             job_name, pool, labels, requested_at, acked_at, started_at, job_started_at, finished_at,
                             outcome, error, image, failed_phase, steps)
    VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, $17, $18, $19, $20, $21)
"#;

#[cfg(feature = "sqlite")]
pub const INSERT_JOB_RUN_ROW: &str = r#"
    INSERT INTO job_runs (job_id, forge, repository, workflow_name, job_name, labels, queued_at, seen_at,
                          first_runner_at, started_at, completed_at, conclusion, runner_name, attempts, outcome)
    VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
"#;

#[cfg(feature = "postgres")]
pub const INSERT_JOB_RUN_ROW: &str = r#"
    INSERT INTO job_runs (job_id, forge, repository, workflow_name, job_name, labels, queued_at, seen_at,
                          first_runner_at, started_at, completed_at, conclusion, runner_name, attempts, outcome)
    VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15)
"#;

#[cfg(feature = "sqlite")]
pub const DELETE_RUNNER_RUNS_FOR_FORGE: &str = "DELETE FROM runner_runs WHERE forge = ?";

#[cfg(feature = "postgres")]
pub const DELETE_RUNNER_RUNS_FOR_FORGE: &str = "DELETE FROM runner_runs WHERE forge = $1";

#[cfg(feature = "sqlite")]
pub const DELETE_JOB_RUNS_FOR_FORGE: &str = "DELETE FROM job_runs WHERE forge = ?";

#[cfg(feature = "postgres")]
pub const DELETE_JOB_RUNS_FOR_FORGE: &str = "DELETE FROM job_runs WHERE forge = $1";
