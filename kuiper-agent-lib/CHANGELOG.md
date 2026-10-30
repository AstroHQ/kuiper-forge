# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## kuiper-agent-lib-v0.6.0 - 2026-10-30
#### Features
- **(metrics)** [**breaking**] record runner and job timings, add a metrics page - (0dc8ffc) - *jfro*

- - -
## kuiper-agent-lib-v0.5.2 - 2026-10-28
#### Bug Fixes
- **(agent-lib)** destroy VMs in parallel on shutdown and allow longer for it - (08c09b3) - *jfro*
- **(agent-lib)** wait for in-flight creates and destroys before a destroy returns - (4943a5a) - *jfro*

#### Refactoring
- **(agent-lib)** generic VmAgent over a VmBackend trait - (9b9b760) - *jfro*
- **(agent-lib)** share the guest-side runner lifecycle - (38b0c25) - *jfro*
- **(agent-lib)** share the SSH transport and runner commands - (5bc5db9) - *jfro*

- - -
## kuiper-agent-lib-v0.5.1 - 2026-10-21
#### Features
- Forgejo jobs on tart (macOS) agents and proxmox Windows VMs - (a180d8c) - *jfro*
- publish forgejo-runner builds for every platform on kuiper-forge - (02b6c9d) - *jfro*
- Forgejo backend with one-job runners on proxmox Linux VMs - (5168175) - *jfro*

- - -