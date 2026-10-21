# Changelog
All notable changes to this project will be documented in this file. See [conventional commits](https://www.conventionalcommits.org/) for commit guidelines.

- - -
## kuiper-proxmox-agent-v0.4.2 - 2026-10-21
#### Features
- Forgejo jobs on tart (macOS) agents and proxmox Windows VMs - (a180d8c) - *jfro*
- publish forgejo-runner builds for every platform on kuiper-forge - (02b6c9d) - *jfro*
- destroy Forgejo runners whose job never starts - (89f177c) - *jfro*
- Forgejo backend with one-job runners on proxmox Linux VMs - (5168175) - *jfro*

#### Bug Fixes
- **(agents)** keep runner tokens out of SSH debug logs - (b159eaf) - *jfro*
- **(proxmox)** SSH keepalive, and stop waiting on a runner whose VM is torn down - (8096b49) - *jfro*
- **(proxmox)** only destroy a VM while its VMID still belongs to it - (5417dec) - *jfro*

- - -
## kuiper-proxmox-agent-v0.4.2 - 2026-10-21
#### Features
- Forgejo jobs on tart (macOS) agents and proxmox Windows VMs - (a180d8c) - *jfro*
- publish forgejo-runner builds for every platform on kuiper-forge - (02b6c9d) - *jfro*
- destroy Forgejo runners whose job never starts - (89f177c) - *jfro*
- Forgejo backend with one-job runners on proxmox Linux VMs - (5168175) - *jfro*

#### Bug Fixes
- **(agents)** keep runner tokens out of SSH debug logs - (b159eaf) - *jfro*
- **(proxmox)** SSH keepalive, and stop waiting on a runner whose VM is torn down - (8096b49) - *jfro*
- **(proxmox)** only destroy a VM while its VMID still belongs to it - (5417dec) - *jfro*

- - -
## kuiper-proxmox-agent-v0.4.1 - 2026-10-20
#### Bug Fixes
- security update to russh - (7249398) - *jfro*

- - -
## kuiper-proxmox-agent-v0.4.0 - 2026-09-24
#### Features
- report agent version in UI - (177cce8) - *jfro*
- agent log upload, consistent browser timezone datestamps in UI - (24edf43) - *jfro*

#### Bug Fixes
- various edge cases from codex review - (1b9911c) - *jfro*
- allow fixed capacity to work with multiple images, pool per map - (c8b62b5) - *jfro*
- separate VM limits for tart agent & reporting. external tart usage awareness for limits - (d7ccf7d) - *jfro*
- avoid leaking sensitive bits over log upload - (59617bc) - *jfro*

- - -

## kuiper-proxmox-agent-v0.3.1 - 2026-09-16
#### Bug Fixes
- security updates - (ae3b27c) - *jfro*

- - -

## kuiper-proxmox-agent-v0.3.0 - 2026-06-17
#### Bug Fixes
- avoid selecting first image when it should use default image if base labels match - (2eabee4) - *jfro*
- (**proxmox-agent**) ensure systemd paths are escaped if needed - (65b20ed) - *jfro*
- lack of linux packaging for proxmox agent - (8a33a60) - *jfro*
- kuiper-proxmox-agent now supports system level installs/config & packaging - (f43078a) - *jfro*

#### Refactoring
- shared runtime for agents handling gRPC etc - (81772a3) - *jfro*
- label handling in shared agent lib - (0e257b9) - *jfro*

- - -

## kuiper-proxmox-agent-v0.2.2 - 2026-05-20
#### Bug Fixes
- ensure coordinator gets status updates on VM transitions - (90a780c) - *jfro*
- ensure max vms & labels is always current from agents - (ec77e28) - *jfro*
- (**proxmox-api**) fix handling of warning as failure - (87308d2) - *jfro*
- cleanup old logs - fixes #18 - (28e8434) - *jfro*
- (**kuiper-agent-lib,kuiper-tart-agent**) runner script being accessed across crates - (3c3dc84) - *jfro*

- - -

## kuiper-proxmox-agent-v0.2.0 - 2026-03-12
#### Bug Fixes
- fix hang risk with tail on linux VMs - (05a4a25) - *jfro*
- fix missing runner group passing along, and not destroying successful runs in debug - (1af0630) - *jfro*
- fix JIT runners for webhook mode, ensure win VMs are time sync'd - (767c543) - *jfro*

- - -

## kuiper-proxmox-agent-v0.1.1 - 2026-02-19
#### Bug Fixes
- (**kuiper-forge,kuiper-tart-agent,kuiper-proxmox-agent**) queued jobs getting stuck due to failed VM start & lack of requeue - (211e86f) - *jfro*

- - -
