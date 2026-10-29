# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.2](https://github.com/iltumio/buddies/compare/v0.3.1...v0.3.2) - 2026-10-29

### Added

- add Iggy transport and repository coordination

### Fixed

- *(deps)* resolve dependency audits and consolidate bot updates

## [0.3.1](https://github.com/iltumio/buddies/compare/v0.3.0...v0.3.1) - 2026-10-25

### Added

- *(orchestrator)* add machine config, multi-resource support, and autonomous service mode

### Fixed

- *(release)* leave 0.3.1 notes for the release PR

### Other

- Merge specialist workers and machine orchestration for v0.3.1
- release v0.1.11
- Abstract room communication transport with Iroh P2P and in-memory backends

### Fixed

- Restore machine orchestrator TOML/JSON configuration and autonomous service mode alongside specialist groups, resource connectors, and the message monitor.
- Create missing machine-service worktrees even when the user checkout has local changes, preserving existing effects and reporting full process startup errors.


## [0.1.11](https://github.com/iltumio/buddies/compare/v0.1.10...v0.1.11) - 2026-10-21

### Added

- *(orchestrator)* add machine config, multi-resource support, and autonomous service mode

### Other

- Abstract room communication transport with Iroh P2P and in-memory backends
## [0.3.0](https://github.com/iltumio/buddies/compare/v0.1.10...v0.3.0) - 2026-10-20

### Added

- *(workers)* load specialist groups from TOML with shared defaults, configuration validation and supervised tmux terminals for Codex and Claude
- *(workers)* coordinate persistent resource specialists across repositories, with optional orchestration, ownership scopes and acceptance checks
- *(linear)* reuse Claude Code's existing Linear MCP or CLI access with team declarations and exact session resume
- *(monitor)* inspect retained room messages, receipts and decoded orchestrator events

### Fixed

- *(workers)* validate coordinator ownership and assignment revisions, preserve read-only policies, and restrict host checks to user-configured commands
- *(connectors)* lock persistent workspaces, track resource-specific Git heads, preflight publication conflicts and retain recovery state
- *(monitor)* reject incompatible server snapshots and preserve completed worker events

## [0.2.3](https://github.com/iltumio/buddies/compare/v0.2.2...v0.2.3) - 2026-10-16

### Added

- *(worker)* launch persistent Claude Code Linear workers with existing MCP/CLI access, team declarations and exact session resume
- *(channel)* expose live Linear resources and instructions in existing Claude sessions without replacing their tool configuration

## [0.2.2](https://github.com/iltumio/buddies/compare/v0.2.1...v0.2.2) - 2026-10-16

### Fixed

- *(monitor)* detect incompatible server snapshots and display completed worker events reliably
- *(worker)* validate coordinator ownership, assignment revisions, resource boundaries, and configured checks
- *(orchestrator)* isolate resource heads and lock persistent workspaces before resuming
- *(connectors)* validate prepared snapshots, preflight publication conflicts, and preserve recovery state

## [0.2.1](https://github.com/iltumio/buddies/compare/v0.2.0...v0.2.1) - 2026-10-16

### Added

- *(monitor)* display live room messages, decoded wire events, and full message detail inspector


## [0.2.0](https://github.com/iltumio/buddies/compare/v0.1.10...v0.2.0) - 2026-10-16

### Added

- *(worker)* support single-specialist workers (`--resource`, `--allow-scope`, `--check`) and optional `--orchestrator` role with strict boundary enforcement and both orchestrated and peer-to-peer coordination
- *(orchestrator)* add multi-resource and stateful non-Git connector engine (`git`, `linear`, `gdrive`, `fs`)


## [0.1.10](https://github.com/iltumio/buddies/compare/v0.1.9...v0.1.10) - 2026-10-11

### Added

- *(orchestrator)* coordinate persistent room assignments
- *(collaboration)* add durable conversations and client adapters
- execute durable delegated tasks with automatic Codex workers

### Other

- publish VitePress site with branding and agent exports

## [0.1.9](https://github.com/iltumio/buddies/compare/v0.1.8...v0.1.9) - 2026-09-27

### Fixed

- persist process-owned room tickets and transport identity

## [0.1.8](https://github.com/iltumio/buddies/compare/v0.1.7...v0.1.8) - 2026-09-27

### Added

- route collaboration between local MCP participants

### Fixed

- advertise buddies package version in MCP initialization

## [0.1.7](https://github.com/iltumio/buddies/compare/v0.1.6...v0.1.7) - 2026-09-27

### Fixed

- recover disconnected rooms and expire stale presence and sessions

## [0.1.6](https://github.com/iltumio/buddies/compare/v0.1.5...v0.1.6) - 2026-09-27

### Added

- add Ratatui network monitor for shared buddies service

## [0.1.5](https://github.com/iltumio/buddies/compare/v0.1.4...v0.1.5) - 2026-09-27

### Added

- add watch_repo, check_file_activity, get_peer_diff tools and conflict notifications
- add repo watcher manager and conflict detection wiring
- scan watched repos for uncommitted changes via git
- parse git porcelain -z output into file change kinds
- add FileActivity wire message and TTL-pruned activity storage
- add DirtySet for local uncommitted-path tracking
- add file-activity entities and diff helpers
- add replay protection for signed P2P messages

### Fixed

- secure peer messaging and bound asynchronous work
- harden repo awareness and update dependencies
- cap diff, branch, and content_hash in received file activity
- harden repo-awareness against transport limits and malicious peers
- bind GPG signature verification to the claimed key id
- harden validation of P2P input and signer configuration
- correct result handling in local and distributed search
- only run release job on tag-ref runs to avoid duplicate race

### Other

- automate GitHub releases with release-plz
- add repo-awareness diagram and development section to README
- document repo awareness tools and privacy model
- add repo-awareness implementation plan
- add repo-awareness design spec
- run fmt, clippy, and tests on PRs and pushes to main
- apply rustfmt across the codebase
- relicense from AGPL-3.0 to MIT
