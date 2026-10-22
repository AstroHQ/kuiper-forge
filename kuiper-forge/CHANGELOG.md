# Changelog
All notable changes to this project will be documented in this file. See [conventional commits](https://www.conventionalcommits.org/) for commit guidelines.

- - -
## kuiper-forge-v0.7.5 - 2026-10-22
#### Bug Fixes
- **(tls)** rustls-webpki 0.103.16, fresh ca-certificates in the image - (519d2a6) - *jfro*

- - -
## kuiper-forge-v0.7.4 - 2026-10-21
#### Features
- Forgejo user (@name) and instance (*) runner scopes - (7e00791) - *jfro*
- Forgejo jobs on tart (macOS) agents and proxmox Windows VMs - (a180d8c) - *jfro*
- destroy Forgejo runners whose job never starts - (89f177c) - *jfro*
- Forgejo backend with one-job runners on proxmox Linux VMs - (5168175) - *jfro*

#### Bug Fixes
- **(auth)** read max_vms as i32 so listing agents works on Postgres - (5f8acd2) - *jfro*
- **(forgejo)** page runners until an empty page - (a9a1f17) - *jfro*
- [runner_scope] only required in fixed-capacity mode - (7e0bf9f) - *jfro*
- Forgejo startup check needs runner write access; e2e results - (62df3e1) - *jfro*
- grace period before a runner with a missing VM counts as lost - (cd27340) - *jfro*

#### Refactoring
- redesign admin web UI with basecoat, tweak some of the UI to - (def7b9e) - *jfro*

- - -
## kuiper-forge-v0.7.3 - 2026-10-20
#### Features
- drain idle pool runners from unavailable agents - (a1ab136) - *jfro*
- mark agents unavailable from the dashboard for maintenance - (8878a38) - *jfro*
- multi-forge groundwork (forge trait, job keys, [[forges]] config) - (7b415f8) - *jfro*
- polling provisioning mode for GitHub - (69e5645) - *jfro*

#### Bug Fixes
- paginate GitHub queue discovery - (32b590d) - *jfro*
- apply label mappings to jobs recovered in webhook mode - (8879858) - *jfro*

- - -
## kuiper-forge-v0.7.2 - 2026-09-25
#### Bug Fixes
- cleaner label display in dashboard - (fa20bfd) - *jfro*
- name runners after their hosts - fixes ENG-4764 - (c67bfdd) - *jfro*

- - -

## kuiper-forge-v0.7.1 - 2026-09-24
#### Bug Fixes
- make PROXY protocol optional so in-cluster connections aren't an issue - (55ed2ff) - *jfro*

- - -

## kuiper-forge-v0.7.0 - 2026-09-24
#### Features
- linux VMs supported in tart-agent - (d01cb4a) - *jfro*
- report agent version in UI - (177cce8) - *jfro*
- agent log upload, consistent browser timezone datestamps in UI - (24edf43) - *jfro*
- agent failures info on agent details page - (3cdd249) - *jfro*
- cleaner dashboard with queued jobs status - (b9da897) - *jfro*
- API & API tokens for querying status etc. - (56f65e1) - *jfro*
- user management web UI - (a4bcfea) - *jfro*

#### Bug Fixes
- reserved slot handling issues with new limits - (fb739ed) - *jfro*
- various edge cases from codex review - (1b9911c) - *jfro*
- allow fixed capacity to work with multiple images, pool per map - (c8b62b5) - *jfro*
- separate VM limits for tart agent & reporting. external tart usage awareness for limits - (d7ccf7d) - *jfro*
- edge case with admin deletion & log prune issue - (176390d) - *jfro*

- - -

## kuiper-forge-v0.6.1 - 2026-09-16
#### Bug Fixes
- better handling of job queue check - (2e8a91d) - *jfro*
- require rust 1.94 for docker/etc now - (6c79581) - *jfro*
- security updates - (ae3b27c) - *jfro*
- jobs not getting handled due to agent assignment assumptions - (1963a34) - *jfro*

- - -

## kuiper-forge-v0.6.0 - 2026-09-14
#### Bug Fixes
- more edge cases - (d8a39ad) - *jfro*
- edge case around timers - (c381316) - *jfro*
- avoid poor agent selection specially on repeated failure - (aaedcf2) - *jfro*

- - -

## kuiper-forge-v0.5.0 - 2026-06-17
#### Bug Fixes
- prevent stuck queued jobs - (483f053) - *jfro*
- track when agents are revoked & clean them ever 7d - (1909a4f) - *jfro*

- - -

## kuiper-forge-v0.4.2 - 2026-05-20
#### Bug Fixes
- avoid clobbering revoked flag - (f6a2b42) - *jfro*
- ensure max vms & labels is always current from agents - (ec77e28) - *jfro*

- - -

## kuiper-forge-v0.4.1 - 2026-05-12
#### Bug Fixes
- add better logging around possible runner conflict - (549d20c) - *jfro*
- avoid gap in runner creation failure causing dangling gh runners - (84c231d) - *jfro*
- show labels from connected agents - (a46b7fa) - *jfro*

- - -

## kuiper-forge-v0.4.0 - 2026-03-12
#### Bug Fixes
- fix missing runner group passing along, and not destroying successful runs in debug - (1af0630) - *jfro*
- fix JIT runners for webhook mode, ensure win VMs are time sync'd - (767c543) - *jfro*

- - -

## kuiper-forge-v0.3.2 - 2026-02-19
#### Bug Fixes
- (**kuiper-forge,kuiper-tart-agent,kuiper-proxmox-agent**) queued jobs getting stuck due to failed VM start & lack of requeue - (211e86f) - *jfro*

- - -

## kuiper-forge-v0.3.1 - 2026-02-17
#### Bug Fixes
- (**kuiper-forge**) runner state mismatch with agent capacity. hopefully - (392784e) - *jfro*

- - -
