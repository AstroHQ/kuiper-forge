# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## kuiper-libvirt-agent-v0.2.0 - 2026-10-30
#### Features
- **(metrics)** [**breaking**] record runner and job timings, add a metrics page - (0dc8ffc) - *jfro*

- - -
## kuiper-libvirt-agent-v0.1.0 - 2026-10-28
#### Features
- **(libvirt-agent)** generate domain XML from per-image hardware settings - (36ea7a0) - *jfro*
- **(libvirt-agent)** per-VM UEFI variable store and a Windows 11 domain template - (72976d9) - *jfro*
- **(libvirt-agent)** agent for ephemeral runner VMs on a local libvirt host - (f1dd9e5) - *jfro*

#### Bug Fixes
- apply logging.retention_days in the agents - (71b0d49) - *jfro*
- **(libvirt-agent)** never leave a VM running unclaimed - (7d0325e) - *jfro*
- **(agent-lib)** destroy VMs in parallel on shutdown and allow longer for it - (08c09b3) - *jfro*

- - -