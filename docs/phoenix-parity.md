# Phoenix coordinator parity checklist

What the Rust coordinator (`kuiper-forge/`, v0.9.1, main @ 15f3134) does that the Phoenix app (`kuiper_coordinator/`) must match, so the existing tart/proxmox/libvirt agents connect unchanged. Source refs are repo-relative (`kuiper-forge/src/<file>:<line>` unless the path says otherwise). "Phoenix test" is the test that should guard the item in `kuiper_coordinator/test`; where a Rust test exists, port it.

Written on 2026-11-21 from the source and the merged PR / issue history. PR numbers are the merge that introduced the commit.

## Hard-won fixes (must carry over, each with a guarding test)

| Fix | Introduced | Where | Phoenix test |
|---|---|---|---|
| Destroy a VM only while its VMID still belongs to it (reused VMID) | #38, 5417dec (issues #53, #54) | proxmox agent; coordinator must send the exact created vm name in DestroyRunner (F8) | DestroyRunner carries the created name; e2e in open #52 |
| Forgejo runner paging until an empty page | #38, a9a1f17 | forgejo.rs:328-355 (FJ3) | port forgejo.rs:963, 995 |
| GitHub list pagination (queue discovery) | #37, 32b590d | github.rs:38-59 (G5) | port github.rs:1111-1161 |
| Agent update release paging; failed page keeps last good | #75, 7a3d524 | agent_updates.rs:145-196 (U1) | port agent_updates.rs:394 |
| Lost-VM 20 s grace | #38, cd27340 | runner_state.rs:346-384 (F9) | port runner_state.rs:488 |
| Idle timeout: 600 s default, 120 s floor, backoff to 1 h | #38 | fleet.rs:53-70, 587-741 (F13) | port fleet.rs:2777, 2792; config.rs:1795 |
| Open #49: idle clock from creation kills booting runners | open | fleet.rs:620-623 (F14) | new: slow-registering runner survives |
| Open #48: repo-less (org/@user/instance) runners never idle out | open | fleet.rs:635-637 (F15) | new: org-scoped idle runner torn down |
| Drain idle pool runners from unavailable agents, 10 min boot grace | #40, a1ab136 | fleet.rs:2154-2254 (F16) | port fleet.rs:2876 |
| Pool count from DB, exact label match (lockup ENG-4829) | #12, 6713129, f3e8dd4 | fleet.rs:2310-2346 (P3) | superset-label runner not counted |
| Runner row before CreateRunner (no dangling forge runners) | #24, 84c231d | fleet.rs:1296-1309 (F3) | timeout cleans forge runner + row |
| Never lose queued jobs at max capacity; webhook 503 on DB failure | #4, 381f5d5 | fleet.rs, webhook.rs (J1, W2) | full fleet keeps jobs |
| Stuck queued jobs: requeue on VM start failure, recovery scan, assignment assumptions | #16 211e86f, e76d217; #27 483f053; #31 1963a34 | fleet.rs (J3, J5) | jobs run once capacity frees |
| Avoid failed agents on retry | #30, aaedcf2 | agent_registry.rs:617-713, pending_jobs.rs:193-252 (R4, J4) | port agent_registry.rs:2240, pending_jobs.rs:454 |
| Label mappings applied to recovered jobs | #37, 8879858 | fleet.rs:1019-1023 (J6) | recovered job gets mapped scope |
| Default required label `self-hosted` | #4, e8b974a | config.rs:302-304 (W5) | job without it ignored |
| Single-use registration token, atomic consume | #4, c624085 | auth.rs:123-148 (A11) | concurrent redeem: one wins |
| Revoke not clobbered by status sync; revoked purge 7 d | #24 f6a2b42; #27 1909a4f | auth.rs:228-244, 338-367 (A13, A14) | revoke survives sync |
| max_vms/labels always current from agents; capacity/reserved-slot fixes | #24 ec77e28; #16 392784e; #33 fb739ed, d7ccf7d, c8b62b5 | agent_registry.rs (R1-R6) | port capacity tests |
| Disconnect failover grace / race fixes | #12, 6713129 | server.rs:644-682 (S9) | reconnect within 30 s keeps runners |
| Cancel replaces a started job's outcome | #67, 76b7e83 | metrics/sql.rs:127-175 (MX5) | port metrics/mod.rs:657-704 |
| Last-admin delete in a transaction; log prune fix | #32, 176390d | admin/auth.rs:174-217 (UI8) | concurrent deletes keep one admin |
| Setup prints webhook steps for the webhook's forge; setup checks certs usable | #81 e94a543; #70 f046479 | setup.rs, tls.rs:56-96 (C8, A4) | — |
| Optional PROXY protocol | #4, #34 55ed2ff | server.rs:84-158 (A3) | direct and PROXY both work |

## Rust behaviors to decide, not copy blindly

1. `unregister` can evict a fresh reconnect (S10). Key by connection in Phoenix.
2. `runner_group` names silently become group 1 (J7).
3. `waiting` treated three different ways (J8); Forgejo scope order differs between status check and webhook (FJ6).
4. Revoked agent's live stream stays open (A15).
5. Recovery watcher cleans up after disconnect despite its comment (F11).
6. Admin: no CSRF/rate limit, login timing leak, no `Secure` cookie (UI10).
7. Management socket: no chmod, unlinks an existing socket; Phoenix needs its own path (M6). Same for listen port (C5).
8. Agent cert issuer DN rebuilt from O (A7): sign with the real CA cert.
9. TLS cert hot reload (open #78) is cheap to add in Phoenix (`:ssl` sni_fun / reload).

Other open issues that touch the coordinator: #51 (PG test DB in CI; Phoenix CI already uses a Postgres service), #52 (real Proxmox Forgejo e2e), #47, #23, #50 (agent-side, out of scope).

## 1. Agent connection, CA and certificates

Contract: `kuiper-agent-proto/proto/agent.proto`. Three RPCs on one TLS port:
`RegistrationService.Register(RegisterRequest) -> RegisterResponse` (agent.proto:16, server-TLS only),
`AgentService.AgentStream(stream AgentMessage) -> stream CoordinatorMessage` (agent.proto:47, mTLS),
`AgentService.UploadLogs(LogBatch) -> UploadLogsResponse` (agent.proto:51, mTLS; a coordinator without it must answer UNIMPLEMENTED so agents stop uploading, agent.proto:49-50).

### 1a. TLS, CA, registration

| # | Behavior | Source | Phoenix test |
|---|---|---|---|
| A1 | TLS port: client certs optional at the TLS layer, enforced per RPC (Register works without one). ALPN h2 + http/1.1 on the combined port, h2 only on the gRPC-only path. | server.rs:959-971, 1214-1225, 1336 | Register without a client cert succeeds; AgentStream without one is rejected `Unauthenticated` |
| A2 | Content-type `application/grpc*` routes to gRPC, everything else to HTTP (webhook/admin) on the same listener. | server.rs:1104-1108 | (only if Phoenix shares the port; otherwise document the split) |
| A3 | Optional PROXY protocol v1/v2 sniffed by first byte (`P` / 0x0D); 5 s header read timeout; direct connections still work. Added #4, made optional in #34 (55ed2ff). Real client address is parsed then discarded. | server.rs:84-158, 1047-1048, 1255 | Direct and PROXY v1 connections both handshake |
| A4 | Server identity self-check at startup: key matches cert, chain verifies for hostname against the CA (skipped with native roots). From #70 (f046479). | tls.rs:56-96 | Startup fails on mismatched key/cert |
| A5 | `server_trust_mode` "ca" / "chain" sent in RegisterResponse; CA PEM is sent unless chain mode with native roots. | tls.rs:24-41, server.rs:268-273 | Register response carries the CA PEM in "ca" mode |
| A6 | CA: ECDSA P-256, 10 y, key file 0600; `init_ca` refuses to overwrite. **D2: Phoenix loads this CA.** | auth.rs:607-659, 784-803 | Phoenix-issued client cert verifies against the existing CA cert file |
| A7 | **Issuer DN quirk:** Rust signs agent certs with a CA rebuilt from the on-disk O field ("{org} CA"), not the cert as-is. Phoenix should sign with the CA cert's real subject (`:public_key.pkix_sign` with the loaded cert), which is equal whenever the CA was made by `init_ca`. | auth.rs:405-418, 835-856 | Cert issued by Phoenix and one issued by Rust both chain to the same CA under `:ssl` verify_peer |
| A8 | Agent client cert: server generates the P-256 key, 365 d, CN = SAN DNS = agent_id, O "CI Runner Agent", EKU ClientAuth; key returned once in `client_key_pem`. | auth.rs:491-524 | Fields asserted on a decoded cert |
| A9 | agent_id = `agent_{type}_{sanitized_hostname}_{8 alnum}`; sanitize maps non-alnum/`-` to `-`, lowercases. | auth.rs:483-488, 815-827 | Port the auth.rs:872 unit test |
| A10 | agent_type rule: 1-32 chars, `[a-z0-9-]` after lowercasing; any type accepted (#64, b90354c). Validated **before** consuming the token so a bad type doesn't burn it (InvalidArgument). Other failures: `Unauthenticated "Registration failed: …"`. | agent_registry.rs:45-58, server.rs:236-255 | Bad type leaves the token usable |
| A11 | **Registration token single use, atomic** (`DELETE … RETURNING`), closed a reuse window in #4 (c624085). Token consumed before the expiry check, so an expired token is burned. Format `reg_` + 32 alnum, TTL from caller (proto says default 1 h). | auth.rs:123-148, 435-480; sql.rs | Two concurrent Registers with one token: exactly one succeeds |
| A12 | Placeholder agent row at registration (labels [], max_vms 0, expires +365 d), overwritten by the first status. | auth.rs:462-469, 527-539 | — |
| A13 | Expired tokens purged hourly (#4, 1520f0f); revoked agents get `revoked_at` and are purged after 7 d (#27, 1909a4f), never affecting auth. | auth.rs:209-222, 338-367; main.rs:672-690 | Purge leaves live tokens and unrevoked agents |
| A14 | Metadata sync updates only labels/max_vms/version so a concurrent revoke isn't undone (#24, f6a2b42). `max_vms` read as i32 on Postgres (#38, 5f8acd2). | auth.rs:228-244, 79-94 | Revoke then status sync: agent stays revoked |
| A15 | Identity = first peer cert's CN; `agent_id` in messages is ignored. Every AgentStream open and UploadLogs checks "exists, not revoked, not expired". A live stream is **not** closed on revoke (Phoenix may choose to; decide). | server.rs:318-380, 411-424; auth.rs:370-381 | Revoked agent's next stream open gets `Unauthenticated` |
| A16 | No cert renewal path (365 d, re-register). TLS certs read once; hot reload is open issue **#78**. | server.rs:934-939 | Optional: Phoenix reloads server cert without restart (closes #78 for the new app) |

### 1b. AgentStream protocol

| # | Behavior | Source | Phoenix test |
|---|---|---|---|
| S1 | First message must be `status` within 30 s, else DeadlineExceeded / InvalidArgument. | server.rs:425-476 | Silent client is dropped after 30 s; non-status first message rejected |
| S2 | First status gives hostname, labels, label_sets, max_vms, active_vms, VM names, agent_version (empty -> nil); persisted only if changed. | server.rs:443-527 | — |
| S3 | `set_capacity` runs **before** the fleet notify so the first scheduling pass sees external VM usage (#33). | server.rs:550-551 | — |
| S4 | On connect: no VMs -> `notify`; VMs present -> `notify_with_recovery(vm_names)` (crash recovery, #4 f6daa15). | server.rs:555-568; fleet.rs:160 | Agent reconnecting with a VM keeps its runner row |
| S5 | Ping every 30 s (first immediately); Pong and Status refresh last_seen. Stale sweep every 60 s removes agents silent 120 s; "online" < 60 s. | server.rs:580-634, 838-842; main.rs:546-552; agent_registry.rs:1089-1108, 1229-1242 | Agent silent 120 s is unregistered |
| S6 | Status handling order: set_capacity, update_status, **settle_reservations** (drop reservations whose VM now shows, so a VM is never counted twice or zero times), reconcile runner state. | server.rs:701-730; agent_registry.rs:883-901 | Reservation released when the VM appears in status |
| S7 | Runner missing from status -> synthetic Destroyed "VM missing during reconciliation" (subject to the lost-VM grace, F9). Reservation GC: older than 60 s and not in DB. | server.rs:734-786 | — |
| S8 | Commands: `send_command` keyed by command_id, 30 s timeout from fleet; late/unknown replies dropped. DestroyRunner is fire-and-forget (answer arrives as a RunnerEvent). | agent_registry.rs:755-821; fleet.rs:710, 1411, 2556 | CreateRunner without Ack within 30 s records CommandFailed |
| S9 | **Disconnect failover grace 30 s:** on disconnect, wait 30 s; if the agent hasn't reconnected (`claim_failover` stamp), emit Destroyed "agent disconnected" for its runners with jobs. Reconnect clears the stamp. Race fixes in #12 (6713129, ENG-4829). | server.rs:54, 644-682; agent_registry.rs:519-597 | Reconnect within 30 s keeps runners; after 30 s they're failed over |
| S10 | **Rust bug to not copy:** `unregister` removes whatever is registered under agent_id, so an old stream's cleanup can evict a fresh reconnect. In Phoenix key the registry entry by connection pid and only remove your own. | agent_registry.rs:551-583 | Old stream exits after new one registered: agent stays connected |
| S11 | Known Rust gap: `send_command` awaits a 32-slot channel send under a lock before its timeout starts. A GenServer call with a timeout avoids this. | agent_registry.rs:768-780 | — |

### 1c. Agent registry, capacity, selection

| # | Behavior | Source | Phoenix test |
|---|---|---|---|
| R1 | available_capacity = min(max_vms - active - reserved, each applicable limit's max - external - active - reserved), saturating; 0 when unavailable. Separate tart VM limits and external usage (#33, d7ccf7d). | agent_registry.rs:382-396, 253-352 | Port the capacity unit tests in agent_registry.rs |
| R2 | Reservations keyed by runner name; reserve fails with no capacity; reserved-slot fixes in #33 (fb739ed). | agent_registry.rs:400-425, 1023-1049 | Two concurrent reserves on 1 free slot: one wins |
| R3 | Label matching: if label sets exist, job labels must be a case-insensitive subset of one set, else flat labels. `selected_set`: exact id, then a non-default set that adds a requested non-base label, then default. Multi-image fixed capacity, pool per map (#33, c8b62b5). | agent_registry.rs:128-132, 264-311, 433-443 | Port selected_set tests |
| R4 | Agent selection order: agents that already failed this job last (only if another fits), then most free slots, fewest in use, id asc (#30, aaedcf2). | agent_registry.rs:617-713 | Port `test_select_agent_avoids_failed_agent_when_possible` (agent_registry.rs:2240) |
| R5 | Runner kinds: GitHub always; other kinds (forgejo) only if agent lists them in runner_kinds (case-insensitive). CreateRunner `forgejo` field only sent to such agents. | agent_registry.rs:360-366, 622-630; agent.proto:215-218 | Forgejo job never goes to a tart agent without "forgejo" kind |
| R6 | Pool definitions: unavailable agents skipped; agents with explicit pool sizes get one pool per set ("labels@set_id"); others summed into one legacy pool per normalized label set. max_vms/labels always current from agents (#24, ec77e28). | agent_registry.rs:1117-1224 | — |
| R7 | Unavailable flag (#40) stored apart from the connection so it survives reconnects; unavailable agents keep labels for the dashboard but zero capacity. | agent_registry.rs:946-975, 519-524 | Mark unavailable, reconnect: still unavailable |

### 1d. Agent logs, failures, updates

| # | Behavior | Source | Phoenix test |
|---|---|---|---|
| L1 | UploadLogs: cert + validity check first; max 2000 records/batch (InvalidArgument); no log store -> accept and discard; DB error -> `Unavailable` so the agent requeues. | server.rs:375-398; agent_logs.rs:13 | 2001-record batch rejected |
| L2 | Dropped-lines marker row (WARN, ts = first record - 1 µs). Levels ERROR=1..TRACE=5, unknown -> INFO. Target capped 256 B, message 16 KiB + " …[truncated]". | agent_logs.rs:16-45, 97-139, 235-244 | Truncation at a UTF-8 boundary |
| L3 | Reads newest first, cursor `{ts_micros}_{seq}`, bad cursor -> first page. Prune hourly: 7 d and 100,000 lines per agent (#18, fix in #32 176390d). | agent_logs.rs:145-224; main.rs:525-528 | Port agent_logs.rs:342 cap test |
| F1 | Failure kinds runner_failed, runner_lost, command_rejected, command_failed (stored as text; unknown shows "Other"). Newest 100 per agent kept, pruned on insert. record() never errors. | agent_failures.rs:13-124 | 101st failure prunes the oldest |
| F2 | recent(agent), recent_all(kind?, agent?), counts_since (all-agents errors page, #74; unknown kinds counted, 33b1c0f; index on occurred_at, 0bc193b). | agent_failures.rs:130-179 | Unknown kind counted in totals |
| U1 | Agent update check: GitHub releases of AstroHQ/kuiper-forge, every 6 h, retry 30 min; **follow Link rel=next up to 50 pages, a failed page keeps the last good map** (#75, 7a3d524). Tag `kuiper-<type>-agent-v<semver>`, no drafts/prereleases, highest per crate; update only when strictly newer. | agent_updates.rs:17-256 | Port `test_failed_page_keeps_last_good_releases` (agent_updates.rs:394) |
## 2. Fleet, pools, runner lifecycle, pending jobs

### 2a. Fleet loop and fixed-capacity pools

| # | Behavior | Source | Phoenix test |
|---|---|---|---|
| P1 | Three modes dispatched in `FleetManager::run`: fixed_capacity (default), webhook, polling. | fleet.rs:380-420; config.rs:52-78 | — |
| P2 | Fixed capacity: reconcile every 30 s and on agent connect, recovery info and runner events. Order: capability warnings, drain unavailable agents, reconcile each pool. No `[runner_scope]` -> log error, create nothing. | fleet.rs:425-470, 2126-2152 | — |
| P3 | **Pool count from the DB (source of truth), not an in-memory counter** (#12, 6713129, ENG-4829 lockup). Explicit pools count that agent's runners with matching `pool`; legacy pools count runners whose normalized labels **exactly** equal the pool labels on available non-explicit agents (#12, f3e8dd4). | fleet.rs:2310-2346; runner_state.rs:203-206 | Pool with a superset-label runner is not counted as filled |
| P4 | Create `min(target - current, capacity)`; no agent -> stop; reserve failure -> next; registration-token failure releases the slot and stops. Registration token (not JIT), from the default (first) forge; `label_set_id` from the pool. | fleet.rs:2355-2533 | Token failure leaves no reservation |
| P5 | Fixed-capacity rejection releases the slot and removes the runner (no reprocess notify, unlike on-demand). `runner_group` is ignored in fixed capacity. | fleet.rs:2591-2602, 2341 | — |
| P6 | On-demand loop: each tick `process_pending_jobs`, `check_idle_runners`, and a forge scan when due. Tick = min(30 s, scan interval); scan piggybacks on the ticker (no starvation). Recovery scan in webhook mode every 300 s; polling uses `interval_secs` (default 120, min 10; warn under 60 with GitHub). | fleet.rs:31, 481-577 | — |
| P7 | Notifier channels: try_send (drop when full) except synthetic runner events, which wait. | fleet.rs:149-211, 329-336 | — |

### 2b. Runner lifecycle

| # | Behavior | Source | Phoenix test |
|---|---|---|---|
| F1 | Runner name `<short-host>-<8 hex>`; host = before first dot, non-alnum -> `-`, lowercase, max 20, trimmed; empty -> `runner` (#34, c67bfdd, ENG-4764). | fleet.rs:308-314, 2719-2742 | Port the naming tests |
| F2 | Job-bound create: reserve slot, then **re-check the job on the forge**; not `queued` / not found -> release slot, drop job as Gone. Check error -> proceed. Only when a repository is known. | fleet.rs:1226-1269 | Job cancelled between queue and create gets no runner |
| F3 | **Runner row written before CreateRunner is sent** so a failure can't leave a dangling forge runner (#24, 84c231d). | fleet.rs:1296-1309 | CreateRunner timeout removes the forge runner and the row |
| F4 | Ack rejected: CommandRejected failure, slot released, forge runner + row removed, reprocess; **not** a retry. Command error/timeout/unexpected reply: CommandFailed, cleanup, retry++ with the agent recorded. | fleet.rs:1377-1463, 1542-1620 | Rejection doesn't bump retry_count; timeout does |
| F5 | RunnerEvent: name = runner_name or vm_id; Started -> metrics; terminal (Completed/Failed/Destroyed) -> leave idle tracking, release slot, outcome mapping Completed > Drained > Failed > Lost > Destroyed. Lost = Destroyed, not drained, and job-bound or error text. Failed records RunnerFailed even if state is already gone. | fleet.rs:1710-1887 | — |
| F6 | Late events for an idle-torn-down runner are ignored (memory 600 s). | fleet.rs:40, 606-607, 1761-1768 | — |
| F7 | Job-bound Completed: `check_job_after_runner` keeps, re-provisions or removes as Ran. Failed/Destroyed: retry++ with agent, abandon at MAX_RETRIES = 3. Row removed, forge removal async, then reprocess. | fleet.rs:1893-2035; pending_jobs.rs:24 | 3 failures abandon the job |
| F8 | Forge removal always **by runner name**; DestroyRunner sends `vm_id = runner_name` / `vm_name`. The proxmox agent only destroys a VM while its VMID still belongs to that name (#38, 5417dec, issues #53/#54), so **Phoenix must send the VM name exactly as created**. Real Proxmox test still open: **#52**. | forge.rs:180-182; fleet.rs:705, 1788, 2108, 2234; kuiper-proxmox-agent/src/vm_manager.rs | DestroyRunner carries the created vm name; with a fake agent, a reused VMID under another name is not destroyed (agent-side, covered by #52 e2e) |
| F9 | **Lost-VM grace 20 s:** a runner whose VM is missing from status is lost only after 20 s missing; clock resets if the VM reappears (#38, cd27340). | runner_state.rs:67, 346-384 | Port `test_missing_vm_grace` (runner_state.rs:488) |
| F10 | Webhook runner cleanup deferred during reconciliation (#6, 74237aa). | runner_state.rs, server.rs | — |
| F11 | Reconnect recovery: listed VMs get a 30 s watcher; missing VMs -> Lost + cleanup (f6daa15). Rust quirk: watcher cleanup still runs after the agent disconnects (comment at fleet.rs:2074 says otherwise); decide in Phoenix. | fleet.rs:1631-1704, 2056-2119 | — |
| F12 | Orphan sweep every 60 s: runners on agents offline 300 s get Destroyed "agent offline". Stale agent sweep 60 s / 120 s. | main.rs:544-648 | Agent offline 300 s: its runners cleared |
| F13 | **Idle timeout (Forgejo one-job runners only):** default 600 s, 0 off, floor 120 s; measured from **runner creation**; destroy retry 120 s; per-job backoff doubles from the timeout up to 3600 s; job in backoff skipped; teardown not a retry. Status `in_progress` -> started; `queued`/404 -> tear down; `waiting`/other/error -> wait (#38). | config.rs:96-122; fleet.rs:34-70, 587-741 | Port `idle_action_from_job_status`, `idle_backoff_doubles_up_to_the_cap` (fleet.rs:2777, 2792), `test_idle_runner_timeout` (config.rs:1795) |
| F14 | **Open issue #49:** idle clock from creation + 120 s floor can kill booting runners. Phoenix should start the idle clock at registration/`Started` (keep the floor as a guard). | fleet.rs:620-623; config.rs:108 | Runner that takes 150 s to register under a 120 s timeout is not torn down |
| F15 | **Open issue #48:** idle check skips runners with no repository (org/@user/instance Forgejo scopes), so they never idle out. Phoenix should check them via the scope's job lookup. | fleet.rs:635-637 | Org-scoped runner that never gets a job is torn down after the timeout |
| F16 | **Drain unavailable agents** (#40, a1ab136): retire idle pool runners (never job-bound) via `remove_idle_runner`: Removed -> destroy, Busy -> keep, NotRegistered -> keep for 10 min boot grace (from created_at) then destroy; resend destroy after 300 s; clear draining map when no agent is unavailable. | fleet.rs:96-117, 2154-2254 | Port `drain_destroys_only_unregistered_runners` (fleet.rs:2876) |

### 2c. Pending jobs

| # | Behavior | Source | Phoenix test |
|---|---|---|---|
| J1 | Persisted in `pending_webhook_jobs`, PK job key, insert `ON CONFLICT DO NOTHING` (dedup). Webhook returns 503 if the write fails so the forge redelivers. **Never lose queued jobs when capacity is maxed** (#4, 381f5d5). | sql.rs:204-208; pending_jobs.rs:140-154; webhook.rs:514-521 | Duplicate queued delivery creates one job, one runner |
| J2 | FIFO by created_at; skip jobs that already have a runner (>1 logs corruption); skip jobs in idle backoff; abandon at retry_count >= 3. | fleet.rs:1067-1121; sql.rs:229 | — |
| J3 | "No available agent" -> try next job, no retry bump (string match in Rust; use a tagged error). Other errors bump retry (including reserve failure; Rust quirk). Stuck-queue fixes #27 (483f053), #31 (1963a34), #16 (211e86f). | fleet.rs:1148-1177 | Full fleet: jobs wait, retry_count stays 0, and run when capacity frees |
| J4 | `failed_agents` JSON array, deduped, used as an avoid-list for selection (#30, aaedcf2). | pending_jobs.rs:193-252 | Port `test_retry_records_failed_agents_once` (pending_jobs.rs:454) |
| J5 | Recovery: webhook mode verifies all pending jobs then runs the recovery scan; polling verifies only jobs the scan didn't see (#16 e76d217, #27). Verify: queued/waiting keep, other/404 remove as Gone, error keep. Jobs with no repo dropped after 24 h; jobs of an unconfigured forge dropped. A forge whose list fails is excluded from drop inference. | fleet.rs:779-916, 930-965, 2744-2763 | Scan error on one forge doesn't drop that forge's pending jobs |
| J6 | Intake (scan): required labels, skip if pending or has runner, **apply label mappings to recovered jobs** (#37, 8879858); job labels used as agent labels (exact job labels used for matching, #4 790923f). | fleet.rs:985-1034 | Recovered job gets the mapped scope/group |
| J7 | `runner_group` stored as string, parsed as u64, **defaults to group 1 on a name**: the config template shows a name, so names silently become the Default group. Phoenix: accept an id, or resolve names via the GitHub runner-groups API; don't silently fall back. | fleet.rs:1129-1133; config.rs:399-404, 1186 | Non-numeric runner_group is resolved or rejected at config load |
| J8 | Rust inconsistency: `waiting` is pending in verification, not-queued in create and in `check_job_after_runner`. Pick one rule in Phoenix and test it. | fleet.rs:239, 886, 1237 | — |

### 2d. Forge behaviour (trait)

| # | Behavior | Source |
|---|---|---|
| T1 | Methods: scope_url, runner_kind, get_registration_token, create_job_runner (GitHub JIT / Forgejo ephemeral), remove_runner (by name), remove_idle_runner (Removed/Busy/NotRegistered), list_queued_jobs, get_job_status (GitHub-style strings or nil). | forge.rs:83-198 |
| T2 | JobKey `forge:id` (id may contain colons; bare id = legacy `github`). Registry: non-empty, unique names; first forge is default; runners of a removed forge fall back to it. | forge.rs:23-67, 200-260; fleet.rs:287-292 |
| T3 | Dry-run MockForge (fake tokens, empty queue). | forge.rs:272-341 |
## 3. GitHub

| # | Behavior | Source | Phoenix test |
|---|---|---|---|
| G1 | App JWT RS256, iat now-60 s, exp now+9 min, iss app id; key from file or env PEM (`\n` unescaped, #4 8f8be0e). `validate()` at startup is fatal: JWT signs and at least one installation. | github.rs:251-348, 301-331; main.rs:378-389 | — |
| G2 | Installations: GET /app/installations (paged), keyed by lowercased login; cache miss re-discovers. | github.rs:353-405 | — |
| G3 | Installation token cached per installation, reused only if it expires more than 5 min out. | github.rs:95-98, 429-480 | Port the 4-min-expiry test (github.rs:1227) |
| G4 | Headers: Bearer, `Accept: application/vnd.github+json`, `X-GitHub-Api-Version: 2022-11-28`, UA kuiper-forge. | github.rs:70-72, 265 | — |
| G5 | **Pagination:** per_page 100, stop on a short page, a failed page fails the listing, cap 100 pages (warn, partial) (#37, 32b590d). | github.rs:27-59 | Port `collect_pages_*` tests (github.rs:1111-1161) |
| G6 | Registration token and JIT config for org and repo scopes only. JIT body `{name, runner_group_id, labels, work_folder:"_work"}`, JIT for webhook mode (#17, 767c543). Runner group passed through (#17, 1af0630). | github.rs:418-576; config.rs:937-961 | — |
| G7 | Runner lookup by `?name=` + exact match (one page); delete: 204 ok, **422 = busy** (Busy, not an error, for idle removal). | github.rs:578-735 | 422 on delete returns Busy |
| G8 | Job status: GET /repos/{o}/{r}/actions/jobs/{id}, 404 -> nil. | github.rs:741-790 | — |
| G9 | Queue discovery (polling + webhook recovery): per installation, list repos (pushed within lookback, default 168 h, 0 = off; missing pushed_at kept), 20 repos concurrently, runs with status queued/in_progress/waiting, their jobs, keep queued/waiting. Per-installation/repo failures are skipped. | github.rs:796-1032 | Lookback excludes stale repos |
| G10 | Scheduled/dispatch runs on quiet repos are missed by the lookback (documented limitation). | github.rs:887-890 | — |

## 4. Forgejo

| # | Behavior | Source | Phoenix test |
|---|---|---|---|
| FJ1 | `{base}/api/v1`, `Authorization: token <t>`, 30 s timeout. Token: token_env > token_path > token. Scopes: `org`, `owner/repo`, `@user`, `*` (instance). Needs Forgejo 15+ (jobs endpoint, handles); webhooks need 17+. | forgejo.rs:3-6, 95-124; config.rs:482-535 | — |
| FJ2 | One-job runners only: POST {scope}/actions/runners `{name, description, ephemeral:true}` -> {id, uuid, token}; registration tokens refused; fixed capacity with Forgejo rejected. | forgejo.rs:376-416; config.rs:622-630 | — |
| FJ3 | **Runner paging until an EMPTY page** (server caps page size, so short pages aren't the end), limit 50, cap 100 pages (#38, a9a1f17). | forgejo.rs:21-24, 328-355 | Port `lists_runners_past_short_pages`, `removes_runner_by_name_across_pages` (forgejo.rs:963, 995) |
| FJ4 | Delete every runner with the exact name; 404 ok. `remove_idle_runner` never removes (present -> Busy, absent -> NotRegistered). | forgejo.rs:419-472 | — |
| FJ5 | Polling: GET {scope}/actions/runners/jobs (unpaged), queue only `waiting`; empty handle skipped (needs 15+); **dedup across scopes by handle**, first scope wins; repo name via GET repositories/{id}, cached; a failing scope skipped, all failing = error. Status map waiting->queued, running->in_progress. | forgejo.rs:26-80, 259-300, 474-550 | Same job in org and instance scope makes one runner |
| FJ6 | get_job_status: list the job's scope and find by handle; absent = finished; list error = error. scope_for_repo order (org/user owner, instance, then repo) **differs from the webhook's order** (repo, org/user, instance). | forgejo.rs:305-325, 552-560; webhook.rs:270-288 | — |
| FJ7 | Startup validate (#38, 62df3e1): @user needs GET /user matching the name (write:user); non-repo scopes probe GET repositories/0 (401/403 fail); each scope's job list; DELETE runners/0 probe proves write access (404/400/2xx ok). Error hints for 401/403. | forgejo.rs:152-248 | Read-only token fails startup with the write-access hint |

## 5. Webhook vs polling

| # | Behavior | Source | Phoenix test |
|---|---|---|---|
| W1 | One POST route, default `/webhook`; source forge chosen by `[provisioning.webhook] forge` (named, else first GitHub, else first). Other Forgejo forges only found by the 5-min recovery scan. | webhook.rs:194-215, 364-369; config.rs:639-659; main.rs:409-422 | — |
| W2 | Responses: 200 for ignored events (checked **before** the signature), 401 bad signature, 400 bad JSON (after signature), 503 on DB write failure. | webhook.rs:427-571 | Each status code |
| W3 | GitHub: `X-GitHub-Event: workflow_job`, `X-Hub-Signature-256: sha256=<hex>` HMAC-SHA256 of raw body, constant-time. queued -> queue; in_progress -> job_started (+ runner name); completed -> job_completed, JobCompleted. Scope = organization.login else owner. | webhook.rs:233-253, 429-566 | Valid/invalid signature over the raw body (use `Plug.Parsers` body_reader to keep raw bytes) |
| W4 | Forgejo 17 (#81): `X-Forgejo-Event: workflow_job_*`, `X-Forgejo-Signature` plain hex HMAC. Branch on the event suffix (waiting/running/success/failure/cancelled/skipped), not on `action`. Empty handle ignored. Job key = handle. | webhook.rs:36-70, 586-693 | Replay `kuiper-forge/tests/fixtures` Forgejo 17 deliveries (469a764); port `tests/forgejo_webhook.rs` incl. `failure_and_cancelled_complete_the_job` |
| W5 | **Required labels default `["self-hosted"]`** (#4, e8b974a), all present, case-insensitive. Forgejo adds no implicit self-hosted. | webhook.rs:294-300, 469-475, 631-637; config.rs:302-304 | Job without self-hosted ignored |
| W6 | Label mappings: forge filter, all mapping labels present (case-insensitive), first match wins, scope falls back to default, empty mapping matches all; no match -> ignored. | webhook.rs:308-361 | Port mapping tests |
| W7 | Persist before notify; notify is try_send (drop OK, the tick picks it up). | webhook.rs:189-191, 514-527 | — |
| W8 | Polling: same label/mapping code; GitHub poll obeys lookback, webhook doesn't; failures retried next scan; metrics only from webhooks. | fleet.rs:985; github.rs:892-907 | — |
## 6. Admin UI (LiveView parity)

All under `/admin`, only when `[admin] enabled`; `/admin` redirects to `/admin/dashboard`. Unauthenticated -> 303 to login. Templates in `kuiper-forge/templates/admin/` (basecoat CSS, Chart.js; redesign #55).

| # | Page / action | Source | Phoenix test |
|---|---|---|---|
| UI1 | Login / logout: form login, Argon2id PHC hashes, session cookie `kuiper_admin_session` (Path=/admin, HttpOnly, SameSite=Strict), 64-char id, fixed expiry `session_timeout_secs` (3600), never extended. Logout deletes the row. | admin/routes.rs:36-41, 93-188; admin/auth.rs:55-99, 231-304 | Expired session redirects to login |
| UI2 | Dashboard: connected agents, active runners, pending jobs (unassigned first, then oldest), capacity (online non-revoked; "total" limit else max_vms), agents table (labels from connected agents a46b7fa, out-of-date version flag #75, unavailable toggle #40), registration tokens. | admin/routes.rs:196-336; admin/templates.rs:90-157 | — |
| UI3 | Create registration token (1 h) shows the `kfr1_` bundle: base64url(JSON {t, ca?, m, u}); delete token. | admin/routes.rs:339-420; main.rs:862-880 | Bundle decodes and registers a Rust agent |
| UI4 | Agent detail: registration, live limits, label split, runners, last 25 failures (#33 3cdd249), 7-day performance card (ok >= 95%, warn >= 80%). Revoke (404 unknown). Availability toggle: DB, then registry, then fleet notify; running runners finish. | admin/routes.rs:423-632; admin/metrics_routes.rs:100-151 | Toggle persists across restart |
| UI5 | Agent logs: level filter, `before` cursor, live tail, 500/page, 404 for unregistered agents, oldest-first display. | admin/agent_log_routes.rs:18-111 | — |
| UI6 | Metrics page: range 24h/7d/30d/90d (unknown -> 7d), header stats, charts (#67). | admin/metrics_routes.rs:28-98 | — |
| UI7 | Recent errors across agents (#74): kind/agent filters, 200 rows, 24 h counts by kind (unknown kinds counted, 33b1c0f), hostname falls back to id. | admin/error_routes.rs:22-125 | — |
| UI8 | Users: list, create (1-64 of `[A-Za-z0-9._-@]`, confirm), set password (kills that user's sessions, keeps your own), delete (not yourself, **last-admin check in a transaction** with a row lock, #32 176390d). | admin/user_routes.rs:26-257; admin/auth.rs:174-217 | Two concurrent deletes can't remove the last admin |
| UI9 | API tokens: `kfapi_` + 40 alnum, SHA-256 stored, 6-char prefix shown, shown once, delete; last_used_at on use. | admin/api_tokens.rs:12-117; admin/api_token_routes.rs | — |
| UI10 | Gaps in Rust not to copy: no CSRF (Phoenix gives it for free), no login rate limit, timing leak on unknown user (run a dummy hash), no `Secure` cookie, TOTP column unused. | admin/routes.rs:128-160; admin/auth.rs:23, 231-239 | Unknown and wrong-password logins take similar time |

### JSON API (`/api/v1`, Bearer API token)

| # | Route | Source |
|---|---|---|
| API1 | Bearer `kfapi_` only (no cookie); 401 JSON `{"error":…}` with WWW-Authenticate. | admin/api.rs:34-67 |
| API2 | GET /status: registered, connected, revoked, unavailable agents, total_capacity (sum of max_vms; differs from dashboard), active_runners, pending_jobs. | admin/api.rs:69-113 |
| API3 | GET /agents: per agent id, hostname, type, status (revoked > online > offline), unavailable(+since), labels, label_sets/limits/last_seen (connected only), max_vms, version, active_runners, created_at, cert_expires_at. | admin/api.rs:115-222 |
| API4 | GET /agents/{id}/logs: level, before, limit 1-1000 (default 500), newest first, next_before when full; unknown agent -> empty 200. | admin/api.rs:224-283 |

## 7. Management gRPC (`management.proto`, Unix socket)

| # | RPC | Source | Phoenix note |
|---|---|---|---|
| M1 | CreateToken(expires_secs, created_by) -> token, expires_at, created_at, server_ca_pem, server_trust_mode. No validation of 0 / huge TTL. | management.proto:12-38; management.rs:40-64 | Validate TTL > 0 |
| M2 | ListTokens returns plaintext tokens. | management.rs:66-85 | — |
| M3 | DeleteToken(token) -> deleted. | management.rs:87-100 | — |
| M4 | ListAgents -> id, hostname, type, labels, max_vms, created_at, expires_at, revoked, agent_version. | management.rs:102-126 | — |
| M5 | RevokeAgent(id) -> revoked (false for unknown, OK status). | management.rs:128-141 | — |
| M6 | Transport: Unix socket `<data_dir>/coordinator.sock`, no auth; unlinks an existing socket on bind; no chmod; start failure only logged. CLI `token`/`agent` commands use it. | management.rs:147-210, 295-297; main.rs:343-354, 849-946 | Phoenix runs beside Rust: **use a different socket path** or the two will steal each other's socket; chmod 0600 |

## 8. Metrics (run history, #67)

| # | Behavior | Source | Phoenix test |
|---|---|---|---|
| MX1 | Tables `runner_runs` (per runner attempt: requested/acked/started/job_started/finished, outcome, error, image, failed_phase, install_source, steps JSON) and `job_runs` (per job: queued/seen/first_runner/started/completed, conclusion, runner_name, attempts, outcome). Best-effort writes. | metrics/mod.rs:101-175; migrations/shared/20261029000000_add_run_history.sql, 20261031000000_add_run_install_source.sql | — |
| MX2 | Insert-once semantics: requested ON CONFLICT DO NOTHING; acked/started/job_started set only when NULL; finished counts only the first call; timings COALESCE + JSON merge, also after finish. | metrics/sql.rs:5-76 | — |
| MX3 | Runner outcomes completed, failed, lost, rejected, command_failed, idled, drained, destroyed. | metrics/mod.rs:18-51 | — |
| MX4 | queued_at = min(forge created, now); attempts++ and first_runner_at once per runner. | metrics/mod.rs:270-299 | — |
| MX5 | **Job outcome precedence:** a cancel replaces ran/gone/abandoned; a start after a cancel keeps the cancel; ran overrides any non-cancel; abandoned/gone only fill blanks (#67, 76b7e83). | metrics/sql.rs:127-175 | Port `test_cancel_replaces_a_started_job`, `test_late_start_keeps_the_cancel`, `test_cancelled_job` (metrics/mod.rs:657-704) and mod.rs:610 |
| MX6 | Summary math: success % = completed / (completed + failed + lost + command_failed); stopped = idled + drained + destroyed; nearest-rank percentiles; negative spans dropped; step stats, install source order, per agent/image groups, job wait/dispatch/provision/pickup breakdown. | metrics/summary.rs:79-427 | Port summary.rs unit tests (e.g. summary.rs:632) |
| MX7 | Retention `[metrics] retention_days` 90: runner_runs by requested_at, job_runs by seen_at; pruned hourly. | metrics/mod.rs:347-361; main.rs:523-539 | — |
## 9. Config, setup, CLI, DB

| # | Behavior | Source | Phoenix note |
|---|---|---|---|
| C1 | TOML file (`~/.config/kuiper-forge/config.toml`) + `KUIPER_` env with `__` nesting (env wins); arrays TOML only; `KUIPER_UPDATE_CHECK` flat. Missing file -> error pointing at `setup`. | config.rs:7-28, 851-860, 1003-1024, 1077-1091 | Phoenix should read the **same TOML** (via `config/runtime.exs`) so one config serves both, or document the mapping |
| C2 | Forges: legacy `[github]` xor `[[forges]]` (unique names, no `:`, github/forgejo); Forgejo url http(s), non-empty token, scopes. | config.rs:416-602 | Both / neither rejected |
| C3 | `[runner_scope]` required only in fixed_capacity (#38, 7e0bf9f), org or repo only; fixed_capacity + Forgejo rejected. | config.rs:610-630 | Port config tests |
| C4 | Provisioning: mode default fixed_capacity; idle_runner_timeout_secs 600 (0 off, floor 120); webhook path `/webhook`, secret required, required_labels `["self-hosted"]`, label_mappings, lookback 168, forge; polling interval 120 (min 10). Webhook mode with no section = warning only. | config.rs:52-304 | — |
| C5 | `[grpc]` listen 0.0.0.0:9443, proxy_protocol false. `[tls]` ca_cert, ca_key, server_cert, server_key required; server_ca_cert; server_use_native_roots; server_trust_mode ca/chain. | config.rs:680-765 | Phoenix needs its **own listen port** while Rust runs |
| C6 | `[database]` sqlite path or postgres (host, port, user, password, database kuiper_forge); migrations shared + dialect; PG job_id BIGINT then TEXT job keys `github:<id>`. | config.rs:772-825; db.rs; migrations/ | D3: Phoenix has its own Ecto DB; no data migration needed unless decided |
| C7 | `[admin]` enabled false, session_timeout_secs 3600, coordinator_url, update_check true. `[metrics]` retention_days 90. | config.rs:831-892 | — |
| C8 | `setup` wizard (#41): flags/--yes, config backup + 0600 write, hostname, forge, mode, webhook secret (64 hex), CA reuse (both files) or init, server cert regenerated when the identity check fails (f046479), **webhook steps from the webhook's forge** (#81, e94a543), admin user via KUIPER_ADMIN_PASSWORD. Printed webhook path hard-coded `/webhook`. | setup.rs:55-1142 | Phoenix: decide whether it needs a setup task or reuses the Rust one's output |
| C9 | CLI: serve (--dry-run, log file rotation), setup, ca init/server-cert/export, token create/list/revoke, agent list/revoke, init-config, admin create-user/list-users/reset-password (reset kills sessions). | main.rs:44-108, 790-1179 | Mix release commands for admin users at least |
| C10 | Startup order: DB, auth, trust, registry (restore unavailable flags from DB), stores, management socket, forges (validate, fatal), fleet, admin, webhook, background tasks. Shutdown only removes the socket; runners persist across restarts. | main.rs:290-788 | Restart with live runners resumes tracking |
| C11 | Background tasks: hourly log/run-history prune; stale agents 60 s/120 s; orphan sweep 60 s/300 s; hourly token + revoked purge (7 d); agent update checker 6 h. | main.rs:523-720; agent_updates.rs:19-20 | Each as a supervised GenServer/Oban job |
