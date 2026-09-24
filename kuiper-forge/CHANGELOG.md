# Changelog
All notable changes to this project will be documented in this file. See [conventional commits](https://www.conventionalcommits.org/) for commit guidelines.

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
