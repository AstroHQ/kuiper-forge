Real `workflow_job_*` deliveries from Forgejo 17.0.0 (Docker, `codeberg.org/forgejo/forgejo:17`),
captured 2026-11-15 from an org webhook on `lodge` (type `forgejo`, JSON, secret
`test-webhook-secret`) with a forgejo-runner 13.2.0 taking the jobs. Each file is
`{"headers": {...}, "body": "<raw body>"}`; the signatures match the raw body, so keep it byte for byte.

- `waiting.json`: `new_job_attempt`, job `build` queued (no `needs`).
- `blocked.json`: `new_job_attempt`, job `fails` held by `needs: build`.
- `waiting-after-needs.json`: `job_status_changed` to waiting once `build` finished.
- `running.json`, `success.json`: `build` taken by a runner and finished.
- `failure.json`: `fails` finished with `exit 1`.
- `cancelled.json`: a waiting job (`slow`) cancelled from the web UI.

Forgejo's API ignores `workflow_job_*` in a hook's `events`; the hook was made in the web UI.
