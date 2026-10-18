# Forge Support (GitHub, Forgejo, Gitea)

Status of the work to let kuiper-forge provision runners for forges other than GitHub,
starting with Forgejo. Design details for the polling side live in
[polling-mode.md](polling-mode.md).

## Goal

One coordinator serving several forges at once (for example GitHub plus a self-hosted
Forgejo), sharing the same agents, with each forge discovering jobs by webhook or by
polling, whichever it supports.

## Status

| # | Step | State |
|---|------|-------|
| 1 | Polling provisioning mode (GitHub) | **Done** |
| – | Webhook-mode recovery scan applies `label_mappings` | **Done** (fix) |
| 2 | Multi-forge groundwork: `Forge` trait, job keys, `[[forges]]` config, DB migration | **Done** |
| 3 | Forgejo backend + `forgejo-runner one-job --handle` on proxmox Linux VMs | **Done** |
| 4 | Idle runner timeout (job listed as waiting but never starts) | Next |
| 5 | End-to-end tests against Forgejo 16 and 17 in Docker | Planned |
| 6 | Forgejo 17 job webhooks (`workflow_job_*`) into the same queue | Planned |
| – | Gitea backend (webhook, or polling via its 1.25+ jobs API) | Later |
| – | macOS (tart) / Windows Forgejo runners | Blocked on runner binaries (see open questions) |

### Done

**Polling mode.** `provisioning.mode = "polling"` finds queued jobs by scanning each
forge every `interval_secs` (default 120, min 10, warning below 60) and feeds the same
DB-backed pending queue webhook mode uses. Label mappings are applied through
`webhook::resolve_runner_scope`, shared with the webhook handler. A pending job a scan
no longer lists is checked with its forge before it's dropped, because the GitHub scan
silently skips repos whose API calls fail. The GitHub scan reads every page of workflow
runs, of each run's jobs, and of App installations, so large matrix runs aren't stuck
behind the first page. See [polling-mode.md](polling-mode.md) and
[provisioning-flow.md](provisioning-flow.md).

**Multi-forge groundwork.**

- `forge.rs`: the `Forge` trait (formerly `github::RunnerTokenProvider`, plus
  `scope_url`), `ForgeRegistry` keyed by configured name, and `MockForge` for dry-run.
  `GitHubClient` implements `Forge`.
- Jobs are identified by `JobKey`: the forge's configured name plus the forge's own job
  id, stored as text `forge:id` (`github:123456`). A bare id parses as the legacy
  `github` forge.
- `active_runners.forge` records which forge a runner is registered with (fixed-capacity
  runners have no job). `NULL` means the default forge.
- Config: a `[[forges]]` list (`name`, `type = "github"`, credentials). A legacy
  `[github]` section is a forge named `github`. The first forge is the default:
  fixed-capacity pools and `[runner_scope]` register there. Label mappings take an
  optional `forge`; `[provisioning.webhook]` takes an optional `forge` (default: the
  first GitHub forge). Webhooks are GitHub-only for now.
- Scans list every forge; a forge that fails to list drops none of its pending jobs.
- Admin UI shows each job's forge.

**Forgejo backend.**

- Config: `[[forges]]` entries with `type = "forgejo"`, `url`, a token (`token_env`,
  `token_path` or `token`, in that order) and `scopes`: the orgs (`org`) and repos
  (`owner/repo`) to poll and register runners in. Forgejo tokens can't list what they
  can reach, so scopes must be named. Startup lists jobs in every scope to check the
  token (no `read:user` scope needed).
- Polling mode only: Forgejo can't be the first forge in fixed-capacity mode (pools
  register long-lived runners there), and `[provisioning.webhook].forge` must be a
  GitHub forge. In webhook mode a Forgejo forge is only picked up by the 5-minute
  recovery scan (a warning says so).
- `forgejo.rs`: `ForgejoClient` implements `Forge`. It lists
  `…/actions/runners/jobs` per scope and keeps `status == "waiting"`; a job's key is its
  `handle`. Repo names come from `/repositories/{id}`, cached. Forgejo answers `null`
  for an empty list, which counts as empty. A scope that fails is skipped (the others
  still count); if every scope fails the listing fails.
- Job status: the job's handle in its scope's list. `waiting` is reported as `queued`
  and `running` as `in_progress` (the fleet manager speaks GitHub's terms); a handle
  that's gone means the attempt finished or was cancelled. A repo under a configured
  org is asked at org scope, so an org-owner token works.
- Runners: `POST …/actions/runners {"name", "ephemeral": true}` per job; the coordinator
  sends the agent `CreateRunnerCommand.forgejo` (`ForgejoRunner`: instance URL, uuid,
  token, handle). Removal lists runners (paged) and deletes by name; Forgejo usually
  removes ephemeral runners itself first.
- `Forge::create_job_runner` replaces `generate_jitconfig` and returns a
  `RunnerLaunch` (GitHub JIT config or Forgejo runner). `Forge::runner_kind` says which
  runner program a forge needs.
- Agent protocol: agents list the runner programs they can start besides GitHub's in
  `AgentStatus.runner_kinds`. A Forgejo job only goes to an agent listing `forgejo`, so
  older agents never get one. The proxmox agent lists it; the tart agent doesn't (no
  macOS forgejo-runner release) and rejects Forgejo commands.
- Proxmox agent (`kuiper-agent-lib/src/forgejo_runner.rs`): on a Linux VM it installs
  forgejo-runner into `~/forgejo-runner` if missing (`vm.forgejo_runner_version`,
  default `latest`, falling back to 13.2.0; at least 13.1.0 for `--handle`), checks the
  release's sha256, writes the token to a mode-600 file and a `run-job.sh`, then runs
  `forgejo-runner one-job --url … --uuid … --token-url file:… --handle … --wait` with
  each label as `label:host`, so steps run on the VM itself. Windows VMs fail with a
  clear error.
- VM templates need `curl`, `sha256sum` and `bash`, plus whatever the workflows use.
  Jobs run on the host, so Docker-based steps (`container:`, `services:`) aren't
  supported, and JavaScript actions such as `actions/checkout` need `node` on the VM.
- `required_labels` is global and defaults to `["self-hosted"]`, which Forgejo jobs
  usually don't have: set it (for example to `[]`) when polling Forgejo.
- Renamed: `scan_github` → `scan_forges`, `recover_queued_jobs_from_github` →
  `recover_queued_jobs`, `verify_pending_jobs_with_github` → `verify_pending_jobs`, and
  forge-neutral log messages.

**Tested.** Unit tests cover the client against a fake Forgejo API, the config, agent
selection by runner kind, and the generated VM commands. End to end against a local
Forgejo 17.0.0 in Docker: the coordinator in polling mode with a Forgejo forge and a
throwaway agent speaking the real agent protocol that ran the agent library's
install/prepare/run commands in a local shell instead of a VM. A one-job workflow and a
two-job (`needs:`) workflow each got exactly one runner per job, every job succeeded,
and the pending and runner records were cleared afterwards. The proxmox agent's own
wiring (`run_forgejo_job`) is only compile- and unit-tested; it hasn't run on a real
Proxmox VM yet. Forgejo 16 wasn't tested (step 5).

### Migration notes

`20261017000000_job_keys` converts `job_id` to text in `pending_webhook_jobs`,
`active_runners` and `agent_failures`, rewriting existing values to `github:<id>`, and
adds `active_runners.forge`.

- SQLite can't change column types, so it rebuilds the three tables. SQLite now runs a
  second migration set (`migrations/sqlite/`) after `migrations/shared/`, like Postgres
  already did; both use `set_ignore_missing` since they share `_sqlx_migrations`.
- Postgres alters the column types in place (`migrations/postgres/`).
- Tested: a unit test upgrades an old-format SQLite database with rows; the Postgres path
  was tested by migrating a database created by the previous release.
- If a forge is later renamed (or a `[github]` section is replaced by a `[[forges]]`
  entry with another name), old `github:` rows won't match a configured forge. Runner
  cleanup then falls back to the default forge, and such pending jobs are dropped after
  their retries. Queue data is short-lived, so this is accepted.

## Forgejo findings

Verified against Forgejo source (v16.0.5, v17.0.0) and a live 17.0.0 instance.

- **Jobs API (≥ 15):** `GET …/actions/runners/jobs` at repo, org, user and admin scope.
  Returns waiting **and** running jobs (filter on `status == "waiting"`), unpaginated,
  with `id`, `attempt`, `handle`, `repo_id`, `runs_on`, `status`. No repo name, only
  `repo_id`.
- **Ephemeral runners (≥ 15):** `POST …/actions/runners {"name", "ephemeral": true}`
  returns `uuid` and `token`; the VM runs
  `forgejo-runner one-job --uuid … --token-url file:… --handle <handle> --wait`.
  `JobKey` for Forgejo will be the `handle` (one per attempt, so reruns are new jobs).
- **Job webhooks:** only from Forgejo 17.0 (events `workflow_job_waiting`, `_running`,
  `_success`, `_failure`, `_cancelled`, `_blocked`, `_skipped`). 15/16 need polling.
- **Token permissions** (tested, 8 tokens × 8 endpoints):

  | Runners at | Token scope | User must be |
  |------------|-------------|--------------|
  | Repository | `write:repository` (repo-limited tokens work) | repo owner (admin collaborators get 403) |
  | Organization | `write:organization` | org owner |
  | Instance | `write:admin` | site admin |

  Creating webhooks needs a full (not repo-limited) `write:repository` token, or
  `write:organization` for org hooks. There is no GitHub App equivalent.

## Open questions

- forgejo-runner release binaries for macOS and Windows: not published (Linux amd64 and
  arm64 only, as of v13.2.0). Tart and Windows runners would need source builds.
- Poll per org/repo scope, or use the admin scope when a site-admin token is configured?
  (Org and repo scopes for now.)
- Idle timeout default, and whether it applies to GitHub runners too (step 4). Until
  then, a runner whose job never starts waits in `one-job --wait` until its VM is
  destroyed.
- `required_labels` per forge, so a GitHub + Forgejo setup can keep `self-hosted` for
  GitHub only.
