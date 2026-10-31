//! Turns the run history into the numbers the metrics page shows. Plain functions over the fetched rows: sqlite has no
//! percentile function, and the row counts are small enough to sort in memory.

use std::collections::{BTreeMap, HashMap};

use chrono::{DateTime, Utc};
use serde::Serialize;

use super::{JobRun, RunnerRun};

const HOUR_MS: i64 = 3_600_000;
const DAY_MS: i64 = 24 * HOUR_MS;

/// Agents drawn in the per-agent charts, the busiest first. More lines than this can't be told apart.
const MAX_CHART_AGENTS: usize = 8;

/// A time window for the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Range {
    Day,
    Week,
    Month,
    Quarter,
}

impl Range {
    pub const ALL: [Range; 4] = [Range::Day, Range::Week, Range::Month, Range::Quarter];

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|r| r.as_str() == s)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Range::Day => "24h",
            Range::Week => "7d",
            Range::Month => "30d",
            Range::Quarter => "90d",
        }
    }

    pub fn duration_ms(self) -> i64 {
        match self {
            Range::Day => DAY_MS,
            Range::Week => 7 * DAY_MS,
            Range::Month => 30 * DAY_MS,
            Range::Quarter => 90 * DAY_MS,
        }
    }

    /// Hourly buckets for the last day, daily past that.
    pub fn bucket_ms(self) -> i64 {
        match self {
            Range::Day => HOUR_MS,
            _ => DAY_MS,
        }
    }
}

/// Count and percentiles of one duration, in millis.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Stat {
    pub count: usize,
    pub p50: Option<i64>,
    pub p95: Option<i64>,
}

impl Stat {
    fn of(mut values: Vec<i64>) -> Self {
        values.sort_unstable();
        Self {
            count: values.len(),
            p50: percentile(&values, 0.50),
            p95: percentile(&values, 0.95),
        }
    }
}

/// Nearest-rank percentile of sorted values.
fn percentile(sorted: &[i64], p: f64) -> Option<i64> {
    if sorted.is_empty() {
        return None;
    }
    let rank = (p * sorted.len() as f64).ceil() as usize;
    Some(sorted[rank.clamp(1, sorted.len()) - 1])
}

/// Durations between two optional times. Negative spans (clocks, or events out of order) are dropped.
fn span(from: Option<i64>, to: Option<i64>) -> Option<i64> {
    let d = to? - from?;
    (d >= 0).then_some(d)
}

/// Runner outcomes that say something about the agent. `rejected` is the agent saying it's full, `idled`, `drained`
/// and `destroyed` are our own decisions, so none of those count for or against it.
fn is_failure(outcome: &str) -> bool {
    matches!(outcome, "failed" | "lost" | "command_failed")
}

/// Agent steps in the order they happen, for table columns and stacked bars. Steps newer agents add go after these.
const STEP_ORDER: &[&str] = &["create", "boot", "install", "setup", "job", "destroy"];

/// Steps that happen after the runner is up, left out of the time-to-runner breakdown.
const AFTER_START: &[&str] = &["job", "destroy"];

/// Every known step, then any other step a run reported, alphabetically.
pub fn step_names(runs: &[RunnerRun]) -> Vec<String> {
    let mut names: Vec<String> = STEP_ORDER.iter().map(|s| s.to_string()).collect();
    let mut extra: Vec<&String> = runs
        .iter()
        .flat_map(|r| r.steps.keys())
        .filter(|k| !STEP_ORDER.contains(&k.as_str()))
        .collect();
    extra.sort();
    extra.dedup();
    names.extend(extra.into_iter().cloned());
    names
}

/// One agent-reported step across a group's runs.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct StepStat {
    /// Runs that did the step
    pub ran: Stat,
    /// Runs that reported it skipped (0), say the runner already in the image
    pub skipped: usize,
    /// Every run that reported the step, skipped ones as 0: what it usually costs
    pub typical: Stat,
}

impl StepStat {
    /// Share of the runs reporting this step that skipped it, None if none did.
    pub fn skipped_pct(&self) -> Option<f64> {
        (self.skipped > 0).then(|| 100.0 * self.skipped as f64 / self.typical.count as f64)
    }
}

/// Stats for one group of runner attempts (an agent, an image).
#[derive(Debug, Clone, Default)]
pub struct RunGroup {
    pub key: String,
    /// What to show for the key: the hostname for an agent
    pub label: String,
    pub detail: String,
    pub runs: usize,
    pub running: usize,
    pub completed: usize,
    pub failures: usize,
    pub rejected: usize,
    /// Idled, drained or destroyed: our own teardowns
    pub stopped: usize,
    /// completed / (completed + failures), None before anything finished
    pub success_pct: Option<f64>,
    pub ack: Stat,
    /// CreateRunner sent until the agent says the runner is up, by our clock
    pub provision: Stat,
    /// The agent's step timings by step name
    pub steps: BTreeMap<String, StepStat>,
    /// Failure counts by the step the agent was on, most common first
    pub failed_phases: Vec<(String, usize)>,
}

impl RunGroup {
    pub fn step(&self, name: &str) -> StepStat {
        self.steps.get(name).copied().unwrap_or_default()
    }
}

fn group_runs<'a>(
    runs: impl IntoIterator<Item = &'a RunnerRun>,
    key: impl Fn(&RunnerRun) -> (String, String, String),
) -> Vec<RunGroup> {
    let mut groups: HashMap<String, Vec<&RunnerRun>> = HashMap::new();
    let mut labels: HashMap<String, (String, String)> = HashMap::new();
    for run in runs {
        let (k, label, detail) = key(run);
        // runs are oldest first, so this keeps the newest hostname/agent type
        labels.insert(k.clone(), (label, detail));
        groups.entry(k).or_default().push(run);
    }
    let mut out: Vec<RunGroup> = groups
        .into_iter()
        .map(|(k, runs)| {
            let (label, detail) = labels.remove(&k).unwrap_or_default();
            summarize(k, label, detail, &runs)
        })
        .collect();
    out.sort_by(|a, b| b.runs.cmp(&a.runs).then_with(|| a.label.cmp(&b.label)));
    out
}

fn summarize(key: String, label: String, detail: String, runs: &[&RunnerRun]) -> RunGroup {
    let mut g = RunGroup {
        key,
        label,
        detail,
        runs: runs.len(),
        ..Default::default()
    };
    let mut phases: HashMap<String, usize> = HashMap::new();
    for run in runs {
        match run.outcome.as_deref() {
            None => g.running += 1,
            Some("completed") => g.completed += 1,
            Some("rejected") => g.rejected += 1,
            Some("idled" | "drained" | "destroyed") => g.stopped += 1,
            Some(o) if is_failure(o) => {
                g.failures += 1;
                let phase = run.failed_phase.clone().unwrap_or_else(|| "unknown".into());
                *phases.entry(phase).or_default() += 1;
            }
            Some(_) => {}
        }
    }
    let decided = g.completed + g.failures;
    g.success_pct = (decided > 0).then(|| 100.0 * g.completed as f64 / decided as f64);

    let collect = |f: &dyn Fn(&RunnerRun) -> Option<i64>| {
        Stat::of(runs.iter().filter_map(|r| f(r)).collect())
    };
    g.ack = collect(&|r| span(Some(r.requested_at), r.acked_at));
    g.provision = collect(&|r| span(Some(r.requested_at), r.started_at));

    let mut steps: BTreeMap<&str, Vec<i64>> = BTreeMap::new();
    for run in runs {
        for (step, ms) in &run.steps {
            steps.entry(step).or_default().push(*ms);
        }
    }
    g.steps = steps
        .into_iter()
        .map(|(step, values)| {
            let ran: Vec<i64> = values.iter().copied().filter(|v| *v > 0).collect();
            let stat = StepStat {
                skipped: values.len() - ran.len(),
                ran: Stat::of(ran),
                typical: Stat::of(values),
            };
            (step.to_string(), stat)
        })
        .collect();

    let mut phases: Vec<_> = phases.into_iter().collect();
    phases.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    g.failed_phases = phases;
    g
}

/// Runner stats for every attempt together.
pub fn total(runs: &[RunnerRun]) -> RunGroup {
    let runs: Vec<&RunnerRun> = runs.iter().collect();
    summarize(String::new(), String::new(), String::new(), &runs)
}

/// Runner stats per agent, busiest first.
pub fn by_agent<'a>(runs: impl IntoIterator<Item = &'a RunnerRun>) -> Vec<RunGroup> {
    group_runs(runs, |r| {
        (r.agent_id.clone(), r.hostname.clone(), r.agent_type.clone())
    })
}

/// The image a run is grouped under. Older agents don't report one, so those fall back to the runner's labels.
fn image_key(run: &RunnerRun) -> String {
    match &run.image {
        Some(image) => image.clone(),
        None => format!("labels: {}", run.labels.join(", ")),
    }
}

/// Runner stats per image.
pub fn by_image<'a>(runs: impl IntoIterator<Item = &'a RunnerRun>) -> Vec<RunGroup> {
    group_runs(runs, |r| {
        let key = image_key(r);
        (key.clone(), key, r.agent_type.clone())
    })
}

/// The runner was already in the image, agents report that as a skipped install
pub const SOURCE_IMAGE: &str = "image";
/// It took time, but the run doesn't say where the runner came from: before agents reported it, or an older agent.
/// With the cache working these are what's left of the VM download times to compare against
pub const SOURCE_NOT_RECORDED: &str = "not_recorded";

/// Install sources in the order they're shown. Any other goes last.
const SOURCE_ORDER: &[&str] = &[
    "hit",
    "filled",
    "fell_back",
    "vm",
    SOURCE_NOT_RECORDED,
    SOURCE_IMAGE,
];

/// Where a run's runner came from, None if it stopped before finding out.
fn install_source(run: &RunnerRun) -> Option<&str> {
    match (run.install_source.as_deref(), run.steps.get("install")) {
        (Some(source), _) => Some(source),
        (None, Some(0)) => Some(SOURCE_IMAGE),
        (None, Some(_)) => Some(SOURCE_NOT_RECORDED),
        (None, None) => None,
    }
}

/// One image's runner installs from one source.
#[derive(Debug, Clone, PartialEq)]
pub struct InstallGroup {
    pub image: String,
    pub source: String,
    pub runs: usize,
    /// How long the install took. Failed installs count in `runs` but not here
    pub install: Stat,
}

/// Runner installs per image and source: busiest image first, then in [`SOURCE_ORDER`].
pub fn by_install<'a>(runs: impl IntoIterator<Item = &'a RunnerRun>) -> Vec<InstallGroup> {
    let mut groups: HashMap<(String, String), (usize, Vec<i64>)> = HashMap::new();
    let mut per_image: HashMap<String, usize> = HashMap::new();
    for run in runs {
        let Some(source) = install_source(run) else {
            continue;
        };
        let image = image_key(run);
        *per_image.entry(image.clone()).or_default() += 1;
        let group = groups.entry((image, source.to_string())).or_default();
        group.0 += 1;
        group
            .1
            .extend(run.steps.get("install").copied().filter(|ms| *ms > 0));
    }

    let order = |source: &str| {
        SOURCE_ORDER
            .iter()
            .position(|s| *s == source)
            .unwrap_or(SOURCE_ORDER.len())
    };
    let mut out: Vec<InstallGroup> = groups
        .into_iter()
        .map(|((image, source), (runs, install))| InstallGroup {
            image,
            source,
            runs,
            install: Stat::of(install),
        })
        .collect();
    out.sort_by(|a, b| {
        per_image[&b.image]
            .cmp(&per_image[&a.image])
            .then_with(|| a.image.cmp(&b.image))
            .then_with(|| order(&a.source).cmp(&order(&b.source)))
            .then_with(|| a.source.cmp(&b.source))
    });
    out
}

/// Where a job's time in the queue went.
#[derive(Debug, Clone, Default)]
pub struct JobSummary {
    pub jobs: usize,
    pub pending: usize,
    pub ran: usize,
    pub abandoned: usize,
    pub gone: usize,
    pub cancelled: usize,
    pub mean_attempts: Option<f64>,
    /// Queued on the forge until a runner took it. Needs the forge's start time (GitHub webhooks)
    pub wait: Stat,
    /// Queued on the forge until we heard about it: webhook delivery, or the next poll
    pub to_coordinator: Stat,
    /// We heard about it until the runner that ran it was requested: waiting for capacity, and earlier failed
    /// attempts
    pub dispatch: Stat,
    /// That runner's request until it was up
    pub provision: Stat,
    /// The runner was up until it took the job
    pub pickup: Stat,
}

pub fn jobs(jobs: &[JobRun], runs: &[RunnerRun]) -> JobSummary {
    // the runner that ran each job is the one the forge named, else the last one that completed it (polling
    // mode never hears which runner took a job)
    let by_name: HashMap<&str, &RunnerRun> =
        runs.iter().map(|r| (r.runner_name.as_str(), r)).collect();
    let mut completed_by_job: HashMap<&str, &RunnerRun> = HashMap::new();
    for run in runs {
        if let Some(job) = run.job_id.as_deref()
            && run.outcome.as_deref() == Some("completed")
        {
            completed_by_job.insert(job, run);
        }
    }

    let mut s = JobSummary {
        jobs: jobs.len(),
        ..Default::default()
    };
    let (mut wait, mut to_coord, mut dispatch, mut provision, mut pickup) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let mut attempts = 0;
    for job in jobs {
        match job.outcome.as_deref() {
            None => s.pending += 1,
            Some("ran") => s.ran += 1,
            Some("abandoned") => s.abandoned += 1,
            Some("gone") => s.gone += 1,
            Some("cancelled") => s.cancelled += 1,
            Some(_) => {}
        }
        attempts += job.attempts;

        wait.extend(span(Some(job.queued_at), job.started_at));
        // equal means the forge gave no created time, not that it took no time
        if job.queued_at < job.seen_at {
            to_coord.push(job.seen_at - job.queued_at);
        }
        let run = job
            .runner_name
            .as_deref()
            .and_then(|n| by_name.get(n).copied())
            .or_else(|| completed_by_job.get(job.job_id.as_str()).copied());
        if let Some(run) = run {
            dispatch.extend(span(Some(job.seen_at), Some(run.requested_at)));
            provision.extend(span(Some(run.requested_at), run.started_at));
            pickup.extend(span(run.started_at, job.started_at));
        }
    }
    s.mean_attempts = (!jobs.is_empty()).then(|| attempts as f64 / jobs.len() as f64);
    s.wait = Stat::of(wait);
    s.to_coordinator = Stat::of(to_coord);
    s.dispatch = Stat::of(dispatch);
    s.provision = Stat::of(provision);
    s.pickup = Stat::of(pickup);
    s
}

/// Series for the charts. Durations are seconds, gaps are null.
#[derive(Debug, Serialize, Default)]
pub struct Charts {
    /// Bucket labels, UTC
    pub buckets: Vec<String>,
    /// Runner attempts per bucket by how they ended
    pub outcomes: Outcomes,
    /// p95 time to a running runner per bucket, one line per agent
    pub provision: Vec<Series>,
    /// p50 and p95 queue wait per bucket
    pub wait_p50: Vec<Option<f64>>,
    pub wait_p95: Vec<Option<f64>>,
    /// p50 per step for each agent, for a stacked bar
    pub phases: Phases,
}

#[derive(Debug, Serialize, Default)]
pub struct Outcomes {
    pub completed: Vec<usize>,
    pub failed: Vec<usize>,
    pub rejected: Vec<usize>,
    pub other: Vec<usize>,
}

#[derive(Debug, Serialize)]
pub struct Series {
    pub label: String,
    pub data: Vec<Option<f64>>,
}

#[derive(Debug, Serialize, Default)]
pub struct Phases {
    pub agents: Vec<String>,
    /// One series per step before the runner is up, in step order. Typical (p50 counting skips as 0) per agent
    pub steps: Vec<Series>,
}

fn secs(ms: Option<i64>) -> Option<f64> {
    ms.map(|ms| (ms as f64 / 100.0).round() / 10.0)
}

/// Bucket starts covering `since` to `now`, aligned to the bucket size (UTC).
fn bucket_starts(since: i64, now: i64, size: i64) -> Vec<i64> {
    let first = since.div_euclid(size) * size;
    (0..)
        .map(|i| first + i * size)
        .take_while(|start| *start <= now)
        .collect()
}

fn bucket_label(start: i64, size: i64) -> String {
    let Some(t) = DateTime::<Utc>::from_timestamp_millis(start) else {
        return String::new();
    };
    if size < DAY_MS {
        t.format("%b %d %H:00").to_string()
    } else {
        t.format("%b %d").to_string()
    }
}

pub fn charts(
    runs: &[RunnerRun],
    jobs: &[JobRun],
    agents: &[RunGroup],
    steps: &[String],
    range: Range,
    now: i64,
) -> Charts {
    let size = range.bucket_ms();
    let starts = bucket_starts(now - range.duration_ms(), now, size);
    let first = starts.first().copied().unwrap_or(now);
    let index = |t: i64| -> Option<usize> {
        let i = (t - first).div_euclid(size);
        (i >= 0 && (i as usize) < starts.len()).then_some(i as usize)
    };
    let n = starts.len();

    let mut outcomes = Outcomes {
        completed: vec![0; n],
        failed: vec![0; n],
        rejected: vec![0; n],
        other: vec![0; n],
    };
    let chart_agents: Vec<&RunGroup> = agents.iter().take(MAX_CHART_AGENTS).collect();
    let mut provision: BTreeMap<&str, Vec<Vec<i64>>> = chart_agents
        .iter()
        .map(|g| (g.key.as_str(), vec![Vec::new(); n]))
        .collect();
    for run in runs {
        let Some(i) = index(run.requested_at) else {
            continue;
        };
        match run.outcome.as_deref() {
            Some("completed") => outcomes.completed[i] += 1,
            Some("rejected") => outcomes.rejected[i] += 1,
            Some(o) if is_failure(o) => outcomes.failed[i] += 1,
            Some(_) => outcomes.other[i] += 1,
            None => {}
        }
        if let (Some(buckets), Some(d)) = (
            provision.get_mut(run.agent_id.as_str()),
            span(Some(run.requested_at), run.started_at),
        ) {
            buckets[i].push(d);
        }
    }

    let mut waits = vec![Vec::new(); n];
    for job in jobs {
        if let (Some(i), Some(d)) = (
            index(job.queued_at),
            span(Some(job.queued_at), job.started_at),
        ) {
            waits[i].push(d);
        }
    }
    let wait_stats: Vec<Stat> = waits.into_iter().map(Stat::of).collect();

    Charts {
        buckets: starts.iter().map(|s| bucket_label(*s, size)).collect(),
        outcomes,
        provision: chart_agents
            .iter()
            .map(|g| Series {
                label: g.label.clone(),
                data: provision
                    .remove(g.key.as_str())
                    .unwrap_or_default()
                    .into_iter()
                    .map(|v| secs(Stat::of(v).p95))
                    .collect(),
            })
            .collect(),
        wait_p50: wait_stats.iter().map(|s| secs(s.p50)).collect(),
        wait_p95: wait_stats.iter().map(|s| secs(s.p95)).collect(),
        phases: Phases {
            agents: chart_agents.iter().map(|g| g.label.clone()).collect(),
            steps: steps
                .iter()
                .filter(|s| !AFTER_START.contains(&s.as_str()))
                .map(|step| Series {
                    label: step.clone(),
                    data: chart_agents
                        .iter()
                        .map(|g| secs(g.step(step).typical.p50))
                        .collect(),
                })
                .collect(),
        },
    }
}

/// Short duration for tables, e.g. `850ms`, `42s`, `3m 12s`, `2h 5m`.
pub fn fmt_ms(ms: Option<i64>) -> String {
    let Some(ms) = ms else {
        return "-".to_string();
    };
    if ms < 1_000 {
        return format!("{ms}ms");
    }
    let s = ms / 1_000;
    if s < 60 {
        format!("{:.1}s", ms as f64 / 1_000.0)
    } else if s < 3_600 {
        format!("{}m {}s", s / 60, s % 60)
    } else {
        format!("{}h {}m", s / 3_600, (s % 3_600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(agent: &str, requested: i64, outcome: Option<&str>) -> RunnerRun {
        RunnerRun {
            runner_name: format!("{agent}-{requested}"),
            agent_id: agent.into(),
            hostname: format!("{agent}.local"),
            agent_type: "tart".into(),
            requested_at: requested,
            outcome: outcome.map(String::from),
            ..Default::default()
        }
    }

    #[test]
    fn test_percentile() {
        let v: Vec<i64> = (1..=100).collect();
        assert_eq!(percentile(&v, 0.5), Some(50));
        assert_eq!(percentile(&v, 0.95), Some(95));
        assert_eq!(percentile(&[7], 0.95), Some(7));
        assert_eq!(percentile(&[], 0.5), None);
    }

    #[test]
    fn test_success_ignores_rejections_and_our_own_teardowns() {
        let runs = vec![
            run("a", 0, Some("completed")),
            run("a", 1, Some("completed")),
            run("a", 2, Some("completed")),
            run("a", 3, Some("lost")),
            run("a", 4, Some("rejected")),
            run("a", 5, Some("idled")),
            run("a", 6, None),
        ];
        let g = &by_agent(&runs)[0];
        assert_eq!(g.runs, 7);
        assert_eq!(
            (g.completed, g.failures, g.rejected, g.stopped, g.running),
            (3, 1, 1, 1, 1)
        );
        assert_eq!(g.success_pct, Some(75.0));
        assert_eq!(g.failed_phases, vec![("unknown".to_string(), 1)]);
    }

    #[test]
    fn test_missing_steps_are_skipped_per_step() {
        let mut old = run("a", 0, Some("completed"));
        old.started_at = Some(30_000);
        let mut new = run("a", 1_000, Some("completed"));
        new.started_at = Some(21_000);
        new.steps.insert("boot".into(), 15_000);
        let g = &by_agent(&[old, new])[0];
        assert_eq!(g.provision.count, 2);
        assert_eq!(g.provision.p95, Some(30_000));
        assert_eq!(g.step("boot").ran.count, 1);
        assert_eq!(g.step("create").ran.count, 0);
        assert_eq!(g.step("create").ran.p50, None);
    }

    #[test]
    fn test_by_image_falls_back_to_labels() {
        let mut a = run("a", 0, Some("completed"));
        a.image = Some("macos-15".into());
        let mut b = run("b", 0, Some("completed"));
        b.labels = vec!["linux".into(), "x64".into()];
        let groups = by_image(&[a, b]);
        let keys: Vec<&str> = groups.iter().map(|g| g.key.as_str()).collect();
        assert!(keys.contains(&"macos-15"));
        assert!(keys.contains(&"labels: linux, x64"));
    }

    #[test]
    fn test_job_breakdown_uses_the_runner_that_ran_it() {
        let mut failed = run("a", 10_000, Some("failed"));
        failed.job_id = Some("github:1".into());
        failed.started_at = Some(11_000);
        let mut ok = run("b", 20_000, Some("completed"));
        ok.job_id = Some("github:1".into());
        ok.started_at = Some(50_000);
        let job = JobRun {
            job_id: "github:1".into(),
            queued_at: 1_000,
            seen_at: 3_000,
            started_at: Some(52_000),
            runner_name: Some(ok.runner_name.clone()),
            attempts: 2,
            outcome: Some("ran".into()),
            ..Default::default()
        };
        let s = jobs(&[job], &[failed, ok]);
        assert_eq!(s.ran, 1);
        assert_eq!(s.mean_attempts, Some(2.0));
        assert_eq!(s.wait.p50, Some(51_000));
        assert_eq!(s.to_coordinator.p50, Some(2_000));
        assert_eq!(s.dispatch.p50, Some(17_000));
        assert_eq!(s.provision.p50, Some(30_000));
        assert_eq!(s.pickup.p50, Some(2_000));
    }

    #[test]
    fn test_buckets() {
        let now = 10 * DAY_MS + 5 * HOUR_MS;
        let starts = bucket_starts(now - Range::Day.duration_ms(), now, HOUR_MS);
        assert_eq!(starts.len(), 25);
        assert_eq!(starts[0], 9 * DAY_MS + 5 * HOUR_MS);
        assert_eq!(*starts.last().unwrap(), now);

        let runs = vec![
            run("a", now - 30 * 60_000, Some("completed")),
            run("a", now - 2 * DAY_MS, Some("completed")),
        ];
        let agents = by_agent(&runs);
        let c = charts(&runs, &[], &agents, &step_names(&runs), Range::Day, now);
        assert_eq!(c.buckets.len(), 25);
        assert_eq!(c.outcomes.completed.iter().sum::<usize>(), 1);
        assert_eq!(c.outcomes.completed[23], 1);
        assert_eq!(c.provision.len(), 1);
    }

    #[test]
    fn test_skipped_steps() {
        let runs: Vec<RunnerRun> = [0, 0, 0, 20_000]
            .into_iter()
            .enumerate()
            .map(|(i, install)| {
                let mut r = run("a", i as i64, Some("completed"));
                r.steps.insert("install".into(), install);
                r
            })
            .collect();
        let install = by_agent(&runs)[0].step("install");
        assert_eq!(install.skipped, 3);
        assert_eq!(install.skipped_pct(), Some(75.0));
        assert_eq!(
            install.ran.p50,
            Some(20_000),
            "a download's time leaves out the cached runs"
        );
        assert_eq!(install.typical.p50, Some(0));
    }

    fn install_run(image: &str, source: Option<&str>, install: Option<i64>) -> RunnerRun {
        let mut r = run("a", 0, Some("completed"));
        r.image = Some(image.into());
        r.install_source = source.map(String::from);
        if let Some(ms) = install {
            r.steps.insert("install".into(), ms);
        }
        r
    }

    #[test]
    fn test_install_sources() {
        let runs = [
            install_run("linux", Some("hit"), Some(3_000)),
            install_run("linux", Some("hit"), Some(5_000)),
            install_run("linux", Some("filled"), Some(9_000)),
            // today's agents, and history from before they said
            install_run("linux", None, Some(40_000)),
            // a failed install has no time, but it did happen
            install_run("linux", Some("fell_back"), None),
            install_run("mac", None, Some(0)),
            // never got as far as installing
            install_run("mac", None, None),
        ];
        let got: Vec<_> = by_install(&runs)
            .into_iter()
            .map(|g| (g.image, g.source, g.runs, g.install.count, g.install.p50))
            .collect();
        let row = |image: &str, source: &str, runs, count, p50| {
            (image.to_string(), source.to_string(), runs, count, p50)
        };
        assert_eq!(
            got,
            vec![
                row("linux", "hit", 2, 2, Some(3_000)),
                row("linux", "filled", 1, 1, Some(9_000)),
                row("linux", "fell_back", 1, 0, None),
                row("linux", SOURCE_NOT_RECORDED, 1, 1, Some(40_000)),
                row("mac", SOURCE_IMAGE, 1, 0, None),
            ]
        );
    }

    #[test]
    fn test_unknown_install_source_is_kept_and_sorts_last() {
        let runs = [
            install_run("linux", Some("zz-newer-agent"), Some(1_000)),
            install_run("linux", None, Some(0)),
        ];
        let sources: Vec<_> = by_install(&runs).into_iter().map(|g| g.source).collect();
        assert_eq!(sources, vec![SOURCE_IMAGE, "zz-newer-agent"]);
    }

    #[test]
    fn test_step_names_keep_known_order_then_extras() {
        let mut r = run("a", 0, None);
        r.steps.insert("zz-new".into(), 1);
        r.steps.insert("clock-sync".into(), 1);
        assert_eq!(
            step_names(&[r]),
            [
                "create",
                "boot",
                "install",
                "setup",
                "job",
                "destroy",
                "clock-sync",
                "zz-new"
            ]
        );
    }

    #[test]
    fn test_fmt_ms() {
        assert_eq!(fmt_ms(None), "-");
        assert_eq!(fmt_ms(Some(850)), "850ms");
        assert_eq!(fmt_ms(Some(42_300)), "42.3s");
        assert_eq!(fmt_ms(Some(192_000)), "3m 12s");
        assert_eq!(fmt_ms(Some(7_500_000)), "2h 5m");
    }
}
