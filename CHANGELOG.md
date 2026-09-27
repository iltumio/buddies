# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
