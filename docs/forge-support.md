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
| 3 | Forgejo backend + `forgejo-runner one-job --handle` on proxmox Linux VMs | Next |
| 4 | Idle runner timeout (job listed as waiting but never starts) | Planned |
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
silently skips repos whose API calls fail. See [polling-mode.md](polling-mode.md) and
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

- forgejo-runner release binaries for macOS and Windows: needed for tart agents and
  Windows VMs. If not published, build from source or limit Forgejo to Linux at first.
- Agent protocol: `CreateRunnerCommand` is GitHub-shaped (`registration_token`,
  `jit_config`). Step 3 adds a Forgejo launch variant; agents must advertise support so
  older agents never receive Forgejo jobs.
- Poll per org/repo scope, or use the admin scope when a site-admin token is configured?
- Idle timeout default, and whether it applies to GitHub runners too.
- Internal names and log messages still say "GitHub" in places (`scan_github`,
  `recover_queued_jobs_from_github`); rename as Forgejo lands.
