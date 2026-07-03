# Repo Awareness over Gossip — Design

**Date**: 2026-07-03
**Status**: Approved

## Problem

Multiple agents (one per developer, each on their own clone) work on the same
git repository. They cannot see what the others are changing until a push/PR,
so they duplicate work and create merge conflicts. Buddies already gives
agents a shared room over gossip; this feature adds near-real-time awareness
of file changes in a shared repo.

## Goal

- Agents in a room see which files their buddies changed, in near-real-time.
- Agents can read the actual diff of a buddy's change.
- Agents are proactively warned when both sides touch the same file.
- Git remains the source of truth. Buddies never writes to the working tree.

## Non-goals (v1)

- Applying peers' diffs automatically (no collaborative editing).
- Catch-up sync for late joiners — you only see activity broadcast after you
  join the room.
- Per-edit intent messages via client hooks (layerable later as enrichment).
- Rename tracking — a rename appears as delete + create.
- Watcher persistence across restarts — watchers die with the process and
  must be re-registered via `watch_repo`.

## Architecture

### 1. Watcher (`src/watcher.rs`, new module)

- New MCP tool `watch_repo({ repo_path, room })` starts a filesystem watcher
  (`notify` crate) on the repo directory. `unwatch_repo({ repo_path })` stops
  it. `repo_path` must contain a `.git` directory.
- Events are debounced into ~1s batches.
- On each batch, buddies shells out to git (consistent with `identity.rs`
  usage): `git status --porcelain` to find changed paths and
  `git diff HEAD -- <path>` per changed path. Delegating to git means
  `.gitignore`d files, build artifacts, `.git/` churn, and binary detection
  are all handled by git — the watcher itself does no filtering beyond
  ignoring events under `.git/`.
- Repo identity (`repo` field) defaults to the repo directory name; the tool
  accepts an optional explicit name so a team can agree on one.
- The watcher tracks the local dirty set (paths with uncommitted changes) for
  conflict detection (see §5).
- A path whose diff did not change since the last broadcast (same content
  hash) is not re-broadcast.

### 2. Wire format (`src/protocol.rs`)

One new `P2PMessageBody` variant:

```rust
FileActivity {
    repo: String,        // room-agreed repo name
    branch: String,      // current branch at broadcast time
    path: String,        // repo-relative, forward slashes
    kind: FileChangeKind // Changed | Created | Deleted
    diff: String,        // unified diff vs HEAD; capped at 64 KiB with a
                         // truncation marker; for binary files git emits a
                         // "Binary files ... differ" stub instead of a diff
    content_hash: String,// SHA-256 of current file content ("" for Deleted)
    author: String,      // sender display name (BUDDIES_USER)
    timestamp: u64,
}
```

Messages go through the existing signing, replay-protection, and
whitelist-verification pipeline unchanged.

### 3. Storage (`src/storage.rs`)

- New redb table `file_activity`, key `repo:path:peer`, value = postcard
  `FileActivity`. Last-writer-wins per key (only the most recent diff per
  peer per file is kept).
- Entries older than 24 h are pruned lazily on write and skipped on read.

### 4. Query tools (`src/server.rs`)

- `check_file_activity({ repo, paths })` → for each path: peers that touched
  it, when, on which branch, and a diff summary (lines added/removed). Server
  instructions are extended: *"call `check_file_activity` before editing
  files in a watched repo."*
- `get_peer_diff({ repo, path, peer })` → the full stored diff.

### 5. Proactive conflict push

When a peer's `FileActivity` arrives for a path that is also in the local
dirty set of a watched repo, buddies emits a
`notifications/buddies/fileConflict` CustomNotification over the existing
channel (same plumbing as `taskArrived`), carrying both sides' metadata
(peer, branch, timestamps, diff summaries) and instructions to reconcile
before continuing.

## Error handling

- git command failures (not a repo, detached operations, etc.) are logged at
  `warn` and the batch is skipped; the watcher keeps running.
- Oversized diffs are truncated, never dropped — the metadata still flows.
- `watch_repo` on an already-watched path is idempotent (returns current
  state).

## Testing

- Pure logic unit-tested without gossip: porcelain-output parsing, diff
  truncation, dirty-set/conflict intersection, storage table round-trip and
  TTL pruning (same style as the existing `ReplayGuard` and merge-helper
  tests).
- Watcher debounce tested with a temp git repo fixture (init, commit, touch
  files) — no network needed.
- Gossip handler wiring covered the same way existing handlers are (thin,
  reviewed, not integration-tested in v1).

## Security / privacy note

This broadcasts source-code diffs to everyone in the room. The README must
state this clearly and recommend `require_signed=true` plus an identity
whitelist for rooms with watched repos.

## New dependencies

- `notify` (filesystem events). Everything else reuses git subprocesses and
  existing crates.
