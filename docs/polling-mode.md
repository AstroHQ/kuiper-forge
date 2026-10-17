# Polling Provisioning Mode

Status: **step 1 implemented** (GitHub polling); Forgejo steps proposed

### Implementation notes (step 1)

- `[provisioning.polling]` is its own section rather than a shared on-demand one;
  `ProvisioningConfig::on_demand_filters()` returns the active mode's filters so the
  scan reads the same fields either way. Webhook configs are unchanged.
- Default `interval_secs` is 120 for GitHub (minimum 10, warning below 60).
- `list_queued_jobs` skips repos whose API calls fail and repos outside the lookback
  window, so a job missing from one scan isn't proof it's gone. Pending jobs the poll
  didn't list are checked with `get_job_status` before they're dropped, rather than
  removed outright.
- Label mappings are applied to polled jobs through `webhook::resolve_runner_scope`,
  shared with the webhook handler. The webhook-mode recovery scan now applies them too;
  before, it accepted recovered jobs the webhook handler would have ignored.

A third provisioning mode, alongside `fixed_capacity` and `webhook`, in which the
coordinator discovers queued jobs by periodically asking the forge's API rather than
waiting for webhook deliveries. Runners are still created on demand, one ephemeral VM
per job, exactly like webhook mode.

## Motivation

- **Forgejo 15/16 has no job webhooks.** Workflow job webhook events first shipped in
  Forgejo 17.0 (2026-10-15). Instances that can't or won't upgrade yet (ours is on
  16.0.5) need another way to learn about queued jobs. Forgejo 15+ does expose a cheap
  queued-jobs API (see [Forgejo specifics](#forgejo-specifics)).
- **Coordinators that webhooks can't reach.** A coordinator behind NAT or on a private
  network can poll outbound without exposing an HTTP endpoint to the forge.
- **Most of the machinery already exists.** Webhook mode already has a DB-backed
  pending-job queue, a 30s processing ticker, and a 5-minute GitHub recovery scan
  (`recover_queued_jobs_from_github`) that turns API results into pending jobs. Polling
  mode promotes that scan from a safety net to the primary job source.

## Design

### Job sources feed one pending queue

Today, the webhook handler and the recovery scan both build a `WebhookRunnerRequest`
and write it into `PendingJobStore`; everything downstream (agent matching, retries,
dedup against `runner_state`, recovery) reads from that queue. Polling mode keeps that
shape and makes the split explicit:

```
            ┌───────────────┐
 webhook ──►│               │
            │  PendingJob   │──► process_pending_jobs ──► create_runner_on_demand ──► agent
 poller  ──►│  queue (DB)   │
            └───────────────┘
```

| Mode | Job sources |
|------|-------------|
| `webhook` | webhook (primary) + periodic recovery scan (every 5 min, unchanged) |
| `polling` | poller (every `interval_secs`) |

No behaviour change for `webhook` mode. When Forgejo 17 is adopted, Forgejo gets the
same webhook + recovery-scan hybrid that GitHub has today; no separate hybrid mode.

### Configuration

```toml
[provisioning]
mode = "polling"            # fixed_capacity | webhook | polling

[provisioning.polling]
interval_secs = 30          # default 30; see GitHub notes for a slower default there
required_labels = []        # pre-filter; ALL must be present on the job
label_mappings = [
  { labels = ["linux", "x64"], runner_scope = { organization = { name = "astro" } } },
]
```

`required_labels` and `label_mappings` mean the same as in webhook mode and should move
to a shared on-demand section used by both modes. For backward compatibility they stay
accepted under `[provisioning.webhook]`.

`required_labels` defaults to `["self-hosted"]` today. That is a GitHub convention;
Forgejo jobs use plain labels like `runs-on: debian`, so the default must be per forge
(empty for Forgejo/Gitea) or the filter would drop every job.

### Poll loop

Runs in `FleetManager` on the existing 30s ticker (as an elapsed-time check, the same
pattern `github_verify_interval` uses to avoid `select!` starvation):

1. `forge.list_queued_jobs()` for each configured scope.
2. Keep only jobs that are actually queued (Forgejo returns running jobs too).
3. Apply `required_labels`, then `label_mappings` (`match_labels`), exactly as the
   webhook handler does.
4. Skip jobs already in `PendingJobStore` or already holding a runner
   (`runner_state.runners_for_job`). This is the existing dedup in
   `recover_queued_jobs_from_github`.
5. `pending_job_store.add_job()` for the rest, then `process_pending_jobs()`.

Steps 3–5 are shared with the webhook handler and the recovery scan; the refactor should
pull them into one function so the three paths can't drift.

### Completion and cleanup without events

Webhook mode removes a pending job when `workflow_job.completed` arrives. In polling mode:

- A pending job (no runner yet) that **no longer appears as queued** on the next poll was
  cancelled or taken by another runner → remove it from the pending queue. This is what
  `verify_pending_jobs_with_github` already does on its 5-minute cycle; in polling mode it
  falls out of each poll's result for free.
- Jobs that already have a runner are cleaned up the way they are today: the agent
  reports the VM/runner exit via `RunnerEvent`, and the runner is ephemeral on the forge.

### Idle runner timeout

A job can be listed as waiting but not yet be runnable (for example, held back by a
`concurrency` group). With Forgejo's `one-job --handle --wait`, the VM created for it
will sit idle until the job starts. Polling mode needs:

- an **idle timeout** (config, default e.g. 30 min): if the runner hasn't started its job
  by then, destroy the VM and remove the runner registration;
- **no duplicate runner** for a job/handle that already has one, even if it is still
  listed as waiting (step 4 above already guarantees this).

## Job identity

Pending jobs and active runners are keyed by a numeric `job_id: u64`
(`pending_webhook_jobs.job_id`, `active_runners.job_id`). Forgejo identifies a specific
run attempt by `handle` (a string); `id` stays the same across reruns while `attempt`
increments, and Forgejo's guidance is to use `handle` rather than build a key from the
other fields.

Proposal: replace `job_id: u64` with an opaque `JobKey(String)` plus a `forge` column.

- GitHub: `JobKey` = the workflow job id as a string.
- Forgejo: `JobKey` = `handle`. A rerun is a new attempt, gets a new handle, and is
  correctly treated as a new job.

This needs a DB migration for both SQLite and Postgres (`sql.rs` has both dialects).
The same change is a prerequisite for multi-forge support generally.

## Forgejo specifics

Verified against Forgejo source (v16.0.5 and v17.0.0) and a live 17.0.0 instance.

**Queued jobs endpoint** (Forgejo ≥ 15):
`GET /api/v1/{repos/{owner}/{repo} | orgs/{org} | user | admin}/actions/runners/jobs`

- Returns jobs with status **waiting and running** (`StatusWaiting, StatusRunning` in
  `routers/api/v1/shared/runners.go`); the poller must filter on `status == "waiting"`.
- Not paginated; returns every matching job in the scope.
- Optional `?labels=a,b` filter: a job is included only if its `runs_on` is satisfied by
  the given labels.
- Fields: `id`, `run_id`, `attempt`, `handle`, `repo_id`, `owner_id`, `name`, `needs`,
  `runs_on`, `task_id`, `status`.
- No repository name, only `repo_id`; resolve via `GET /api/v1/repositories/{id}` and
  cache, for display and logging.
- One call covers every repo in an org, so a 15–30s interval is cheap.

**Runner registration** (Forgejo ≥ 15): `POST .../actions/runners` with
`{"name": "...", "ephemeral": true}` returns `id`, `uuid`, `token`. The VM then runs:

```
forgejo-runner one-job --url <forgejo> --uuid <uuid> --token-url file:<path> \
  --label <label>:host --handle <handle> --wait
```

`--handle` pins the runner to that exact job attempt. Forgejo deletes ephemeral runner
registrations after their one job completes.

**Token permissions** (tested on 17.0.0, 8 tokens × 8 endpoints):

| Scope of runners | Token needs | User must be |
|------------------|-------------|--------------|
| Repository | `write:repository` (repo-specific tokens work) | repo **owner** (an admin collaborator gets 403) |
| Organization | `write:organization` | org owner |
| Instance | `write:admin` | site admin |

`read:*` of the same category is enough to list jobs. Scopes don't cross levels: an
admin-scoped token gets 403 on repo and org runner endpoints. There is no GitHub App
equivalent; OAuth2 apps act as a user. Least privilege for kuiper: a dedicated bot user
that owns only the orgs/repos kuiper serves.

## GitHub specifics

Polling mode works with GitHub using today's `list_queued_jobs`, but that scan is
expensive: GitHub has no documented org-wide queued-jobs endpoint, so it walks each
recently pushed repo (`queued_scan_lookback_hours`) and lists its workflow runs. With
GitHub App rate limits, polling every 30s across many repos isn't viable.

- Default `interval_secs` for GitHub should be slower (2–5 min) and the coordinator
  should warn when it's set lower.
- [actions-service-api-research.md](actions-service-api-research.md) describes the runner
  scale set "acquirable jobs" API ARC uses: one call (or a long-poll) for all queued
  jobs in an org. It's undocumented, but it would make GitHub polling as cheap as
  Forgejo's. Worth a follow-up as an alternative `list_queued_jobs` backend; out of scope
  here.

## Gitea

Gitea 1.24+ has a `workflow_job` webhook and ephemeral runners, so webhook mode is the
natural fit. Gitea 1.25+ also has a GitHub-style jobs list,
`GET /api/v1/{repos/{owner}/{repo} | orgs/{org} | user | admin}/actions/jobs?status=queued`,
returning `id`, `run_id`, `run_attempt`, `labels`, `status` and more (checked in Gitea
source; not present in 1.24.0). That is enough for polling mode, as a backup scan in
webhook mode, or as a primary source. There is no Forgejo-style `handle`, so a runner
can't be pinned to one job; jobs are matched by labels only.

## Implementation plan

| # | Change | Size |
|---|--------|------|
| 1 | `polling` mode for GitHub on current code: config, shared on-demand label config, poll loop, drop-off cleanup, tests | ~1 day |
| 2 | Forge abstraction (generalise `RunnerTokenProvider`), `JobKey` + `forge` column migration, `[[forges]]` config; GitHub unchanged | ~1.5–2 days |
| 3 | Forgejo backend: queued jobs, ephemeral registration, delete; proxmox agent launches `forgejo-runner one-job --handle` on Linux | ~2 days |
| 4 | Idle runner timeout | ~0.5 day |
| 5 | End-to-end tests against Forgejo 16 and 17 in Docker | ~1 day |
| 6 | Forgejo 17 webhooks (`workflow_job_*` events) feeding the same queue | ~0.5–1 day |

Step 1 ships on its own and lays the groundwork the Forgejo work needs.

## Open questions

- forgejo-runner release binaries for macOS and Windows: needed for tart agents and
  Windows VMs; if not published, build from source or limit Forgejo to Linux VMs initially.
- Should the poller run one request per org/repo scope, or use the `admin` scope when a
  site-admin token is configured (one call for the whole instance)?
- Idle timeout default, and whether it applies to GitHub runners too.
- Forgejo 17 changed webhook headers (`X-*-Event`, `X-*-Event-Type`) and deprecated
  `action_run_*` events; confirm the payload shape before step 6.
