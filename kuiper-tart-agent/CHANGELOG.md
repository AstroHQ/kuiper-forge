# Changelog
All notable changes to this project will be documented in this file. See [conventional commits](https://www.conventionalcommits.org/) for commit guidelines.

- - -
## kuiper-tart-agent-v0.4.0 - 2026-09-24
#### Features
- linux VMs supported in tart-agent - (d01cb4a) - *jfro*
- report agent version in UI - (177cce8) - *jfro*
- agent log upload, consistent browser timezone datestamps in UI - (24edf43) - *jfro*

#### Bug Fixes
- reserved slot handling issues with new limits - (fb739ed) - *jfro*
- various edge cases from codex review - (1b9911c) - *jfro*
- allow fixed capacity to work with multiple images, pool per map - (c8b62b5) - *jfro*
- separate VM limits for tart agent & reporting. external tart usage awareness for limits - (d7ccf7d) - *jfro*
- avoid leaking sensitive bits over log upload - (59617bc) - *jfro*

- - -

## kuiper-tart-agent-v0.3.1 - 2026-09-16
#### Bug Fixes
- security updates - (ae3b27c) - *jfro*

- - -

## kuiper-tart-agent-v0.3.0 - 2026-06-17
#### Bug Fixes
- avoid selecting first image when it should use default image if base labels match - (2eabee4) - *jfro*

#### Refactoring
- shared runtime for agents handling gRPC etc - (81772a3) - *jfro*
- label handling in shared agent lib - (0e257b9) - *jfro*

- - -

## kuiper-tart-agent-v0.2.2 - 2026-05-20
#### Bug Fixes
- ensure coordinator gets status updates on VM transitions - (90a780c) - *jfro*
- ensure max vms & labels is always current from agents - (ec77e28) - *jfro*
- cleanup old logs - fixes #18 - (28e8434) - *jfro*

- - -

## kuiper-tart-agent-v0.2.1 - 2026-03-12
#### Bug Fixes
- (**kuiper-agent-lib,kuiper-tart-agent**) runner script being accessed across crates - (3c3dc84) - *jfro*

- - -

## kuiper-tart-agent-v0.2.0 - 2026-03-12
#### Bug Fixes
- fix missing runner group passing along, and not destroying successful runs in debug - (1af0630) - *jfro*
- fix JIT runners for webhook mode, ensure win VMs are time sync'd - (767c543) - *jfro*

- - -

## kuiper-tart-agent-v0.1.1 - 2026-02-19
#### Bug Fixes
- (**kuiper-forge,kuiper-tart-agent,kuiper-proxmox-agent**) queued jobs getting stuck due to failed VM start & lack of requeue - (211e86f) - *jfro*

- - -
