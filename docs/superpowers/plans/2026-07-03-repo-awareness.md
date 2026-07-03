# Repo Awareness Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Agents in a buddies room see which files their buddies changed in a shared git repo in near-real-time, can read the diffs, and get pushed a conflict notification when both sides touch the same file.

**Architecture:** A `notify`-based filesystem watcher (started via a new `watch_repo` MCP tool) debounces events and shells out to git (`status --porcelain -z`, `diff HEAD`) so git does all ignore/binary filtering. Each changed file becomes a `FileActivity` gossip message (signed + replay-protected by the existing pipeline). Receivers store the latest diff per (repo, path, peer) in redb with a 24h TTL, expose it via `check_file_activity` / `get_peer_diff` tools, and push a `notifications/buddies/fileConflict` CustomNotification when a peer's activity hits a locally-dirty path.

**Tech Stack:** Rust edition 2024, tokio, iroh-gossip, redb, postcard, rmcp, notify (new dep), sha2, git subprocesses.

## Global Constraints

- Spec: `docs/superpowers/specs/2026-07-03-repo-awareness-design.md`.
- Only new dependency allowed: `notify = "8"` (fall back to `"7"` if resolution fails; note it in the commit).
- All new gossip traffic goes through the existing `P2PMessage` sign/replay/whitelist pipeline — do not add a parallel path.
- Buddies never writes into the watched working tree.
- Diff payload cap: 64 KiB with truncation marker. Activity TTL: 24 hours.
- Every commit must pass: `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test`.
- Commit messages end with `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`.
- TDD: write the failing test, watch it fail, implement, watch it pass, commit.

---

### Task 1: Activity entities and diff helpers (`src/activity.rs`)

**Files:**
- Create: `src/activity.rs`
- Modify: `src/main.rs` (add `mod activity;` as the first entry of the module list, before `mod identity;`)
- Test: inline `#[cfg(test)] mod tests` in `src/activity.rs`

**Interfaces:**
- Consumes: nothing.
- Produces: `pub struct FileActivityEntry { pub repo: String, pub branch: String, pub path: String, pub kind: FileChangeKind, pub diff: String, pub content_hash: String, pub author: String, pub timestamp: u64 }`; `pub enum FileChangeKind { Changed, Created, Deleted }` (Display: `"changed"`/`"created"`/`"deleted"`); `pub fn diff_summary(diff: &str) -> (u64, u64)`; `pub fn truncate_diff(diff: String) -> String`; `pub const MAX_DIFF_BYTES: usize`.

- [ ] **Step 1: Create `src/activity.rs` with the tests only** (types referenced don't exist yet — this is the failing state):

```rust
use serde::{Deserialize, Serialize};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_summary_counts_added_and_removed_lines() {
        let diff = "--- a/f.rs\n+++ b/f.rs\n@@ -1,2 +1,2 @@\n-old line\n+new line\n+another\n context\n";
        assert_eq!(diff_summary(diff), (2, 1));
    }

    #[test]
    fn truncate_diff_caps_at_limit_on_char_boundary() {
        let short = "small diff".to_string();
        assert_eq!(truncate_diff(short.clone()), short);

        // multi-byte char straddling the cap must not panic
        let long = "é".repeat(MAX_DIFF_BYTES); // 2 bytes each => 128 KiB
        let out = truncate_diff(long);
        assert!(out.len() <= MAX_DIFF_BYTES + DIFF_TRUNCATION_MARKER.len());
        assert!(out.ends_with(DIFF_TRUNCATION_MARKER));
    }

    #[test]
    fn file_change_kind_display() {
        assert_eq!(FileChangeKind::Changed.to_string(), "changed");
        assert_eq!(FileChangeKind::Created.to_string(), "created");
        assert_eq!(FileChangeKind::Deleted.to_string(), "deleted");
    }
}
```

- [ ] **Step 2: Add `mod activity;` to `src/main.rs`** (first line of the module block, before `mod identity;`), then run `cargo test --bin buddies activity 2>&1 | tail -5`. Expected: FAIL to compile — `diff_summary`, `truncate_diff`, `MAX_DIFF_BYTES`, `FileChangeKind` not found.

- [ ] **Step 3: Implement above the tests module:**

```rust
pub const MAX_DIFF_BYTES: usize = 64 * 1024;
pub const DIFF_TRUNCATION_MARKER: &str = "\n[... diff truncated by buddies ...]";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileChangeKind {
    Changed,
    Created,
    Deleted,
}

impl std::fmt::Display for FileChangeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Changed => write!(f, "changed"),
            Self::Created => write!(f, "created"),
            Self::Deleted => write!(f, "deleted"),
        }
    }
}

/// One peer's latest change to one file in a watched repo.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileActivityEntry {
    pub repo: String,
    pub branch: String,
    /// Repo-relative path, forward slashes.
    pub path: String,
    pub kind: FileChangeKind,
    /// Unified diff vs HEAD, capped at MAX_DIFF_BYTES.
    pub diff: String,
    /// SHA-256 hex of current content, empty for Deleted.
    pub content_hash: String,
    pub author: String,
    pub timestamp: u64,
}

/// (added, removed) line counts of a unified diff.
pub fn diff_summary(diff: &str) -> (u64, u64) {
    let mut added = 0;
    let mut removed = 0;
    for line in diff.lines() {
        if line.starts_with("+++") || line.starts_with("---") {
            continue;
        }
        if line.starts_with('+') {
            added += 1;
        } else if line.starts_with('-') {
            removed += 1;
        }
    }
    (added, removed)
}

pub fn truncate_diff(mut diff: String) -> String {
    if diff.len() <= MAX_DIFF_BYTES {
        return diff;
    }
    let mut cut = MAX_DIFF_BYTES;
    while !diff.is_char_boundary(cut) {
        cut -= 1;
    }
    diff.truncate(cut);
    diff.push_str(DIFF_TRUNCATION_MARKER);
    diff
}
```

Add `use super::*;` is already in the tests. Import `DIFF_TRUNCATION_MARKER` is covered by the glob.

- [ ] **Step 4: Run `cargo test --bin buddies activity 2>&1 | tail -4`.** Expected: 3 passed. (`FileActivityEntry` will warn as dead code until Task 3 — silence with `#[allow(dead_code)]` on the struct ONLY if clippy fails, and remove that allow in Task 3.)

- [ ] **Step 5: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`, then commit:**

```bash
git add src/activity.rs src/main.rs
git commit -m "feat: add file-activity entities and diff helpers

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 2: DirtySet (`src/activity.rs`)

**Files:**
- Modify: `src/activity.rs`

**Interfaces:**
- Produces: `pub struct DirtySet` with `pub fn new() -> Self`, `pub fn set_repo_dirty(&self, repo: &str, paths: std::collections::HashSet<String>)`, `pub fn clear_repo(&self, repo: &str)`, `pub fn is_dirty(&self, repo: &str, path: &str) -> bool`. Interior mutability (`std::sync::RwLock`) — shared as `Arc<DirtySet>` between watcher (writer) and RoomManager (reader).

- [ ] **Step 1: Add failing tests to the tests module in `src/activity.rs`:**

```rust
    #[test]
    fn dirty_set_tracks_paths_per_repo() {
        let dirty = DirtySet::new();
        assert!(!dirty.is_dirty("repo-a", "src/a.rs"));

        dirty.set_repo_dirty("repo-a", ["src/a.rs".to_string()].into());
        assert!(dirty.is_dirty("repo-a", "src/a.rs"));
        assert!(!dirty.is_dirty("repo-a", "src/b.rs"));
        assert!(!dirty.is_dirty("repo-b", "src/a.rs"));

        // replace semantics: a new scan result overwrites the old set
        dirty.set_repo_dirty("repo-a", ["src/b.rs".to_string()].into());
        assert!(!dirty.is_dirty("repo-a", "src/a.rs"));
        assert!(dirty.is_dirty("repo-a", "src/b.rs"));

        dirty.clear_repo("repo-a");
        assert!(!dirty.is_dirty("repo-a", "src/b.rs"));
    }
```

- [ ] **Step 2: Run `cargo test --bin buddies dirty_set 2>&1 | tail -4`.** Expected: compile FAIL, `DirtySet` not found.

- [ ] **Step 3: Implement in `src/activity.rs`:**

```rust
use std::collections::{HashMap, HashSet};
use std::sync::RwLock;

/// Locally-modified paths per watched repo, shared between the watcher
/// (writer) and the gossip handler (reader, for conflict detection).
#[derive(Default)]
pub struct DirtySet {
    inner: RwLock<HashMap<String, HashSet<String>>>,
}

impl DirtySet {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_repo_dirty(&self, repo: &str, paths: HashSet<String>) {
        self.inner
            .write()
            .expect("dirty set lock poisoned")
            .insert(repo.to_string(), paths);
    }

    pub fn clear_repo(&self, repo: &str) {
        self.inner
            .write()
            .expect("dirty set lock poisoned")
            .remove(repo);
    }

    pub fn is_dirty(&self, repo: &str, path: &str) -> bool {
        self.inner
            .read()
            .expect("dirty set lock poisoned")
            .get(repo)
            .is_some_and(|paths| paths.contains(path))
    }
}
```

- [ ] **Step 4: Run `cargo test --bin buddies dirty_set 2>&1 | tail -4`.** Expected: 1 passed.

- [ ] **Step 5: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`, then commit:**

```bash
git add src/activity.rs
git commit -m "feat: add DirtySet for local uncommitted-path tracking

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 3: Protocol variant + storage table and queries

**Files:**
- Modify: `src/protocol.rs` (new `P2PMessageBody` variant)
- Modify: `src/storage.rs` (new table + 3 methods + open in both constructors)

**Interfaces:**
- Consumes: `FileActivityEntry` from Task 1.
- Produces: `P2PMessageBody::FileActivity { entry: FileActivityEntry }`; `Storage::store_file_activity(&self, entry: &FileActivityEntry, now: u64) -> Result<()>`; `Storage::get_file_activity(&self, repo: &str, paths: Option<&[String]>, now: u64) -> Result<Vec<FileActivityEntry>>`; `Storage::get_peer_file_activity(&self, repo: &str, path: &str, peer: &str, now: u64) -> Result<Option<FileActivityEntry>>`; `pub const FILE_ACTIVITY_TTL_SECS: u64 = 86_400;`. `now` is passed in so the functions are deterministic in tests; callers use the existing time helpers.

- [ ] **Step 1: Add failing test to `src/storage.rs` tests module** (also add `use crate::activity::{FileActivityEntry, FileChangeKind};` and `use super::FILE_ACTIVITY_TTL_SECS;` to the test imports):

```rust
    fn activity(repo: &str, path: &str, peer: &str, timestamp: u64) -> FileActivityEntry {
        FileActivityEntry {
            repo: repo.to_string(),
            branch: "main".to_string(),
            path: path.to_string(),
            kind: FileChangeKind::Changed,
            diff: "+line\n".to_string(),
            content_hash: "abc".to_string(),
            author: peer.to_string(),
            timestamp,
        }
    }

    #[test]
    fn file_activity_stores_latest_per_peer_and_prunes_stale() {
        let storage = test_storage();
        let now = 1_000_000;

        storage
            .store_file_activity(&activity("repo-a", "src/a.rs", "alice", now - 10), now)
            .expect("store alice v1");
        // same key overwrites (last writer wins)
        storage
            .store_file_activity(&activity("repo-a", "src/a.rs", "alice", now - 5), now)
            .expect("store alice v2");
        storage
            .store_file_activity(&activity("repo-a", "src/a.rs", "bob", now - 3), now)
            .expect("store bob");
        storage
            .store_file_activity(&activity("repo-a", "src/b.rs", "bob", now - 2), now)
            .expect("store bob other file");
        storage
            .store_file_activity(&activity("repo-b", "src/a.rs", "carol", now - 1), now)
            .expect("store other repo");

        let all = storage
            .get_file_activity("repo-a", None, now)
            .expect("query repo-a");
        assert_eq!(all.len(), 3);

        let filtered = storage
            .get_file_activity("repo-a", Some(&["src/a.rs".to_string()]), now)
            .expect("query one path");
        assert_eq!(filtered.len(), 2);

        let alice = storage
            .get_peer_file_activity("repo-a", "src/a.rs", "alice", now)
            .expect("query alice")
            .expect("alice entry exists");
        assert_eq!(alice.timestamp, now - 5); // overwritten, not duplicated

        assert!(
            storage
                .get_peer_file_activity("repo-a", "src/a.rs", "nobody", now)
                .expect("query missing")
                .is_none()
        );

        // stale entries are filtered on read and pruned on the next write
        let later = now + FILE_ACTIVITY_TTL_SECS + 100;
        assert!(
            storage
                .get_file_activity("repo-a", None, later)
                .expect("query later")
                .is_empty()
        );
        storage
            .store_file_activity(&activity("repo-a", "src/c.rs", "dave", later), later)
            .expect("store triggers prune");
        let remaining = storage
            .get_file_activity("repo-a", None, later)
            .expect("query after prune");
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].path, "src/c.rs");
    }
```

- [ ] **Step 2: Run `cargo test --bin buddies file_activity 2>&1 | tail -4`.** Expected: compile FAIL, methods not found.

- [ ] **Step 3: Implement in `src/storage.rs`.** Add to imports: `use crate::activity::FileActivityEntry;`. Add near the other table definitions and methods:

```rust
const FILE_ACTIVITY_TABLE: TableDefinition<&str, &[u8]> = TableDefinition::new("file_activity");

/// Peer file activity older than this is pruned/ignored.
pub const FILE_ACTIVITY_TTL_SECS: u64 = 86_400;

fn activity_key(repo: &str, path: &str, peer: &str) -> String {
    // NUL separators: valid in Rust strings, cannot appear in the fields.
    format!("{repo}\u{0}{path}\u{0}{peer}")
}
```

Open the table in BOTH `Storage::open` and `Storage::in_memory` (add `let _ = tx.open_table(FILE_ACTIVITY_TABLE)?;` next to the existing three). Add methods to `impl Storage`:

```rust
    pub fn store_file_activity(&self, entry: &FileActivityEntry, now: u64) -> Result<()> {
        let key = activity_key(&entry.repo, &entry.path, &entry.author);
        let value = postcard::to_allocvec(entry)?;
        let tx = self.db.begin_write()?;
        {
            let mut table = tx.open_table(FILE_ACTIVITY_TABLE)?;
            table.insert(key.as_str(), value.as_slice())?;

            // lazy TTL prune: collect stale keys, then remove
            let mut stale = Vec::new();
            for item in table.iter()? {
                let (k, v) = item?;
                let e: FileActivityEntry = postcard::from_bytes(v.value())?;
                if e.timestamp + FILE_ACTIVITY_TTL_SECS < now {
                    stale.push(k.value().to_string());
                }
            }
            for k in stale {
                table.remove(k.as_str())?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn get_file_activity(
        &self,
        repo: &str,
        paths: Option<&[String]>,
        now: u64,
    ) -> Result<Vec<FileActivityEntry>> {
        let prefix = format!("{repo}\u{0}");
        let tx = self.db.begin_read()?;
        let table = tx.open_table(FILE_ACTIVITY_TABLE)?;
        let mut results = Vec::new();
        for item in table.iter()? {
            let (k, v) = item?;
            if !k.value().starts_with(&prefix) {
                continue;
            }
            let entry: FileActivityEntry = postcard::from_bytes(v.value())?;
            if entry.timestamp + FILE_ACTIVITY_TTL_SECS < now {
                continue;
            }
            if let Some(paths) = paths
                && !paths.contains(&entry.path)
            {
                continue;
            }
            results.push(entry);
        }
        results.sort_by_key(|e| std::cmp::Reverse(e.timestamp));
        Ok(results)
    }

    pub fn get_peer_file_activity(
        &self,
        repo: &str,
        path: &str,
        peer: &str,
        now: u64,
    ) -> Result<Option<FileActivityEntry>> {
        let key = activity_key(repo, path, peer);
        let tx = self.db.begin_read()?;
        let table = tx.open_table(FILE_ACTIVITY_TABLE)?;
        match table.get(key.as_str())? {
            Some(value) => {
                let entry: FileActivityEntry = postcard::from_bytes(value.value())?;
                if entry.timestamp + FILE_ACTIVITY_TTL_SECS < now {
                    return Ok(None);
                }
                Ok(Some(entry))
            }
            None => Ok(None),
        }
    }
```

- [ ] **Step 4: Add the protocol variant.** In `src/protocol.rs` add `use crate::activity::FileActivityEntry;` and append to `P2PMessageBody`:

```rust
    FileActivity {
        entry: FileActivityEntry,
    },
```

Note: appending a variant at the END keeps existing variant indices stable in postcard, but peers still must run matching versions (spec note).

- [ ] **Step 5: Run `cargo test --bin buddies file_activity 2>&1 | tail -4`.** Expected: 1 passed. Then `cargo test 2>&1 | tail -3` — expect a compile error in `src/room.rs`: non-exhaustive match on `P2PMessageBody`. Add a placeholder arm to `handle_message` in `src/room.rs` (replaced in Task 6):

```rust
            P2PMessageBody::FileActivity { entry } => {
                debug!(repo = %entry.repo, path = %entry.path, "file activity received (handler wired in a later task)");
            }
```

- [ ] **Step 6: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test` (all green), then commit:**

```bash
git add src/storage.rs src/protocol.rs src/room.rs src/activity.rs
git commit -m "feat: add FileActivity wire message and TTL-pruned activity storage

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 4: git porcelain parsing (`src/watcher.rs`, pure part)

**Files:**
- Create: `src/watcher.rs`
- Modify: `src/main.rs` (add `mod watcher;` after `mod ticket;`)
- Modify: `Cargo.toml` (add `notify = "8"` under the P2P networking section)

**Interfaces:**
- Consumes: `FileChangeKind` from Task 1.
- Produces: `pub(crate) fn parse_porcelain_z(bytes: &[u8]) -> Vec<(FileChangeKind, String)>` — parses `git status --porcelain -z` output; renames become Deleted(old) + Created(new).

- [ ] **Step 1: Create `src/watcher.rs` with tests only:**

```rust
use crate::activity::FileChangeKind;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_porcelain_z_handles_all_change_kinds() {
        // -z format: "XY path\0", renames: "R  new\0old\0"
        let raw = b" M src/modified.rs\0?? new_untracked.txt\0 D gone.txt\0A  staged_new.rs\0R  renamed_new.rs\0renamed_old.rs\0";
        let parsed = parse_porcelain_z(raw);
        assert_eq!(
            parsed,
            vec![
                (FileChangeKind::Changed, "src/modified.rs".to_string()),
                (FileChangeKind::Created, "new_untracked.txt".to_string()),
                (FileChangeKind::Deleted, "gone.txt".to_string()),
                (FileChangeKind::Created, "staged_new.rs".to_string()),
                (FileChangeKind::Deleted, "renamed_old.rs".to_string()),
                (FileChangeKind::Created, "renamed_new.rs".to_string()),
            ]
        );
    }

    #[test]
    fn parse_porcelain_z_ignores_garbage() {
        assert!(parse_porcelain_z(b"").is_empty());
        assert!(parse_porcelain_z(b"X\0").is_empty()); // too short
    }
}
```

- [ ] **Step 2: Add `mod watcher;` to `src/main.rs` and `notify = "8"` to `Cargo.toml`, run `cargo test --bin buddies watcher 2>&1 | tail -4`.** Expected: compile FAIL, `parse_porcelain_z` not found. (If `notify = "8"` fails to resolve, use `notify = "7"`.)

- [ ] **Step 3: Implement in `src/watcher.rs`:**

```rust
/// Parse `git status --porcelain -z` output into change kinds and
/// repo-relative paths. In -z format entries are NUL-terminated
/// `XY <path>`, and rename/copy entries are followed by the original
/// path as an extra NUL-terminated token.
pub(crate) fn parse_porcelain_z(bytes: &[u8]) -> Vec<(FileChangeKind, String)> {
    let mut out = Vec::new();
    let mut tokens = bytes
        .split(|b| *b == 0)
        .filter(|t| !t.is_empty())
        .map(|t| String::from_utf8_lossy(t).into_owned());

    while let Some(entry) = tokens.next() {
        if entry.len() < 4 {
            continue;
        }
        let (status, path) = entry.split_at(3);
        let x = status.as_bytes()[0] as char;
        let y = status.as_bytes()[1] as char;
        let path = path.to_string();

        if x == 'R' || x == 'C' {
            if let Some(original) = tokens.next() {
                out.push((FileChangeKind::Deleted, original));
            }
            out.push((FileChangeKind::Created, path));
            continue;
        }

        let kind = if x == '?' || x == 'A' {
            FileChangeKind::Created
        } else if x == 'D' || y == 'D' {
            FileChangeKind::Deleted
        } else {
            FileChangeKind::Changed
        };
        out.push((kind, path));
    }
    out
}
```

- [ ] **Step 4: Run `cargo test --bin buddies watcher 2>&1 | tail -4`.** Expected: 2 passed.

- [ ] **Step 5: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test` (a dead-code warning on `parse_porcelain_z` may appear — add `#[allow(dead_code)]` and remove it in Task 5), then commit:**

```bash
git add src/watcher.rs src/main.rs Cargo.toml Cargo.lock
git commit -m "feat: parse git porcelain -z output into file change kinds

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 5: Repo scanning (`src/watcher.rs`, git subprocess part)

**Files:**
- Modify: `src/watcher.rs`
- Modify: `src/room.rs` (make `now_unix` visible: change `fn now_unix()` to `pub(crate) fn now_unix()`)

**Interfaces:**
- Consumes: `parse_porcelain_z` (Task 4), `FileActivityEntry`, `truncate_diff` (Task 1).
- Produces: `pub(crate) async fn collect_activity(repo_path: &Path, repo_name: &str, author: &str, last: &mut HashMap<String, String>) -> (Vec<FileActivityEntry>, HashSet<String>)` — returns (entries to broadcast, full current dirty path set). `last` maps path → `"{kind}:{content_hash}"` so unchanged files are not re-broadcast; entries for now-clean paths are dropped from `last`.

- [ ] **Step 1: Add failing test to `src/watcher.rs` tests module** (uses a real temp git repo; git is available locally and in CI):

```rust
    use std::collections::HashMap;
    use std::path::Path;

    async fn git_ok(repo: &Path, args: &[&str]) {
        let out = tokio::process::Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .output()
            .await
            .expect("run git");
        assert!(out.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
    }

    async fn fixture_repo() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("buddies-watch-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create fixture dir");
        git_ok(&dir, &["init", "-b", "main"]).await;
        git_ok(&dir, &["config", "user.email", "t@example.com"]).await;
        git_ok(&dir, &["config", "user.name", "tester"]).await;
        git_ok(&dir, &["config", "commit.gpgsign", "false"]).await;
        std::fs::write(dir.join("tracked.txt"), "original\n").expect("write tracked");
        git_ok(&dir, &["add", "."]).await;
        git_ok(&dir, &["commit", "-m", "init"]).await;
        dir
    }

    #[tokio::test]
    async fn collect_activity_reports_changes_once() {
        let repo = fixture_repo().await;
        let mut last = HashMap::new();

        // clean repo: nothing to report
        let (entries, dirty) = collect_activity(&repo, "fixture", "alice", &mut last).await;
        assert!(entries.is_empty());
        assert!(dirty.is_empty());

        std::fs::write(repo.join("tracked.txt"), "changed\n").expect("modify");
        std::fs::write(repo.join("brand_new.txt"), "hello\n").expect("create");

        let (entries, dirty) = collect_activity(&repo, "fixture", "alice", &mut last).await;
        assert_eq!(entries.len(), 2);
        assert_eq!(dirty.len(), 2);

        let changed = entries.iter().find(|e| e.path == "tracked.txt").expect("tracked entry");
        assert_eq!(changed.kind, FileChangeKind::Changed);
        assert_eq!(changed.repo, "fixture");
        assert_eq!(changed.branch, "main");
        assert_eq!(changed.author, "alice");
        assert!(changed.diff.contains("+changed"), "diff was: {}", changed.diff);
        assert!(!changed.content_hash.is_empty());

        let created = entries.iter().find(|e| e.path == "brand_new.txt").expect("created entry");
        assert_eq!(created.kind, FileChangeKind::Created);
        assert!(created.diff.contains("+hello"), "diff was: {}", created.diff);

        // second scan with no further edits: dedup, nothing re-broadcast
        let (entries, dirty) = collect_activity(&repo, "fixture", "alice", &mut last).await;
        assert!(entries.is_empty());
        assert_eq!(dirty.len(), 2);
    }
```

- [ ] **Step 2: Run `cargo test --bin buddies collect_activity 2>&1 | tail -4`.** Expected: compile FAIL, `collect_activity` not found.

- [ ] **Step 3: Implement in `src/watcher.rs`.** Replace the imports at the top with:

```rust
use std::collections::{HashMap, HashSet};
use std::path::Path;

use sha2::{Digest, Sha256};
use tracing::warn;

use crate::activity::{FileActivityEntry, FileChangeKind, truncate_diff};
use crate::room::now_unix;
```

(and in `src/room.rs`, change `fn now_unix()` to `pub(crate) fn now_unix()`). Then add:

```rust
async fn git(repo: &Path, args: &[&str]) -> anyhow::Result<std::process::Output> {
    Ok(tokio::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .await?)
}

fn hash_file(path: &Path) -> String {
    match std::fs::read(path) {
        Ok(bytes) => {
            let digest: [u8; 32] = Sha256::digest(&bytes).into();
            data_encoding::HEXLOWER.encode(&digest)
        }
        Err(_) => String::new(),
    }
}

async fn diff_for(repo: &Path, kind: FileChangeKind, path: &str) -> String {
    // Tracked files (modified/deleted/staged) diff against HEAD; untracked
    // files need --no-index against /dev/null (git exits 1 when files
    // differ there, so ignore the exit code and take stdout).
    if let Ok(out) = git(repo, &["diff", "HEAD", "--", path]).await
        && !out.stdout.is_empty()
    {
        return String::from_utf8_lossy(&out.stdout).into_owned();
    }
    if kind == FileChangeKind::Created
        && let Ok(out) = git(repo, &["diff", "--no-index", "--", "/dev/null", path]).await
    {
        return String::from_utf8_lossy(&out.stdout).into_owned();
    }
    String::new()
}

/// Scan a repo for uncommitted changes. Returns the entries that changed
/// since the previous scan (tracked via `last`: path -> "kind:hash") and
/// the full set of currently dirty paths.
pub(crate) async fn collect_activity(
    repo_path: &Path,
    repo_name: &str,
    author: &str,
    last: &mut HashMap<String, String>,
) -> (Vec<FileActivityEntry>, HashSet<String>) {
    let status = match git(repo_path, &["status", "--porcelain", "-z"]).await {
        Ok(out) if out.status.success() => out.stdout,
        Ok(out) => {
            warn!(repo = %repo_name, stderr = %String::from_utf8_lossy(&out.stderr), "git status failed");
            return (Vec::new(), HashSet::new());
        }
        Err(error) => {
            warn!(repo = %repo_name, %error, "failed to run git status");
            return (Vec::new(), HashSet::new());
        }
    };

    let changes = parse_porcelain_z(&status);
    let dirty: HashSet<String> = changes.iter().map(|(_, p)| p.clone()).collect();
    last.retain(|path, _| dirty.contains(path));

    let branch = match git(repo_path, &["rev-parse", "--abbrev-ref", "HEAD"]).await {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_string(),
        _ => "unknown".to_string(),
    };

    let now = now_unix();
    let mut entries = Vec::new();
    for (kind, path) in changes {
        let content_hash = if kind == FileChangeKind::Deleted {
            String::new()
        } else {
            hash_file(&repo_path.join(&path))
        };
        let marker = format!("{kind}:{content_hash}");
        if last.get(&path) == Some(&marker) {
            continue;
        }
        let diff = truncate_diff(diff_for(repo_path, kind, &path).await);
        last.insert(path.clone(), marker);
        entries.push(FileActivityEntry {
            repo: repo_name.to_string(),
            branch: branch.clone(),
            path,
            kind,
            diff,
            content_hash,
            author: author.to_string(),
            timestamp: now,
        });
    }
    (entries, dirty)
}
```

- [ ] **Step 4: Run `cargo test --bin buddies watcher 2>&1 | tail -4`.** Expected: 3 passed (2 parse + 1 collect).

- [ ] **Step 5: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`, then commit:**

```bash
git add src/watcher.rs src/room.rs
git commit -m "feat: scan watched repos for uncommitted changes via git

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 6: WatcherManager + RoomManager/node wiring

**Files:**
- Modify: `src/watcher.rs` (WatcherManager)
- Modify: `src/room.rs` (DirtySet field, conflict broadcast channel, real FileActivity handler)
- Modify: `src/node.rs` (create DirtySet + WatcherManager, pass through)

**Interfaces:**
- Consumes: `collect_activity` (Task 5), `DirtySet` (Task 2), `Storage::store_file_activity` (Task 3).
- Produces:
  - `WatcherManager::new(room_manager: Arc<RoomManager>, dirty: Arc<DirtySet>, author: String) -> Arc<Self>`
  - `WatcherManager::watch(&self, repo_path: &Path, room: &str, repo_name: Option<String>) -> anyhow::Result<String>` (returns resolved repo name; idempotent)
  - `WatcherManager::unwatch(&self, repo_path: &Path) -> anyhow::Result<bool>` (true if a watcher was removed)
  - `RoomManager::new` gains a 6th parameter `dirty: Arc<DirtySet>`
  - `RoomManager::subscribe_conflict_events(&self) -> tokio::sync::broadcast::Receiver<FileActivityEntry>`
  - `BuddiesNode` gains `pub watcher_manager: Arc<crate::watcher::WatcherManager>` and `pub fn subscribe_conflict_events(...)` mirroring `subscribe_task_events`.

There is no isolated unit test for this wiring (constructing a RoomManager requires a live iroh gossip instance); correctness is covered by compilation, the existing suite staying green, and the pure pieces already tested in Tasks 1–5. The tool-level behavior is exercised in Task 7's manual verification.

- [ ] **Step 1: RoomManager changes in `src/room.rs`.** Add imports: `use crate::activity::{DirtySet, FileActivityEntry};`. Add fields to `RoomManager`:

```rust
    dirty: Arc<DirtySet>,
    conflict_broadcast: tokio::sync::broadcast::Sender<FileActivityEntry>,
```

Extend `RoomManager::new` signature with `dirty: Arc<DirtySet>` (after `signer`) and initialize:

```rust
            dirty,
            conflict_broadcast: tokio::sync::broadcast::channel(64).0,
```

Add next to `subscribe_task_events`:

```rust
    /// Subscribe to conflict events: fired when a peer's file activity
    /// arrives for a path that is also locally modified in a watched repo.
    pub fn subscribe_conflict_events(
        &self,
    ) -> tokio::sync::broadcast::Receiver<FileActivityEntry> {
        self.conflict_broadcast.subscribe()
    }
```

Replace the Task 3 placeholder arm in `handle_message`:

```rust
            P2PMessageBody::FileActivity { entry } => {
                if let Err(e) = self.storage.store_file_activity(&entry, now_unix()) {
                    warn!(error = %e, "failed to store received file activity");
                }
                if self.dirty.is_dirty(&entry.repo, &entry.path) {
                    info!(repo = %entry.repo, path = %entry.path, peer = %entry.author, "conflicting file activity from peer");
                    let _ = self.conflict_broadcast.send(entry);
                }
            }
```

- [ ] **Step 2: WatcherManager in `src/watcher.rs`.** Add imports: `use std::path::PathBuf; use std::sync::Arc; use std::time::Duration; use anyhow::{Context, Result}; use notify::Watcher; use crate::protocol::{P2PMessage, P2PMessageBody}; use crate::room::RoomManager; use crate::activity::DirtySet; use tracing::debug;`. Then:

```rust
pub struct WatcherManager {
    room_manager: Arc<RoomManager>,
    dirty: Arc<DirtySet>,
    author: String,
    watchers: tokio::sync::Mutex<HashMap<PathBuf, WatchedRepo>>,
}

struct WatchedRepo {
    repo_name: String,
    _watcher: notify::RecommendedWatcher,
    scan_task: tokio::task::JoinHandle<()>,
}

impl WatcherManager {
    pub fn new(room_manager: Arc<RoomManager>, dirty: Arc<DirtySet>, author: String) -> Arc<Self> {
        Arc::new(Self {
            room_manager,
            dirty,
            author,
            watchers: tokio::sync::Mutex::new(HashMap::new()),
        })
    }

    /// Start watching a git repo, broadcasting file activity to `room`.
    /// Idempotent: watching an already-watched path returns its repo name.
    pub async fn watch(
        &self,
        repo_path: &Path,
        room: &str,
        repo_name: Option<String>,
    ) -> Result<String> {
        let repo_path = repo_path
            .canonicalize()
            .with_context(|| format!("cannot resolve repo path {}", repo_path.display()))?;
        if !repo_path.join(".git").exists() {
            anyhow::bail!("not a git repository: {}", repo_path.display());
        }

        let mut watchers = self.watchers.lock().await;
        if let Some(existing) = watchers.get(&repo_path) {
            return Ok(existing.repo_name.clone());
        }

        let repo_name = repo_name.unwrap_or_else(|| {
            repo_path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "repo".to_string())
        });

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(event) = res {
                // .git churn (index, lockfiles) must not trigger scans
                let outside_git = event
                    .paths
                    .iter()
                    .any(|p| !p.components().any(|c| c.as_os_str() == ".git"));
                if outside_git {
                    let _ = tx.send(());
                }
            }
        })?;
        watcher.watch(&repo_path, notify::RecursiveMode::Recursive)?;

        let room_manager = Arc::clone(&self.room_manager);
        let dirty = Arc::clone(&self.dirty);
        let author = self.author.clone();
        let scan_repo_path = repo_path.clone();
        let scan_repo_name = repo_name.clone();
        let scan_room = room.to_string();
        let scan_task = tokio::spawn(async move {
            let mut last: HashMap<String, String> = HashMap::new();
            // initial scan announces current WIP and seeds the dirty set
            scan_and_broadcast(
                &scan_repo_path, &scan_repo_name, &author, &scan_room,
                &room_manager, &dirty, &mut last,
            )
            .await;
            while rx.recv().await.is_some() {
                tokio::time::sleep(Duration::from_secs(1)).await; // debounce
                while rx.try_recv().is_ok() {}
                scan_and_broadcast(
                    &scan_repo_path, &scan_repo_name, &author, &scan_room,
                    &room_manager, &dirty, &mut last,
                )
                .await;
            }
        });

        watchers.insert(
            repo_path,
            WatchedRepo {
                repo_name: repo_name.clone(),
                _watcher: watcher,
                scan_task,
            },
        );
        Ok(repo_name)
    }

    pub async fn unwatch(&self, repo_path: &Path) -> Result<bool> {
        let repo_path = repo_path
            .canonicalize()
            .with_context(|| format!("cannot resolve repo path {}", repo_path.display()))?;
        let mut watchers = self.watchers.lock().await;
        match watchers.remove(&repo_path) {
            Some(watched) => {
                watched.scan_task.abort();
                self.dirty.clear_repo(&watched.repo_name);
                Ok(true)
            }
            None => Ok(false),
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn scan_and_broadcast(
    repo_path: &Path,
    repo_name: &str,
    author: &str,
    room: &str,
    room_manager: &Arc<RoomManager>,
    dirty: &Arc<DirtySet>,
    last: &mut HashMap<String, String>,
) {
    let (entries, dirty_paths) = collect_activity(repo_path, repo_name, author, last).await;
    dirty.set_repo_dirty(repo_name, dirty_paths);
    for entry in entries {
        let msg = P2PMessage::new(P2PMessageBody::FileActivity { entry });
        if let Err(e) = room_manager.broadcast_to_room(room, msg).await {
            debug!(error = %e, "failed to broadcast file activity");
        }
    }
}
```

- [ ] **Step 3: Wire `src/node.rs`.** Add imports `use crate::activity::DirtySet;` and `use crate::watcher::WatcherManager;`. In `BuddiesNode` add field `pub watcher_manager: Arc<WatcherManager>`. In `BuddiesNode::new`, before `RoomManager::new`: `let dirty = Arc::new(DirtySet::new());` and clone the user name first (`let author = config.user_name.clone();`). Pass `Arc::clone(&dirty)` as the new last argument to `RoomManager::new`, then:

```rust
        let watcher_manager = WatcherManager::new(Arc::clone(&room_manager), dirty, author);
```

Include `watcher_manager` in the returned struct. Also add next to `subscribe_task_events`:

```rust
    pub fn subscribe_conflict_events(
        &self,
    ) -> tokio::sync::broadcast::Receiver<crate::activity::FileActivityEntry> {
        self.room_manager.subscribe_conflict_events()
    }
```

- [ ] **Step 4: Fix the RoomManager test-support fallout if any** (room.rs unit tests only use associated functions, so no constructor changes needed there). Run `cargo test 2>&1 | tail -3`. Expected: all tests pass (count unchanged from Task 5).

- [ ] **Step 5: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test`, then commit:**

```bash
git add src/watcher.rs src/room.rs src/node.rs
git commit -m "feat: add repo watcher manager and conflict detection wiring

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 7: MCP tools + conflict notification (`src/server.rs`)

**Files:**
- Modify: `src/server.rs`

**Interfaces:**
- Consumes: `WatcherManager::watch/unwatch`, `Storage::get_file_activity/get_peer_file_activity`, `BuddiesNode::subscribe_conflict_events`, `diff_summary`.
- Produces: MCP tools `watch_repo`, `unwatch_repo`, `check_file_activity`, `get_peer_diff`; notification `notifications/buddies/fileConflict`.

- [ ] **Step 1: Add request structs** next to the existing ones:

```rust
#[derive(Debug, Deserialize, JsonSchema)]
pub struct WatchRepoRequest {
    #[schemars(description = "Absolute path to a local git repository to watch")]
    pub repo_path: String,
    #[schemars(description = "Room to broadcast file activity to")]
    pub room: String,
    #[schemars(description = "Repo name agreed with teammates (default: directory name)")]
    pub repo_name: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct UnwatchRepoRequest {
    pub repo_path: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CheckFileActivityRequest {
    #[schemars(description = "Repo name as agreed in watch_repo")]
    pub repo: String,
    #[schemars(description = "Repo-relative paths to check (default: all recent activity)")]
    pub paths: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetPeerDiffRequest {
    pub repo: String,
    #[schemars(description = "Repo-relative file path")]
    pub path: String,
    #[schemars(description = "Peer (author) name whose diff to fetch")]
    pub peer: String,
}
```

- [ ] **Step 2: Add tools inside the `#[tool_router] impl BuddiesServer`** (imports to add at the top: `use std::path::Path;` and `use crate::activity::diff_summary;`):

```rust
    #[tool(
        name = "watch_repo",
        description = "Watch a local git repository and broadcast every uncommitted file change (with diff) to peers in the room. Enables check_file_activity, get_peer_diff, and proactive conflict notifications. WARNING: this shares source-code diffs with everyone in the room."
    )]
    async fn watch_repo(&self, Parameters(req): Parameters<WatchRepoRequest>) -> Result<CallToolResult, McpError> {
        let repo = self
            .node
            .watcher_manager
            .watch(Path::new(&req.repo_path), &req.room, req.repo_name)
            .await
            .map_err(|e| err(e.to_string()))?;
        ok_json(&serde_json::json!({
            "watching": true,
            "repo": repo,
            "room": req.room,
        }))
    }

    #[tool(name = "unwatch_repo", description = "Stop watching a repository.")]
    async fn unwatch_repo(&self, Parameters(req): Parameters<UnwatchRepoRequest>) -> Result<CallToolResult, McpError> {
        let stopped = self
            .node
            .watcher_manager
            .unwatch(Path::new(&req.repo_path))
            .await
            .map_err(|e| err(e.to_string()))?;
        ok_json(&serde_json::json!({ "stopped": stopped }))
    }

    #[tool(
        name = "check_file_activity",
        description = "Check which peers recently changed files in a watched repo. Call this BEFORE editing files in a shared repo to avoid conflicts. Returns per-file: peer, branch, change kind, timestamp, and diff line counts."
    )]
    async fn check_file_activity(
        &self,
        Parameters(req): Parameters<CheckFileActivityRequest>,
    ) -> Result<CallToolResult, McpError> {
        let entries = self
            .node
            .storage
            .get_file_activity(&req.repo, req.paths.as_deref(), now_ts())
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        let activity: Vec<serde_json::Value> = entries
            .iter()
            .map(|e| {
                let (added, removed) = diff_summary(&e.diff);
                serde_json::json!({
                    "path": e.path,
                    "peer": e.author,
                    "kind": e.kind.to_string(),
                    "branch": e.branch,
                    "timestamp": e.timestamp,
                    "lines_added": added,
                    "lines_removed": removed,
                })
            })
            .collect();
        ok_json(&serde_json::json!({ "repo": req.repo, "activity": activity }))
    }

    #[tool(
        name = "get_peer_diff",
        description = "Get the full diff of a peer's latest change to a file in a watched repo."
    )]
    async fn get_peer_diff(&self, Parameters(req): Parameters<GetPeerDiffRequest>) -> Result<CallToolResult, McpError> {
        let entry = self
            .node
            .storage
            .get_peer_file_activity(&req.repo, &req.path, &req.peer, now_ts())
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;
        match entry {
            Some(e) => ok_json(&serde_json::json!({
                "repo": e.repo,
                "path": e.path,
                "peer": e.author,
                "branch": e.branch,
                "kind": e.kind.to_string(),
                "timestamp": e.timestamp,
                "diff": e.diff,
            })),
            None => Err(err(format!(
                "no recent activity from '{}' on '{}' in repo '{}'",
                req.peer, req.path, req.repo
            ))),
        }
    }
```

- [ ] **Step 3: Conflict notifications.** In `on_initialized`, after the existing task-event spawn, add a second spawn:

```rust
        let peer = context.peer.clone();
        let mut conflict_rx = self.node.subscribe_conflict_events();
        tokio::spawn(async move {
            loop {
                match conflict_rx.recv().await {
                    Ok(entry) => {
                        let (added, removed) = crate::activity::diff_summary(&entry.diff);
                        let payload = serde_json::json!({
                            "repo": entry.repo,
                            "path": entry.path,
                            "peer": entry.author,
                            "branch": entry.branch,
                            "kind": entry.kind.to_string(),
                            "timestamp": entry.timestamp,
                            "lines_added": added,
                            "lines_removed": removed,
                            "instructions": format!(
                                "Peer '{}' changed '{}' (branch '{}'), which you have also modified locally. \
                                 Call get_peer_diff to inspect their change and reconcile before continuing.",
                                entry.author, entry.path, entry.branch
                            ),
                        });
                        if let Err(e) = peer
                            .send_notification(ServerNotification::CustomNotification(
                                CustomNotification::new(
                                    "notifications/buddies/fileConflict",
                                    Some(payload),
                                ),
                            ))
                            .await
                        {
                            tracing::warn!(error = %e, "failed to send conflict notification");
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "conflict notification listener lagged");
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });
```

(Note: `let peer = context.peer.clone();` shadows — the existing task spawn already consumed its own clone; take both clones from `context` before the first spawn.)

- [ ] **Step 4: Extend server instructions** in `get_info` — append to the instructions string:

```text
 To collaborate on a shared git repo, call 'watch_repo' with the repo path and room; \
 then call 'check_file_activity' before editing files to see what peers changed, and \
 'get_peer_diff' to read a peer's change. When you receive a \
 'notifications/buddies/fileConflict' notification, inspect the peer's diff and \
 reconcile with your local changes before continuing to edit that file.
```

- [ ] **Step 5: Run `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test 2>&1 | tail -3`.** Expected: all green.

- [ ] **Step 6: Manual end-to-end smoke test** (single node — verifies watch → scan → tool output without a second peer):

```bash
SCRATCH=$(mktemp -d) && git -C "$SCRATCH" init -q -b main && git -C "$SCRATCH" config user.email t@t && git -C "$SCRATCH" config user.name t && echo hi > "$SCRATCH/f.txt" && git -C "$SCRATCH" add . && git -C "$SCRATCH" commit -qm init
cargo build 2>/dev/null
# Drive the MCP server over stdio: initialize, join a room, watch the repo, edit a file, check no crash
(
  printf '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}\n'
  printf '{"jsonrpc":"2.0","method":"notifications/initialized"}\n'
  printf '{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"join_room","arguments":{"room":"t"}}}\n'
  printf '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"watch_repo","arguments":{"repo_path":"%s","room":"t"}}}\n' "$SCRATCH"
  sleep 1; echo changed >> "$SCRATCH/f.txt"; sleep 3
  printf '{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"check_file_activity","arguments":{"repo":"'$(basename "$SCRATCH")'"}}}\n'
  sleep 1
) | BUDDIES_USER=smoketest BUDDIES_SIGNER=none ./target/debug/buddies 2>/dev/null | tail -2
```

Expected: the `watch_repo` response contains `"watching":true` and the process does not crash. (`check_file_activity` returns an empty list here — a single node stores only *peers'* activity — that's correct.)

- [ ] **Step 7: Commit:**

```bash
git add src/server.rs
git commit -m "feat: add watch_repo, check_file_activity, get_peer_diff tools and conflict notifications

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 8: README + final verification

**Files:**
- Modify: `README.md`

- [ ] **Step 1: Add tools to the README tools table:**

```markdown
| **watch_repo** | Watch a local git repo and stream uncommitted file diffs to the room. |
| **unwatch_repo** | Stop watching a repository. |
| **check_file_activity** | See which peers recently changed which files in a watched repo. |
| **get_peer_diff** | Read a peer's latest diff for a specific file. |
```

- [ ] **Step 2: Add a "Repo awareness" section after "Task delegation"**, covering: `watch_repo` starts a filesystem watcher; git does the filtering (gitignore respected); diffs travel signed over gossip capped at 64 KiB; latest diff per peer per file kept for 24h; `notifications/buddies/fileConflict` is pushed when a peer touches a file you've also modified; **privacy warning paragraph**: watching a repo shares source-code diffs with everyone in the room — use `require_signed=true` and an identity whitelist for such rooms.

- [ ] **Step 3: Full verification:**

```bash
cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test
```

Expected: all green.

- [ ] **Step 4: Commit:**

```bash
git add README.md
git commit -m "docs: document repo awareness tools and privacy model

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```
