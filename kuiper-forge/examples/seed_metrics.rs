//! Fills the coordinator database with mock runner and job history, for working on the metrics page.
//!
//!   mise run seed:metrics                  # 30 days into the default data dir's database
//!   mise run seed:metrics -- --days 7 -d /tmp/kf
//!   mise run seed:metrics -- --clear       # remove the mock rows again
//!
//! Everything goes under the `mock` forge, so a re-run or `--clear` only touches mock rows. The mock agents aren't
//! registered, so their links on the page 404.

use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::Utc;
use clap::Parser;
use kuiper_forge::config::{Config, DatabaseConfig};
use kuiper_forge::db::Database;
use kuiper_forge::metrics::{JobRun, MetricsStore, RunnerRun};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};

const FORGE: &str = "mock";
const HOUR_MS: i64 = 3_600_000;
/// Failed runners before a job is abandoned, like `pending_jobs::MAX_RETRIES`
const MAX_FAILURES: usize = 3;

#[derive(Parser)]
struct Args {
    /// Coordinator config, for its [database] section. Without one the default sqlite database is used
    #[arg(short, long, default_value_os_t = Config::default_path())]
    config: PathBuf,

    /// Coordinator data directory (where the sqlite database lives)
    #[arg(short, long, default_value_os_t = Config::default_data_dir())]
    data_dir: PathBuf,

    /// Days of history to make, ending now
    #[arg(long, default_value_t = 30)]
    days: i64,

    /// Average jobs per day
    #[arg(long, default_value_t = 120)]
    jobs_per_day: u32,

    /// Random seed, the same seed makes the same history (relative to now)
    #[arg(long, default_value_t = 1)]
    seed: u64,

    /// Only remove the mock rows
    #[arg(long)]
    clear: bool,
}

/// A mock agent and how it behaves.
struct MockAgent {
    hostname: &'static str,
    agent_type: &'static str,
    /// None for an agent too old to report step timings
    image: Option<&'static str>,
    labels: &'static [&'static str],
    /// Multiplies every step's duration
    slowness: f64,
    /// Chance an attempt fails or is lost
    failure_rate: f64,
    /// Chance the image already has the runner, so there's no download
    runner_cached: f64,
}

const MAC: &[&str] = &["self-hosted", "macos", "arm64"];
const LINUX: &[&str] = &["self-hosted", "linux", "x64"];
const WINDOWS: &[&str] = &["self-hosted", "windows", "x64"];

const AGENTS: &[MockAgent] = &[
    MockAgent {
        hostname: "mac-mini-1",
        agent_type: "tart",
        image: Some("macos-15-xcode-26"),
        labels: MAC,
        slowness: 1.0,
        failure_rate: 0.02,
        runner_cached: 1.0,
    },
    MockAgent {
        hostname: "mac-mini-2",
        agent_type: "tart",
        image: Some("macos-15-xcode-26"),
        labels: MAC,
        slowness: 1.3,
        failure_rate: 0.03,
        runner_cached: 0.85,
    },
    // the flaky one, mostly boot failures, and its image has no runner so every run downloads it
    MockAgent {
        hostname: "mac-studio",
        agent_type: "tart",
        image: Some("macos-14-xcode-16"),
        labels: MAC,
        slowness: 0.8,
        failure_rate: 0.12,
        runner_cached: 0.0,
    },
    MockAgent {
        hostname: "old-tart",
        agent_type: "tart",
        image: None,
        labels: MAC,
        slowness: 1.1,
        failure_rate: 0.03,
        runner_cached: 1.0,
    },
    MockAgent {
        hostname: "pve-1",
        agent_type: "proxmox",
        image: Some("ubuntu-24.04"),
        labels: LINUX,
        slowness: 0.5,
        failure_rate: 0.04,
        runner_cached: 0.95,
    },
    MockAgent {
        hostname: "pve-2",
        agent_type: "proxmox",
        image: Some("windows-2022"),
        labels: WINDOWS,
        slowness: 1.6,
        failure_rate: 0.06,
        runner_cached: 0.0,
    },
];

const REPOS: &[(&str, &[&str])] = &[
    ("astro/app", &["build", "test", "lint"]),
    ("astro/engine", &["build", "unit-tests", "integration"]),
    ("astro/website", &["deploy"]),
];

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let database = if args.config.exists() {
        Config::load(&args.config)?.database
    } else {
        DatabaseConfig::default()
    };
    let db = Database::new(&database, &args.data_dir)
        .await
        .context("Failed to open the coordinator database")?;
    let store = MetricsStore::new(db.pool());

    let removed = store.delete_forge(FORGE).await?;
    if removed > 0 {
        println!("Removed {removed} old mock rows");
    }
    if args.clear {
        return Ok(());
    }

    let mut rng = StdRng::seed_from_u64(args.seed);
    let now = Utc::now().timestamp_millis();
    let start = now - args.days * 24 * HOUR_MS;
    let (mut runs, mut jobs) = (0, 0);
    let mut next_id = 0u64;

    for hour in 0..args.days * 24 {
        let hour_start = start + hour * HOUR_MS;
        let count = poisson(
            &mut rng,
            args.jobs_per_day as f64 / 24.0 * busyness(hour_start),
        );
        for _ in 0..count {
            next_id += 1;
            let queued = hour_start + rng.random_range(0..HOUR_MS);
            if queued > now {
                continue;
            }
            let (job, job_runs) = mock_job(&mut rng, next_id, queued, now);
            for run in &job_runs {
                store.insert_run(run).await?;
            }
            store.insert_job(&job).await?;
            runs += job_runs.len();
            jobs += 1;
        }
    }

    println!(
        "Seeded {jobs} jobs and {runs} runner attempts over {} days into {}",
        args.days,
        args.data_dir.display()
    );
    println!("Open /admin/metrics, `--clear` removes them");
    Ok(())
}

/// Relative job volume: busy on weekday afternoons (UTC), quiet at night and on weekends.
fn busyness(at: i64) -> f64 {
    let hours = at / HOUR_MS;
    let hour = hours % 24;
    // 1970-01-01 was a Thursday
    let weekday = (hours / 24 + 3) % 7;
    let day = if weekday >= 5 { 0.25 } else { 1.0 };
    let time = match hour {
        14..=22 => 2.0,
        11..=13 | 23 => 1.0,
        _ => 0.2,
    };
    day * time
}

/// One job with its runner attempts.
fn mock_job(rng: &mut StdRng, id: u64, queued: i64, now: i64) -> (JobRun, Vec<RunnerRun>) {
    let (repository, names) = REPOS[rng.random_range(0..REPOS.len())];
    let job_name = names[rng.random_range(0..names.len())];
    let job_id = format!("{FORGE}:{id}");
    // webhook delivery
    let seen = queued + rng.random_range(300..3_000);
    let mut job = JobRun {
        job_id: job_id.clone(),
        forge: FORGE.to_string(),
        repository: Some(repository.to_string()),
        workflow_name: Some("CI".to_string()),
        job_name: Some(job_name.to_string()),
        queued_at: queued,
        seen_at: seen,
        ..Default::default()
    };

    // cancelled while still queued, never gets a runner
    if rng.random_bool(0.02) {
        job.completed_at = Some(seen + rng.random_range(5_000..120_000));
        job.conclusion = Some("cancelled".into());
        job.outcome = Some("cancelled".into());
        return (job, Vec::new());
    }

    // waiting for capacity, now and then a long backlog
    let backlog = if rng.random_bool(0.05) {
        300_000.0
    } else {
        8_000.0
    };
    let mut at = seen + exp(rng, backlog) as i64;
    let mut runs = Vec::new();
    let mut tried = Vec::new();
    let mut failures = 0;
    while failures < MAX_FAILURES && at <= now {
        let agent = pick_agent(rng, &tried);
        tried.push(agent.hostname);
        // every runner counts as an attempt, only failures use up retries (rejections don't)
        job.attempts += 1;
        job.first_runner_at.get_or_insert(at);
        let run = mock_run(rng, agent, &job, at, now);
        let done = run.outcome.as_deref() == Some("completed") || run.outcome.is_none();
        if matches!(run.outcome.as_deref(), Some("failed" | "lost")) {
            failures += 1;
        }
        at = run.finished_at.unwrap_or(now) + rng.random_range(500..5_000);
        let ran = run
            .job_started_at
            .map(|t| (t, run.runner_name.clone(), run.finished_at));
        runs.push(run);
        if let Some((started, runner, finished)) = ran {
            job.started_at = Some(started);
            job.runner_name = Some(runner);
            job.outcome = Some("ran".into());
            if let Some(finished) = finished {
                job.completed_at = Some(finished);
                let conclusion = match rng.random_range(0..100) {
                    0..90 => "success",
                    90..98 => "failure",
                    _ => "cancelled",
                };
                job.conclusion = Some(conclusion.into());
                if conclusion == "cancelled" {
                    job.outcome = Some("cancelled".into());
                }
            }
        }
        if done {
            break;
        }
    }
    if job.outcome.is_none() && failures >= MAX_FAILURES {
        job.outcome = Some("abandoned".into());
    }
    (job, runs)
}

fn pick_agent(rng: &mut StdRng, tried: &[&str]) -> &'static MockAgent {
    let fresh: Vec<&MockAgent> = AGENTS
        .iter()
        .filter(|a| !tried.contains(&a.hostname))
        .collect();
    let pool = if fresh.is_empty() {
        AGENTS.iter().collect()
    } else {
        fresh
    };
    pool[rng.random_range(0..pool.len())]
}

/// One runner attempt on `agent`, requested at `requested`. Anything that would end after `now` is still running.
fn mock_run(
    rng: &mut StdRng,
    agent: &MockAgent,
    job: &JobRun,
    requested: i64,
    now: i64,
) -> RunnerRun {
    let mut run = RunnerRun {
        runner_name: format!("mock-{}-{:08x}", agent.hostname, rng.random::<u32>()),
        agent_id: format!("mock-{}", agent.hostname),
        hostname: agent.hostname.to_string(),
        agent_type: agent.agent_type.to_string(),
        forge: Some(FORGE.to_string()),
        job_id: Some(job.job_id.clone()),
        repository: job.repository.clone(),
        workflow_name: job.workflow_name.clone(),
        job_name: job.job_name.clone(),
        labels: agent.labels.iter().map(|l| l.to_string()).collect(),
        requested_at: requested,
        image: agent.image.map(String::from),
        ..Default::default()
    };

    if rng.random_bool(0.03) {
        run.outcome = Some("rejected".into());
        run.error = Some("no free slot for this image".into());
        run.finished_at = Some(requested + rng.random_range(50..300));
        return run;
    }
    run.acked_at = Some(requested + rng.random_range(80..450));

    let step = |rng: &mut StdRng, mean: f64| {
        (normal(rng, mean, mean * 0.18) * agent.slowness).max(200.0) as i64
    };
    let create = step(rng, 8_000.0);
    let boot = step(rng, 35_000.0);
    let setup = step(rng, 9_000.0);
    let cached = rng.random_bool(agent.runner_cached);
    let install = if cached { 0 } else { step(rng, 25_000.0) };
    let job_ms = exp(rng, 420_000.0).max(15_000.0) as i64;
    let destroy = normal(rng, 4_000.0, 700.0).max(500.0) as i64;

    let failed = rng.random_bool(agent.failure_rate);
    let failed_phase = failed.then(|| match rng.random_range(0..10) {
        0..4 => "boot",
        // a download can only fail when there is one
        4..6 if !cached => "install",
        4..8 => "setup",
        _ => "job",
    });
    // old agents don't report steps
    let reported = agent.image.is_some();
    let mut record = |step: &str, ms: i64| {
        if reported {
            run.steps.insert(step.to_string(), ms);
        }
    };

    // walks the steps in order until the failed one, which gets half its time and no record
    let mut t = requested;
    let mut started = None;
    let mut job_started = None;
    let plan = [
        ("create", create),
        ("boot", boot),
        ("install", install),
        ("setup", setup),
        ("job", job_ms),
    ];
    for (step, ms) in plan {
        if step == "job" {
            // the runner is up whether or not the job then goes wrong
            started = Some(t);
        }
        if failed_phase == Some(step) {
            t += ms / 2;
            break;
        }
        if step == "job" {
            let pickup = rng.random_range(800..4_000);
            job_started = Some(t + pickup);
            t += pickup;
        }
        t += ms;
        record(step, ms);
    }
    t += destroy;
    record("destroy", destroy);
    run.started_at = started;
    run.job_started_at = job_started;

    // still going, the history only has what happened so far
    if t > now {
        run.started_at = run.started_at.filter(|s| *s <= now);
        run.job_started_at = run.job_started_at.filter(|s| *s <= now);
        run.steps.remove("job");
        run.steps.remove("destroy");
        return run;
    }
    run.finished_at = Some(t);
    match failed_phase {
        None => run.outcome = Some("completed".into()),
        Some(phase) => {
            // a dead VM mid-job mostly shows up as lost
            run.outcome = Some(
                if phase == "job" && rng.random_bool(0.5) {
                    "lost"
                } else {
                    "failed"
                }
                .into(),
            );
            run.failed_phase = reported.then(|| phase.to_string());
            run.error = Some(
                match phase {
                    "boot" => "VM didn't answer on SSH in time",
                    "install" => "runner download failed: curl exited with 56",
                    "setup" => "runner config.sh exited with 1",
                    _ => "runner exited with signal 9",
                }
                .into(),
            );
        }
    }
    run
}

fn normal(rng: &mut StdRng, mean: f64, sd: f64) -> f64 {
    // Box-Muller, rand_distr isn't a dependency
    let (u1, u2): (f64, f64) = (rng.random::<f64>().max(1e-12), rng.random());
    mean + sd * (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
}

fn exp(rng: &mut StdRng, mean: f64) -> f64 {
    -mean * rng.random::<f64>().max(1e-12).ln()
}

fn poisson(rng: &mut StdRng, mean: f64) -> u32 {
    // Knuth's, fine for the small means here
    let limit = (-mean).exp();
    let mut p = rng.random::<f64>();
    let mut n = 0;
    while p > limit {
        p *= rng.random::<f64>();
        n += 1;
    }
    n
}
